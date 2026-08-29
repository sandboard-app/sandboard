//! The human face. Thin: every handler here delegates straight to `Board`, so
//! the pixels and the agent API can't drift apart.

use crate::model::{
    AgentRuntimeConfig, CockpitSession, ItemId, McpServerDesired, OpenShellPolicy,
    OpenShellProviderDesired,
    OpenShellProviderRefreshDesired, OpenShellProviderTypeDesired, SandboxProfile,
    SandboxProfileCreateDefaults, State, WebhookPollConfig, WorkItem, WorkspaceBinding,
};
use crate::openshell::{ProviderRefreshSpec, ProviderTypeProfile};
use crate::provider_types::ProviderTypeCatalogEntry;
use crate::secrets::{open_string_map, seal_string_map};
use crate::store::{AncestryLine, SharedBoard};

use axum::extract::{Path, State as AxState};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use axum_extra::extract::cookie::CookieJar;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug)]
pub struct ApiError(String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": self.0 })),
        )
            .into_response()
    }
}

impl<E: std::fmt::Display> From<E> for ApiError {
    fn from(e: E) -> Self {
        ApiError(e.to_string())
    }
}

type ApiResult<T> = Result<Json<T>, ApiError>;

pub fn routes() -> Router<SharedBoard> {
    Router::new()
        .route("/version", get(version))
        .route("/board", get(board))
        .route("/digest", get(digest))
        .route("/webhooks/github", post(github_webhook))
        .route("/items", post(create_item))
        .route("/items/{id}", get(item_detail).delete(delete_item))
        .route("/items/{id}/delete", post(delete_item))
        .route("/items/{id}/logs", get(item_logs))
        .route("/items/{id}/transition", post(transition))
        .route("/items/{id}/update", post(update_item))
        .route("/items/{id}/steer", post(steer))
        .route("/items/{id}/plan", post(save_plan))
        .route("/items/{id}/halt", post(halt))
        .route("/items/{id}/park", post(park))
        .route("/items/{id}/unpark", post(unpark))
        .route("/items/{id}/answer", post(answer))
        .route("/items/{id}/approve", post(approve))
        .route("/items/{id}/approve-plan", post(approve_plan))
        .route("/items/{id}/init-plan", post(init_plan))
        .route("/items/{id}/request-changes", post(request_changes))
        .route("/items/{id}/cut", post(cut_scope))
        .route("/items/{id}/unarchive", post(unarchive_scope))
        .route("/items/{id}/dispatch", post(dispatch_item))
        .route("/items/{id}/auto-dispatch", post(set_auto_dispatch))
        .route(
            "/items/{id}/materialize-proposal",
            post(materialize_proposal_heal),
        )
        .route(
            "/items/{id}/sandbox-profile",
            post(set_item_sandbox_profile),
        )
        .route(
            "/sandbox-profiles",
            get(list_sandbox_profiles).post(upsert_sandbox_profile),
        )
        .route(
            "/sandbox-profiles/{id}",
            get(get_sandbox_profile).delete(delete_sandbox_profile),
        )
        .route(
            "/sandbox-profiles/{id}/default",
            post(set_default_sandbox_profile),
        )
        .route(
            "/sandbox-profiles/{id}/cockpit",
            post(set_cockpit_sandbox_profile),
        )
        .route(
            "/sandbox-profiles/cockpit/clear",
            post(clear_cockpit_sandbox_profile),
        )
        .route("/workspace", get(get_workspace).put(put_workspace))
        .route("/webhook-poll", get(get_webhook_poll).put(put_webhook_poll))
        .route(
            "/agent-runtime",
            get(get_agent_runtime).put(put_agent_runtime),
        )
        .route(
            "/cockpit-session",
            get(get_cockpit_session)
                .post(create_cockpit_session)
                .put(update_cockpit_session)
                .delete(stop_cockpit_session),
        )
        .route("/cockpit-session/park", post(park_cockpit_session))
        .route("/cockpit-session/resume", post(resume_cockpit_session))
        .route(
            "/cockpit-session/mcp-cred",
            post(provision_cockpit_mcp_cred),
        )
        // Host-mediated cockpit attach (interactive TTY) + legacy cockpit-chat bridge.
        // Board cockpit_session stays authoritative for both.
        .merge(crate::cockpit_attach::routes())
        .merge(crate::cockpit_chat::routes())
        .route("/openshell/status", get(openshell_status))
        .route("/openshell", get(get_openshell).put(put_openshell))
        .nest(
            "/openshell/oidc",
            crate::openshell_oauth::routes(),
        )
        .route(
            "/openshell/providers",
            get(list_openshell_providers).post(create_openshell_provider),
        )
        .route("/openshell/providers/sync", post(sync_openshell_providers))
        // Nested before `{name}` so `antigravity` is not captured as a provider name.
        .nest(
            "/openshell/providers/antigravity/oauth",
            crate::antigravity_oauth::api_routes(),
        )
        .route(
            "/openshell/providers/{name}",
            put(update_openshell_provider).delete(delete_openshell_provider),
        )
        .route(
            "/openshell/provider-profiles",
            get(list_openshell_provider_profiles),
        )
        .route(
            "/openshell/provider-types",
            get(list_openshell_provider_types).put(put_openshell_provider_type),
        )
        .route(
            "/openshell/provider-types/{id}",
            delete(delete_openshell_provider_type),
        )
        .route(
            "/openshell/policies",
            get(list_openshell_policies).post(upsert_openshell_policy),
        )
        .route(
            "/openshell/policies/{id}",
            get(get_openshell_policy).delete(delete_openshell_policy),
        )
        .route(
            "/openshell/mcp-servers",
            get(list_mcp_servers).post(upsert_mcp_server),
        )
        // Nested before `{id}` so `oauth` is not captured as a server id.
        .nest(
            "/openshell/mcp-servers/oauth",
            crate::mcp_client_oauth::api_routes(),
        )
        .route(
            "/openshell/mcp-servers/{id}",
            get(get_mcp_server).delete(delete_mcp_server),
        )
        .route("/github-app", get(get_github_app).put(put_github_app))
        .route("/github-app/sync-token", post(sync_github_app_token))
        .route("/github-app/repo-access", get(get_github_repo_access))
        .route(
            "/github-app/repo-access/refresh",
            post(refresh_github_repo_access),
        )
        .nest("/auth", crate::auth::api_settings_routes())
}

#[derive(Serialize)]
pub struct Version {
    version: &'static str,
}

async fn version() -> Json<Version> {
    Json(Version {
        version: env!("CARGO_PKG_VERSION"),
    })
}

async fn board(AxState(b): AxState<SharedBoard>) -> Json<crate::store::Snapshot> {
    Json(b.snapshot())
}

async fn digest(AxState(b): AxState<SharedBoard>) -> Json<crate::store::Digest> {
    Json(b.digest())
}

/// Layer 3 of the cognitive model: is this right? Transcript, diff, cost — and
/// the intent chain that says why it exists at all.
#[derive(Serialize)]
pub struct ItemDetail {
    #[serde(flatten)]
    item: WorkItem,
    ancestry: Vec<AncestryLine>,
    children: Vec<ItemId>,
    default_engine: String,
    default_model: String,
}

async fn item_detail(
    AxState(b): AxState<SharedBoard>,
    Path(id): Path<ItemId>,
) -> ApiResult<ItemDetail> {
    let mut item = b
        .get(id)
        .ok_or_else(|| ApiError(format!("no work item #{id}")))?;
    let agents = b.effective_agents();
    let default_engine = agents.engine.clone();
    // Display the resolved profile engine (not stale WorkItem.engine).
    item.engine = Some(b.resolve_engine_for_card(id));
    item.resolved_model = b.resolve_model_for_card(id);
    let default_model = item.resolved_model.clone().unwrap_or_default();
    Ok(Json(ItemDetail {
        ancestry: b.ancestry(id),
        children: b.children_of(id),
        item,
        default_engine,
        default_model,
    }))
}

#[derive(Serialize)]
pub struct LogResponse {
    /// Observed agent transcript (any engine — cursor / agy / claude).
    pub agent: Vec<String>,
    pub openshell: Vec<String>,
}

async fn item_logs(
    AxState(b): AxState<SharedBoard>,
    Path(id): Path<ItemId>,
) -> ApiResult<LogResponse> {
    let item = b
        .get(id)
        .ok_or_else(|| ApiError(format!("no work item #{id}")))?;
    let agent = b.get_agent_logs(id);

    let env_name = item.environment.clone().unwrap_or_else(|| {
        crate::schema::card_sandbox_name(id, item.run_failures + 1)
    });

    let os = b.openshell_client();
    let openshell = if let Ok(logs) = os.logs(&env_name, 60).await {
        logs.lines().map(|s| s.to_string()).collect()
    } else {
        Vec::new()
    };

    Ok(Json(LogResponse { agent, openshell }))
}

#[derive(Deserialize)]
pub struct CreateItem {
    parent: Option<ItemId>,
    title: String,
    intent: String,
    #[serde(default)]
    definition_of_done: Option<String>,
    #[serde(default)]
    capability: Option<String>,
    #[serde(default)]
    above_line: bool,
    /// Required for Projects — `owner/name` Initial plan clones for planning.
    #[serde(default)]
    clone_repo: Option<String>,
    /// Optional sibling Task ids this new Task waits on (Task create only).
    #[serde(default)]
    blocked_by: Vec<ItemId>,
    /// Accepted on Task create; clone targets are named in intent/DoD.
    #[serde(default)]
    repo: Option<crate::schema::RepoConfig>,
    /// Accepted and unused — use `clone_repo` on Project create.
    #[serde(default)]
    product_repo: Option<serde_json::Value>,
    /// Standing instructions for Projects — defaults on create if omitted.
    #[serde(default)]
    project_prompt: Option<String>,
}

async fn create_item(
    AxState(b): AxState<SharedBoard>,
    Json(req): Json<CreateItem>,
) -> ApiResult<WorkItem> {
    let _ = req.product_repo;
    let _ = req.repo; // Task clone targets live in intent/DoD
    let item = match req.parent {
        None => {
            if !req.blocked_by.is_empty() {
                return Err(ApiError(
                    "blocked_by applies to Tasks under a Project, not Project create".into(),
                ));
            }
            let clone = req.clone_repo.as_deref().ok_or_else(|| {
                ApiError("clone_repo is required for Projects (`owner/name`)".into())
            })?;
            let item = b
                .create_project(
                    req.title,
                    req.intent,
                    clone,
                    req.above_line,
                    req.project_prompt,
                )
                .map_err(ApiError)?;
            // A project dropped in plain language starts shaping immediately.
            b.transition(item.id, State::Shaping, "human", None)
                .unwrap_or(item)
        }
        Some(parent) => {
            let dod = req.definition_of_done.ok_or_else(|| {
                ApiError("definition_of_done is required so the Task can enter Backlog".into())
            })?;
            b.create_task(
                parent,
                req.title,
                req.intent,
                dod,
                req.blocked_by,
                req.capability,
                req.above_line,
            )
            .map_err(ApiError)?
        }
    };

    Ok(Json(item))
}

/// Approve Initial plan proposal (id = Project or Initial plan Task).
/// Never transitions the Project itself to Backlog.
async fn approve_plan(
    AxState(b): AxState<SharedBoard>,
    Path(id): Path<ItemId>,
) -> ApiResult<Vec<ItemId>> {
    let published = b.approve_plan(id).map_err(ApiError)?;
    Ok(Json(published))
}

#[derive(Deserialize, Default)]
pub struct InitPlanReq {
    /// Accepted; body may be empty `{}`.
    #[serde(default)]
    #[allow(dead_code)]
    repo: Option<crate::schema::RepoConfig>,
}

/// Ensure Initial plan Task exists (id = Project). Usually already auto-seeded.
async fn init_plan(
    AxState(b): AxState<SharedBoard>,
    Path(id): Path<ItemId>,
    Json(_req): Json<InitPlanReq>,
) -> ApiResult<WorkItem> {
    let seed = b.init_plan(id).map_err(ApiError)?;
    Ok(Json(seed))
}

#[derive(Deserialize)]
pub struct TransitionReq {
    to: State,
    #[serde(default)]
    reason: Option<String>,
}

async fn transition(
    AxState(b): AxState<SharedBoard>,
    Path(id): Path<ItemId>,
    Json(req): Json<TransitionReq>,
) -> ApiResult<WorkItem> {
    Ok(Json(b.transition(id, req.to, "human", req.reason)?))
}

#[derive(Deserialize)]
pub struct TextReq {
    text: String,
}

#[derive(Deserialize)]
pub struct ReasonReq {
    #[serde(default)]
    reason: Option<String>,
}

async fn steer(
    AxState(b): AxState<SharedBoard>,
    Path(id): Path<ItemId>,
    Json(req): Json<TextReq>,
) -> ApiResult<WorkItem> {
    Ok(Json(b.steer(id, req.text).map_err(ApiError)?))
}

#[derive(Deserialize)]
pub struct PlanTaskBody {
    key: String,
    title: String,
    intent: String,
    definition_of_done: String,
    #[serde(default)]
    blocked_by_keys: Vec<String>,
    #[serde(default)]
    capability: Option<String>,
    /// Accepted; clone targets are named in intent/DoD.
    #[serde(default)]
    repo: Option<crate::schema::RepoConfig>,
}

#[derive(Deserialize)]
pub struct SavePlanReq {
    #[serde(default)]
    summary: Option<String>,
    tasks: Vec<PlanTaskBody>,
    #[serde(default)]
    cancel_keys: Vec<String>,
}

/// Write / revise the proposal on the Initial plan card (id = Project or Initial plan).
/// Does not materialize Tasks — Approve does.
async fn save_plan(
    AxState(b): AxState<SharedBoard>,
    Path(id): Path<ItemId>,
    Json(req): Json<SavePlanReq>,
) -> ApiResult<crate::model::TaskProposal> {
    let summary = req
        .summary
        .unwrap_or_else(|| b.get(id).map(|i| i.intent.clone()).unwrap_or_default());
    let tasks = req
        .tasks
        .into_iter()
        .map(|t| crate::model::PlanTaskSpec {
            key: t.key,
            title: t.title,
            intent: t.intent,
            definition_of_done: t.definition_of_done,
            blocked_by_keys: t.blocked_by_keys,
            capability: t.capability,
            repo: t.repo,
            item_id: None,
        })
        .collect();
    Ok(Json(
        b.propose_plan(id, summary, tasks, req.cancel_keys)
            .map_err(ApiError)?,
    ))
}

#[derive(Deserialize)]
pub struct UpdateItemReq {
    title: Option<String>,
    intent: Option<String>,
    definition_of_done: Option<String>,
    engine: Option<String>,
    #[serde(default)]
    project_prompt: Option<String>,
    /// Accepted and unused — name clone targets in intent/DoD.
    #[serde(default)]
    repo: Option<crate::schema::RepoConfig>,
    /// Accepted and unused on update.
    #[serde(default)]
    product_repo: Option<serde_json::Value>,
}

async fn update_item(
    AxState(b): AxState<SharedBoard>,
    Path(id): Path<ItemId>,
    Json(req): Json<UpdateItemReq>,
) -> ApiResult<WorkItem> {
    let _ = req.product_repo;
    let item = b
        .update_item(
            id,
            req.title,
            req.intent,
            req.definition_of_done,
            req.engine,
            req.project_prompt,
        )
        .map_err(ApiError)?;
    let _ = req.repo;
    Ok(Json(item))
}

async fn halt(
    AxState(b): AxState<SharedBoard>,
    Path(id): Path<ItemId>,
    Json(req): Json<ReasonReq>,
) -> ApiResult<WorkItem> {
    Ok(Json(b.halt(id, req.reason).map_err(ApiError)?))
}

async fn park(
    AxState(b): AxState<SharedBoard>,
    Path(id): Path<ItemId>,
    Json(req): Json<ReasonReq>,
) -> ApiResult<WorkItem> {
    Ok(Json(b.park(id, req.reason).map_err(ApiError)?))
}

async fn unpark(AxState(b): AxState<SharedBoard>, Path(id): Path<ItemId>) -> ApiResult<WorkItem> {
    Ok(Json(b.unpark(id).map_err(ApiError)?))
}

/// Queue a Backlog card for the supervisor to claim. Explicit start — unless
/// the containing Project has auto mode on.
async fn dispatch_item(
    AxState(b): AxState<SharedBoard>,
    Path(id): Path<ItemId>,
) -> ApiResult<WorkItem> {
    Ok(Json(b.enqueue_dispatch(id).map_err(ApiError)?))
}

#[derive(Deserialize)]
pub struct AutoDispatchReq {
    enabled: bool,
}

/// Play/pause Project auto mode — continuously queue claimable Backlog leaves.
async fn set_auto_dispatch(
    AxState(b): AxState<SharedBoard>,
    Path(id): Path<ItemId>,
    Json(req): Json<AutoDispatchReq>,
) -> ApiResult<WorkItem> {
    Ok(Json(
        b.set_auto_dispatch(id, req.enabled).map_err(ApiError)?,
    ))
}

/// Heal: create sibling Tasks from a Done card's proposal (e.g. merged before
/// materialize-on-Done was wired).
async fn materialize_proposal_heal(
    AxState(b): AxState<SharedBoard>,
    Path(id): Path<ItemId>,
) -> ApiResult<Vec<ItemId>> {
    let before: std::collections::HashSet<_> = b
        .get(id)
        .and_then(|i| i.parent)
        .map(|p| b.children_of(p))
        .unwrap_or_default()
        .into_iter()
        .collect();
    let made = b.materialize_pending_proposal(id).map_err(ApiError)?;
    if let Some(parent) = b.get(id).and_then(|i| i.parent) {
        let mut new_ids = Vec::new();
        for cid in b.children_of(parent) {
            if !before.contains(&cid) {
                new_ids.push(cid);
            }
        }
    }
    Ok(Json(made.into_iter().map(|i| i.id).collect()))
}

#[derive(Deserialize)]
pub struct AnswerReq {
    choice: String,
}

async fn answer(
    AxState(b): AxState<SharedBoard>,
    Path(id): Path<ItemId>,
    Json(req): Json<AnswerReq>,
) -> ApiResult<WorkItem> {
    Ok(Json(b.answer_escalation(id, req.choice).map_err(ApiError)?))
}

async fn approve(AxState(b): AxState<SharedBoard>, Path(id): Path<ItemId>) -> ApiResult<WorkItem> {
    let before: std::collections::HashSet<_> = b
        .get(id)
        .and_then(|i| i.parent)
        .map(|p| b.children_of(p))
        .unwrap_or_default()
        .into_iter()
        .collect();
    let item = b.approve_review(id).map_err(ApiError)?;
    if let Some(parent) = item.parent {
        let mut new_ids = Vec::new();
        for cid in b.children_of(parent) {
            if !before.contains(&cid) {
                new_ids.push(cid);
            }
        }
    }
    Ok(Json(item))
}

async fn request_changes(
    AxState(b): AxState<SharedBoard>,
    Path(id): Path<ItemId>,
    Json(req): Json<TextReq>,
) -> ApiResult<WorkItem> {
    Ok(Json(b.request_changes(id, req.text).map_err(ApiError)?))
}

async fn cut_scope(
    AxState(b): AxState<SharedBoard>,
    Path(id): Path<ItemId>,
    Json(req): Json<ReasonReq>,
) -> ApiResult<Vec<ItemId>> {
    Ok(Json(b.cut_scope(id, req.reason).map_err(ApiError)?))
}

async fn unarchive_scope(
    AxState(b): AxState<SharedBoard>,
    Path(id): Path<ItemId>,
    Json(req): Json<ReasonReq>,
) -> ApiResult<Vec<ItemId>> {
    Ok(Json(b.unarchive_scope(id, req.reason).map_err(ApiError)?))
}

async fn delete_item(
    AxState(b): AxState<SharedBoard>,
    Path(id): Path<ItemId>,
) -> ApiResult<serde_json::Value> {
    b.delete_item(id).map_err(ApiError)?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

// ---------------------------------------------------------------- workspace binding

/// GET returns the durable binding, or an empty default when unbound (so
/// Settings can render editors without a separate "missing" shape).
async fn get_workspace(AxState(b): AxState<SharedBoard>) -> Json<WorkspaceBinding> {
    Json(b.workspace_binding().unwrap_or_default())
}

async fn put_workspace(
    AxState(b): AxState<SharedBoard>,
    Json(req): Json<WorkspaceBinding>,
) -> ApiResult<WorkspaceBinding> {
    b.set_workspace_binding(req).map(Json).map_err(ApiError)
}

// ---------------------------------------------------------------- webhook poll

async fn get_webhook_poll(AxState(b): AxState<SharedBoard>) -> Json<WebhookPollConfig> {
    Json(b.webhook_poll_config())
}

async fn put_webhook_poll(
    AxState(b): AxState<SharedBoard>,
    Json(req): Json<WebhookPollConfig>,
) -> Json<WebhookPollConfig> {
    Json(b.set_webhook_poll_config(req))
}

// ---------------------------------------------------------------- agent runtime

/// GET returns durable Agent runtime, seeding from compiled defaults when
/// unset so Settings always has something to edit.
async fn get_agent_runtime(AxState(b): AxState<SharedBoard>) -> Json<AgentRuntimeConfig> {
    if b.agent_runtime().is_none() {
        let _ = b.seed_agent_runtime_if_empty();
    }
    Json(
        b.agent_runtime()
            .unwrap_or_else(|| runtime_seed_fallback(&b.schema.execution.agents)),
    )
}

async fn put_agent_runtime(
    AxState(b): AxState<SharedBoard>,
    Json(req): Json<AgentRuntimeConfig>,
) -> Json<AgentRuntimeConfig> {
    Json(b.set_agent_runtime(req))
}

/// Fallback when board runtime is still unset: compiled defaults.
fn runtime_seed_fallback(_agents: &crate::schema::AgentConfig) -> AgentRuntimeConfig {
    AgentRuntimeConfig::default()
}

// ---------------------------------------------------------------- OpenShell connectivity

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct OpenShellSettings {
    /// Gateway URL (`https://…`). Not secret. Must be https.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gateway_endpoint: Option<String>,
    /// Explicit auth mode — required for a healthy client. Never inferred.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_mode: Option<crate::model::OpenShellAuthMode>,
    /// Non-secret OIDC client settings (when auth_mode is oidc).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oidc: Option<crate::model::OpenShellOidcConfig>,
    /// Write-only PEM fields — accepted on PUT, never returned on GET.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ca_pem: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_cert_pem: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_key_pem: Option<String>,
    /// When true on PUT, wipe sealed mTLS material.
    #[serde(default)]
    pub clear_mtls: bool,
    /// When true on PUT, wipe sealed OIDC tokens.
    #[serde(default)]
    pub clear_oidc: bool,
    /// Read-only presence flags (GET / after PUT).
    #[serde(default)]
    pub mtls: crate::secrets::OpenShellMtlsStatus,
    /// Read-only OIDC login presence.
    #[serde(default)]
    pub oidc_status: crate::secrets::OpenShellOidcStatus,
}

#[derive(Debug, Clone, Serialize)]
pub struct OpenShellStatusOut {
    pub healthy: bool,
    pub summary: String,
    /// True when endpoint or selected auth material is missing.
    pub not_configured: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gateway_endpoint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_mode: Option<crate::model::OpenShellAuthMode>,
    pub mtls: crate::secrets::OpenShellMtlsStatus,
    pub oidc_status: crate::secrets::OpenShellOidcStatus,
}

fn openshell_settings_view(b: &SharedBoard) -> OpenShellSettings {
    OpenShellSettings {
        gateway_endpoint: b.openshell_gateway_endpoint(),
        auth_mode: b.openshell_auth_mode(),
        oidc: b.openshell_oidc_config(),
        ca_pem: None,
        client_cert_pem: None,
        client_key_pem: None,
        clear_mtls: false,
        clear_oidc: false,
        mtls: b.openshell_mtls_status(),
        oidc_status: b.openshell_oidc_status(),
    }
}

async fn get_openshell(AxState(b): AxState<SharedBoard>) -> Json<OpenShellSettings> {
    Json(openshell_settings_view(&b))
}

async fn put_openshell(
    AxState(b): AxState<SharedBoard>,
    Json(req): Json<OpenShellSettings>,
) -> Result<Json<OpenShellSettings>, (axum::http::StatusCode, String)> {
    if let Some(ref ep) = req.gateway_endpoint {
        let ep = ep.trim();
        if !ep.is_empty() && ep.starts_with("http://") {
            return Err((
                axum::http::StatusCode::BAD_REQUEST,
                "gateway endpoint must be https:// (plaintext HTTP is not supported)".into(),
            ));
        }
    }
    let _ = b.set_openshell_gateway_endpoint(req.gateway_endpoint);

    // Auth mode is explicit. `Some` sets; omit leaves existing (incl. mTLS migration).
    if req.auth_mode.is_some() {
        b.set_openshell_auth_mode(req.auth_mode);
    }

    if let Some(oidc) = req.oidc {
        let oidc = oidc.trimmed();
        if oidc.is_complete() {
            oidc.validate().map_err(|e| (axum::http::StatusCode::BAD_REQUEST, e))?;
        }
        b.set_openshell_oidc_config(Some(oidc));
    }

    if req.clear_oidc {
        b.set_openshell_oidc_sealed(None);
    }

    if req.clear_mtls {
        b.set_openshell_mtls_sealed(None);
    } else {
        let any_pem = req.ca_pem.as_ref().is_some_and(|s| !s.trim().is_empty())
            || req
                .client_cert_pem
                .as_ref()
                .is_some_and(|s| !s.trim().is_empty())
            || req
                .client_key_pem
                .as_ref()
                .is_some_and(|s| !s.trim().is_empty());
        if any_pem {
            // Merge with existing decrypted bundle when only some fields are sent.
            let mut bundle = match b.openshell_mtls_sealed() {
                Some(s) => {
                    crate::secrets::open_mtls(&s).unwrap_or(crate::secrets::OpenShellMtlsBundle {
                        ca_pem: String::new(),
                        client_cert_pem: String::new(),
                        client_key_pem: String::new(),
                    })
                }
                None => crate::secrets::OpenShellMtlsBundle {
                    ca_pem: String::new(),
                    client_cert_pem: String::new(),
                    client_key_pem: String::new(),
                },
            };
            if let Some(pem) = req.ca_pem.filter(|s| !s.trim().is_empty()) {
                bundle.ca_pem = pem;
            }
            if let Some(pem) = req.client_cert_pem.filter(|s| !s.trim().is_empty()) {
                bundle.client_cert_pem = pem;
            }
            if let Some(pem) = req.client_key_pem.filter(|s| !s.trim().is_empty()) {
                bundle.client_key_pem = pem;
            }
            let sealed = crate::secrets::seal_mtls(&bundle).map_err(|e| {
                (
                    axum::http::StatusCode::BAD_REQUEST,
                    format!("seal mTLS: {e}"),
                )
            })?;
            b.set_openshell_mtls_sealed(Some(sealed));
        }
    }

    Ok(Json(openshell_settings_view(&b)))
}

async fn openshell_status(AxState(b): AxState<SharedBoard>) -> Json<OpenShellStatusOut> {
    let st = b.openshell_client().gateway_status().await;
    Json(OpenShellStatusOut {
        healthy: st.healthy,
        summary: st.summary,
        not_configured: st.not_configured,
        error: st.error,
        gateway_endpoint: b.openshell_gateway_endpoint(),
        auth_mode: b.openshell_auth_mode(),
        mtls: b.openshell_mtls_status(),
        oidc_status: b.openshell_oidc_status(),
    })
}

// ---------------------------------------------------------------- GitHub App credentials

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct GitHubAppSettings {
    /// Non-secret App ID — returned on GET when configured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_id: Option<String>,
    /// Non-secret OAuth Client ID — returned on GET when set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    /// Write-only — accepted on PUT, never returned on GET.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub private_key_pem: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webhook_secret: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    /// Installation that mints sandbox tokens. Cleared with `null` / omit to keep.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installation_id: Option<u64>,
    /// When true on PUT, clear `installation_id`.
    #[serde(default)]
    pub clear_installation_id: bool,
    /// When true on PUT, wipe sealed GitHub App material.
    #[serde(default)]
    pub clear: bool,
    /// Read-only presence flags (GET / after PUT).
    #[serde(default)]
    pub status: crate::secrets::GitHubAppStatus,
    /// Installations visible to the App (GET only; best-effort).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub installations: Vec<crate::github_app::InstallationInfo>,
    /// Last token sync status (GET only).
    #[serde(default)]
    pub token_status: GitHubAppTokenStatus,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct GitHubAppTokenStatus {
    pub configured: bool,
    pub provider_attached: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

async fn github_app_settings_view(b: &SharedBoard) -> GitHubAppSettings {
    let bundle = b.github_app_bundle();
    let cache = b.github_app_token_cache();
    let provider_attached = b
        .openshell_providers()
        .iter()
        .any(|p| p.name == crate::github_app::PROVIDER_NAME);
    let mut installations = Vec::new();
    if let Some(ref bundle) = bundle {
        if !bundle.app_id.trim().is_empty() && !bundle.private_key_pem.trim().is_empty() {
            if let Ok(jwt) = crate::github_app::make_app_jwt(bundle, chrono::Utc::now()) {
                if let Ok(list) = crate::github_app::list_installations(&jwt).await {
                    installations = list;
                }
            }
        }
    }
    GitHubAppSettings {
        app_id: bundle
            .as_ref()
            .map(|x| x.app_id.trim().to_string())
            .filter(|s| !s.is_empty()),
        client_id: bundle
            .as_ref()
            .map(|x| x.client_id.trim().to_string())
            .filter(|s| !s.is_empty()),
        private_key_pem: None,
        webhook_secret: None,
        client_secret: None,
        installation_id: b.github_app_installation_id(),
        clear_installation_id: false,
        clear: false,
        status: b.github_app_status(),
        installations,
        token_status: GitHubAppTokenStatus {
            configured: crate::github_app::configured_for_tokens(b),
            provider_attached,
            expires_at: cache.expires_at.map(|t| t.to_rfc3339()),
            error: cache.last_error,
        },
    }
}

async fn get_github_app(AxState(b): AxState<SharedBoard>) -> Json<GitHubAppSettings> {
    Json(github_app_settings_view(&b).await)
}

async fn put_github_app(
    AxState(b): AxState<SharedBoard>,
    Json(req): Json<GitHubAppSettings>,
) -> Result<Json<GitHubAppSettings>, (axum::http::StatusCode, String)> {
    // Compatibility shim: App material lives on the `github-app` provider row.
    if req.clear {
        b.clear_github_app();
        return Ok(Json(github_app_settings_view(&b).await));
    }

    if req.clear_installation_id {
        b.set_github_app_installation_id(None);
    } else if let Some(id) = req.installation_id {
        b.set_github_app_installation_id(Some(id));
    }

    let touching = req.app_id.as_ref().is_some_and(|s| !s.trim().is_empty())
        || req.client_id.as_ref().is_some()
        || req
            .private_key_pem
            .as_ref()
            .is_some_and(|s| !s.trim().is_empty())
        || req.webhook_secret.as_ref().is_some()
        || req.client_secret.as_ref().is_some();

    if touching {
        let mut bundle = b.github_app_bundle().unwrap_or_default();
        if let Some(id) = req.app_id.filter(|s| !s.trim().is_empty()) {
            bundle.app_id = id.trim().to_string();
        }
        // Explicit empty string clears optional client_id.
        if let Some(id) = req.client_id {
            bundle.client_id = id.trim().to_string();
        }
        if let Some(pem) = req.private_key_pem.filter(|s| !s.trim().is_empty()) {
            bundle.private_key_pem = pem;
        }
        if let Some(sec) = req.webhook_secret {
            bundle.webhook_secret = sec;
        }
        if let Some(sec) = req.client_secret {
            bundle.client_secret = sec;
        }
        b.set_github_app_bundle(&bundle).map_err(|e| {
            (
                axum::http::StatusCode::BAD_REQUEST,
                format!("seal GitHub App: {e}"),
            )
        })?;
    }

    // Persist succeeded — mint/push immediately when App + installation are ready
    // (changing installation must not wait for Mint / sync or the sweeper).
    if crate::github_app::configured_for_tokens(&b) {
        b.set_github_app_token_cache(crate::github_app::TokenCache {
            expires_at: None,
            last_error: None,
        });
        if let Err(e) = crate::github_app::ensure_github_provider(&b).await {
            // Credentials/installation are already saved; surface mint failure
            // on token_status rather than failing the PUT.
            tracing::warn!(error = %e, "GitHub App save: installation token sync failed");
        }
    }

    Ok(Json(github_app_settings_view(&b).await))
}

async fn sync_github_app_token(
    AxState(b): AxState<SharedBoard>,
) -> Result<Json<GitHubAppSettings>, (axum::http::StatusCode, String)> {
    // Force remint even if cache looks fresh.
    b.set_github_app_token_cache(crate::github_app::TokenCache {
        expires_at: None,
        last_error: None,
    });
    match crate::github_app::ensure_github_provider(&b).await {
        Ok(true) => Ok(Json(github_app_settings_view(&b).await)),
        Ok(false) => Err((
            axum::http::StatusCode::BAD_REQUEST,
            "GitHub App incomplete or installation not selected".into(),
        )),
        Err(e) => Err((
            axum::http::StatusCode::BAD_GATEWAY,
            format!("sync token: {e}"),
        )),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GitHubRepoAccessRepoView {
    pub full_name: String,
    pub installation_id: u64,
    #[serde(default)]
    pub permissions: BTreeMap<String, String>,
    pub last_seen_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GitHubRepoAccessInstallationView {
    pub id: u64,
    pub account_login: String,
    #[serde(default)]
    pub account_type: String,
    pub manage_url: String,
    pub repos: Vec<GitHubRepoAccessRepoView>,
}

/// GET `/api/github-app/repo-access` — cached installations + repos (visibility).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GitHubRepoAccessView {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refreshed_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    pub install_url: String,
    /// Singleton used by `github-app` token minting — unchanged by this cache.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_installation_id: Option<u64>,
    pub installations: Vec<GitHubRepoAccessInstallationView>,
}

fn github_repo_access_view(b: &SharedBoard) -> GitHubRepoAccessView {
    let cache = b.github_repo_access_cache();
    let mut installations: Vec<GitHubRepoAccessInstallationView> = cache
        .installations
        .iter()
        .map(|inst| GitHubRepoAccessInstallationView {
            id: inst.id,
            account_login: inst.account_login.clone(),
            account_type: inst.account_type.clone(),
            manage_url: crate::github_app::installation_manage_url(
                &inst.account_login,
                &inst.account_type,
                inst.id,
            ),
            repos: Vec::new(),
        })
        .collect();
    for (full_name, entry) in &cache.repos {
        let repo = GitHubRepoAccessRepoView {
            full_name: full_name.clone(),
            installation_id: cache
                .installation_id_for(full_name)
                .unwrap_or(entry.installation_id),
            permissions: entry.permissions.clone(),
            last_seen_at: entry.last_seen_at.to_rfc3339(),
        };
        if let Some(inst) = installations
            .iter_mut()
            .find(|i| i.id == entry.installation_id)
        {
            inst.repos.push(repo);
        } else {
            installations.push(GitHubRepoAccessInstallationView {
                id: entry.installation_id,
                account_login: String::new(),
                account_type: String::new(),
                manage_url: crate::github_app::installation_manage_url("", "", entry.installation_id),
                repos: vec![repo],
            });
        }
    }
    for inst in &mut installations {
        inst.repos.sort_by(|a, b| a.full_name.cmp(&b.full_name));
    }
    installations.sort_by(|a, b| a.account_login.cmp(&b.account_login).then(a.id.cmp(&b.id)));
    GitHubRepoAccessView {
        refreshed_at: cache.refreshed_at.map(|t| t.to_rfc3339()),
        last_error: cache.last_error,
        install_url: crate::github_app::INSTALLATIONS_MANAGE_URL.to_string(),
        token_installation_id: b.github_app_installation_id(),
        installations,
    }
}

async fn get_github_repo_access(AxState(b): AxState<SharedBoard>) -> Json<GitHubRepoAccessView> {
    Json(github_repo_access_view(&b))
}

async fn refresh_github_repo_access(
    AxState(b): AxState<SharedBoard>,
) -> Result<Json<GitHubRepoAccessView>, (axum::http::StatusCode, String)> {
    match crate::github_app::refresh_repo_access_cache(&b).await {
        Ok(_) => Ok(Json(github_repo_access_view(&b))),
        Err(e) => {
            // Cache may still hold a last_error; surface the walk failure.
            Err((
                axum::http::StatusCode::BAD_GATEWAY,
                format!("refresh repo access: {e}"),
            ))
        }
    }
}

// ---------------------------------------------------------------- OpenShell providers

/// Safe GET view — never includes credential values.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct OpenShellProviderView {
    pub name: String,
    #[serde(rename = "type")]
    pub provider_type: String,
    pub config: BTreeMap<String, String>,
    pub credential_keys: Vec<String>,
    pub has_credentials: bool,
    pub has_refresh: bool,
    /// Present when the gateway was reachable for this request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gateway_synced: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
pub struct OpenShellProvidersOut {
    pub providers: Vec<OpenShellProviderView>,
    /// True when the gateway list call succeeded (sync badges are meaningful).
    pub gateway_reachable: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OpenShellProviderWrite {
    pub name: String,
    #[serde(rename = "type")]
    pub provider_type: String,
    #[serde(default)]
    pub config: BTreeMap<String, String>,
    /// Write-only. Omit / empty on PUT to keep existing sealed credentials.
    #[serde(default)]
    pub credentials: Option<BTreeMap<String, String>>,
    /// Optional refresh bootstrap for providers that need gateway-owned refresh.
    #[serde(default)]
    pub refresh: Option<OpenShellProviderRefreshWrite>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OpenShellProviderRefreshWrite {
    pub credential_key: String,
    pub strategy: String,
    pub material: BTreeMap<String, String>,
    #[serde(default)]
    pub secret_material_keys: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SyncProvidersOut {
    pub applied: Vec<String>,
    pub errors: Vec<SyncProviderError>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SyncProviderError {
    pub name: String,
    pub error: String,
}

fn provider_view(
    p: &OpenShellProviderDesired,
    gateway_types: Option<&BTreeMap<String, String>>,
) -> OpenShellProviderView {
    OpenShellProviderView {
        name: p.name.clone(),
        provider_type: p.provider_type.clone(),
        config: p.config.clone(),
        credential_keys: p.credential_keys.clone(),
        has_credentials: p.has_credentials(),
        has_refresh: p.refresh.is_some(),
        gateway_synced: gateway_types.map(|g| {
            g.get(&p.name)
                .is_some_and(|t| t.eq_ignore_ascii_case(&p.provider_type))
        }),
    }
}

fn seal_credentials_from_write(
    credentials: Option<&BTreeMap<String, String>>,
    existing: Option<&OpenShellProviderDesired>,
) -> Result<(Option<String>, Vec<String>), (StatusCode, String)> {
    if let Some(creds) = credentials {
        // Merge: blank values mean "keep existing" (Providers form write-only fields).
        let mut merged = existing
            .and_then(|e| e.credentials_sealed.as_deref())
            .filter(|s| !s.trim().is_empty())
            .map(open_string_map)
            .transpose()
            .map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("open existing credentials: {e}"),
                )
            })?
            .unwrap_or_default();
        let mut any_new = false;
        for (k, v) in creds {
            let key = k.trim();
            if key.is_empty() || v.trim().is_empty() {
                continue;
            }
            merged.insert(key.to_string(), v.clone());
            any_new = true;
        }
        if !any_new {
            return Ok((
                existing.and_then(|e| e.credentials_sealed.clone()),
                existing
                    .map(|e| e.credential_keys.clone())
                    .unwrap_or_default(),
            ));
        }
        let keys: Vec<_> = merged.keys().cloned().collect();
        let sealed = seal_string_map(&merged).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("seal credentials: {e}"),
            )
        })?;
        return Ok((Some(sealed), keys));
    }
    Ok((
        existing.and_then(|e| e.credentials_sealed.clone()),
        existing
            .map(|e| e.credential_keys.clone())
            .unwrap_or_default(),
    ))
}

fn seal_refresh_from_write(
    refresh: Option<&OpenShellProviderRefreshWrite>,
    existing: Option<&OpenShellProviderDesired>,
) -> Result<Option<OpenShellProviderRefreshDesired>, (StatusCode, String)> {
    let Some(r) = refresh else {
        return Ok(existing.and_then(|e| e.refresh.clone()));
    };
    let material: BTreeMap<String, String> = r
        .material
        .iter()
        .map(|(k, v)| (k.trim().to_string(), v.clone()))
        .filter(|(k, v)| !k.is_empty() && !v.is_empty())
        .collect();
    if material.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "refresh.material must not be empty".into(),
        ));
    }
    let sealed = seal_string_map(&material).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("seal refresh material: {e}"),
        )
    })?;
    Ok(Some(OpenShellProviderRefreshDesired {
        credential_key: r.credential_key.trim().to_string(),
        strategy: r.strategy.trim().to_string(),
        material_sealed: sealed,
        secret_material_keys: r.secret_material_keys.clone(),
    }))
}

fn credentials_for_apply(p: &OpenShellProviderDesired) -> Result<BTreeMap<String, String>, String> {
    match p.credentials_sealed.as_deref() {
        None | Some("") => Ok(BTreeMap::new()),
        Some(s) => open_string_map(s).map_err(|e| format!("open credentials: {e}")),
    }
}

fn refresh_for_apply(p: &OpenShellProviderDesired) -> Result<Option<ProviderRefreshSpec>, String> {
    let Some(r) = &p.refresh else {
        return Ok(None);
    };
    let material =
        open_string_map(&r.material_sealed).map_err(|e| format!("open refresh material: {e}"))?;
    Ok(Some(ProviderRefreshSpec {
        credential_key: r.credential_key.clone(),
        strategy: r.strategy.clone(),
        material,
        secret_material_keys: r.secret_material_keys.clone(),
    }))
}

async fn apply_desired_to_gateway(
    b: &SharedBoard,
    p: &OpenShellProviderDesired,
) -> Result<(), String> {
    // App-minted github-app: mint (or reuse cache) then push GH_TOKEN only.
    if p.name == crate::github_app::PROVIDER_NAME
        || p.provider_type == crate::github_app::PROVIDER_TYPE
    {
        match crate::github_app::ensure_github_provider(b).await {
            Ok(true) => return Ok(()),
            Ok(false) => {
                // Incomplete App material — still try a filtered apply of whatever
                // GH_TOKEN is already sealed (no private key leakage).
            }
            Err(e) => return Err(e.to_string()),
        }
    }

    let os = b.openshell_client();
    let mut credentials = credentials_for_apply(p)?;
    let mut config = p.config.clone();
    if p.name == crate::github_app::PROVIDER_NAME
        || p.provider_type == crate::github_app::PROVIDER_TYPE
    {
        credentials = crate::github_app::gateway_credentials(&credentials);
        config = crate::github_app::gateway_config(&config);
    }
    let refresh = refresh_for_apply(p)?;
    os.apply_provider(
        &p.name,
        &p.provider_type,
        credentials,
        config,
        refresh.as_ref(),
    )
    .await
    .map(|_| ())
    .map_err(|e| e.to_string())
}

/// Reconcile provider profiles and only the instances a sandbox will attach.
///
/// Card dispatch cannot assume Settings was opened first: OpenShell resolves a
/// provider's profile at sandbox creation time, so the profile import and
/// provider instance must be current before `CreateSandbox`.
pub(crate) async fn reconcile_attached_providers(
    b: &SharedBoard,
    provider_names: &[String],
) -> Result<(), String> {
    if provider_names.is_empty() {
        return Ok(());
    }
    let os = b.openshell_client();
    crate::provider_types::import_attached_provider_types_to_gateway(b, &os, provider_names)
        .await?;
    let desired = b.openshell_providers();
    for name in provider_names {
        let provider = desired
            .iter()
            .find(|p| p.name == *name)
            .ok_or_else(|| format!("provider {name:?} is not in Board desired state"))?;
        apply_desired_to_gateway(b, provider).await?;
    }
    Ok(())
}

async fn list_openshell_providers(AxState(b): AxState<SharedBoard>) -> Json<OpenShellProvidersOut> {
    let desired = b.openshell_providers();
    let os = b.openshell_client();
    let (gateway_reachable, gateway_types) = match os.list_providers().await {
        Ok(list) => (
            true,
            Some(
                list.into_iter()
                    .map(|p| (p.name, p.provider_type))
                    .collect::<BTreeMap<_, _>>(),
            ),
        ),
        Err(_) => (false, None),
    };
    let providers = desired
        .iter()
        .map(|p| provider_view(p, gateway_types.as_ref()))
        .collect();
    Json(OpenShellProvidersOut {
        providers,
        gateway_reachable,
    })
}

async fn create_openshell_provider(
    AxState(b): AxState<SharedBoard>,
    Json(req): Json<OpenShellProviderWrite>,
) -> Result<Json<OpenShellProviderView>, ApiError> {
    let name = req.name.trim().to_string();
    if name.is_empty() {
        return Err(ApiError("name is required".into()));
    }
    if req.provider_type.trim().is_empty() {
        return Err(ApiError("type is required".into()));
    }
    let existing = b.openshell_providers().into_iter().find(|p| p.name == name);
    let (credentials_sealed, credential_keys) =
        seal_credentials_from_write(req.credentials.as_ref(), existing.as_ref())
            .map_err(|(_, m)| ApiError(m))?;
    let refresh = seal_refresh_from_write(req.refresh.as_ref(), existing.as_ref())
        .map_err(|(_, m)| ApiError(m))?;
    let desired = OpenShellProviderDesired {
        name,
        provider_type: req.provider_type.trim().to_string(),
        config: req.config,
        credentials_sealed,
        credential_keys,
        refresh,
    }
    .normalized();
    let stored = b.upsert_openshell_provider(desired);
    if stored.name == crate::github_app::PROVIDER_NAME
        || stored.provider_type == crate::github_app::PROVIDER_TYPE
    {
        b.set_github_app_token_cache(crate::github_app::TokenCache::default());
    }
    // Best-effort apply; desired state is already persisted.
    let apply_err = match crate::provider_types::import_board_types_to_gateway(
        &b,
        &b.openshell_client(),
    )
    .await
    {
        Ok(()) => apply_desired_to_gateway(&b, &stored).await.err(),
        Err(e) => Some(e),
    };
    let gateway_synced = match &apply_err {
        None if b.openshell_client().gateway_status().await.healthy => Some(true),
        None => None,
        Some(_) => Some(false),
    };
    let mut view = provider_view(&stored, None);
    view.gateway_synced = gateway_synced;
    if let Some(err) = apply_err {
        tracing::warn!(provider = %stored.name, error = %err, "provider saved locally; gateway apply failed");
    }
    Ok(Json(view))
}

async fn update_openshell_provider(
    AxState(b): AxState<SharedBoard>,
    Path(name): Path<String>,
    Json(req): Json<OpenShellProviderWrite>,
) -> Result<Json<OpenShellProviderView>, ApiError> {
    let name = name.trim().to_string();
    let existing = b
        .openshell_providers()
        .into_iter()
        .find(|p| p.name == name)
        .ok_or_else(|| ApiError(format!("no provider {name:?}")))?;
    // Name in path is authoritative; body name may rename.
    let new_name = {
        let n = req.name.trim();
        if n.is_empty() {
            name.clone()
        } else {
            n.to_string()
        }
    };
    if new_name != name {
        let _ = b.delete_openshell_provider(&name);
        let _ = b.openshell_client().delete_provider(&name).await;
    }
    let (credentials_sealed, credential_keys) =
        seal_credentials_from_write(req.credentials.as_ref(), Some(&existing))
            .map_err(|(_, m)| ApiError(m))?;
    let refresh = seal_refresh_from_write(req.refresh.as_ref(), Some(&existing))
        .map_err(|(_, m)| ApiError(m))?;
    let desired = OpenShellProviderDesired {
        name: new_name,
        provider_type: {
            let t = req.provider_type.trim();
            if t.is_empty() {
                existing.provider_type.clone()
            } else {
                t.to_string()
            }
        },
        config: if req.config.is_empty() {
            existing.config.clone()
        } else {
            req.config
        },
        credentials_sealed,
        credential_keys,
        refresh,
    }
    .normalized();
    let stored = b.upsert_openshell_provider(desired);
    if stored.name == crate::github_app::PROVIDER_NAME
        || stored.provider_type == crate::github_app::PROVIDER_TYPE
    {
        b.set_github_app_token_cache(crate::github_app::TokenCache::default());
    }
    let apply_err = match crate::provider_types::import_board_types_to_gateway(
        &b,
        &b.openshell_client(),
    )
    .await
    {
        Ok(()) => apply_desired_to_gateway(&b, &stored).await.err(),
        Err(e) => Some(e),
    };
    let mut view = provider_view(&stored, None);
    view.gateway_synced = Some(apply_err.is_none());
    Ok(Json(view))
}

async fn delete_openshell_provider(
    AxState(b): AxState<SharedBoard>,
    Path(name): Path<String>,
) -> Result<StatusCode, ApiError> {
    let name = name.trim().to_string();
    if !b.delete_openshell_provider(&name) {
        return Err(ApiError(format!("no provider {name:?}")));
    }
    let _ = b.openshell_client().delete_provider(&name).await;
    Ok(StatusCode::NO_CONTENT)
}

async fn sync_openshell_providers(AxState(b): AxState<SharedBoard>) -> Json<SyncProvidersOut> {
    let os = b.openshell_client();
    let mut applied = Vec::new();
    let mut errors = Vec::new();
    // Board custom types must exist on the gateway before CreateProvider.
    // Import-or-skip only; updating changed YAML may need a gateway update path later.
    if let Err(e) = crate::provider_types::import_board_types_to_gateway(&b, &os).await {
        tracing::warn!(error = %e, "board provider type import failed");
        errors.push(SyncProviderError {
            name: "provider-types".into(),
            error: e,
        });
    }
    let desired = b.openshell_providers();
    for p in desired {
        match apply_desired_to_gateway(&b, &p).await {
            Ok(()) => applied.push(p.name),
            Err(e) => errors.push(SyncProviderError {
                name: p.name,
                error: e,
            }),
        }
    }

    if let Err(e) = crate::antigravity::attach_to_running_cockpit(&b).await {
        tracing::warn!(error = %e, "antigravity cockpit attach after sync failed");
        errors.push(SyncProviderError {
            name: crate::model::ANTIGRAVITY_PROVIDER.into(),
            error: format!("cockpit attach: {e}"),
        });
    }

    Json(SyncProvidersOut { applied, errors })
}

async fn list_openshell_provider_profiles(
    AxState(b): AxState<SharedBoard>,
) -> Result<Json<Vec<ProviderTypeProfile>>, ApiError> {
    let os = b.openshell_client();
    let st = os.gateway_status().await;
    if st.not_configured {
        return Err(ApiError(st.summary));
    }
    os.list_provider_profiles()
        .await
        .map(Json)
        .map_err(|e| ApiError(e.to_string()))
}

#[derive(Deserialize)]
pub struct ProviderTypeWrite {
    pub id: String,
    pub yaml: String,
    #[serde(default)]
    pub form_config_keys: Option<Vec<String>>,
}

async fn list_openshell_provider_types(
    AxState(b): AxState<SharedBoard>,
) -> Json<Vec<ProviderTypeCatalogEntry>> {
    let board_types = b.openshell_provider_types();
    let gateway: Vec<ProviderTypeProfile> = b
        .openshell_client()
        .list_provider_profiles()
        .await
        .unwrap_or_default();
    Json(crate::provider_types::merge_catalog(&board_types, &gateway))
}

async fn put_openshell_provider_type(
    AxState(b): AxState<SharedBoard>,
    Json(req): Json<ProviderTypeWrite>,
) -> Result<Json<OpenShellProviderTypeDesired>, ApiError> {
    let id = req.id.trim().to_string();
    if id.is_empty() {
        return Err(ApiError("provider type id required".into()));
    }
    let _meta = crate::provider_types::parse_provider_type_yaml(&req.yaml, Some(&id))?;
    let existing = b.openshell_provider_types().get(&id).cloned();
    let form_config_keys = match req.form_config_keys {
        Some(keys) => keys,
        None => existing
            .as_ref()
            .map(|e| e.form_config_keys.clone())
            .unwrap_or_default(),
    };
    let shipped = existing.as_ref().map(|e| e.shipped).unwrap_or(false);
    let stored = b
        .upsert_openshell_provider_type(OpenShellProviderTypeDesired {
            id,
            yaml: req.yaml,
            shipped,
            form_config_keys,
        })
        .map_err(ApiError)?;
    Ok(Json(stored))
}

async fn delete_openshell_provider_type(
    AxState(b): AxState<SharedBoard>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let id = id.trim().to_string();
    if !b.delete_openshell_provider_type(&id) {
        return Err(ApiError(format!("no provider type {id:?}")));
    }
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------- sandbox profiles

#[derive(Serialize)]
pub struct SandboxProfilesOut {
    pub profiles: Vec<SandboxProfile>,
    pub default_sandbox_profile_id: Option<String>,
    pub cockpit_sandbox_profile_id: Option<String>,
    /// Prefill for Settings → Create (minimal policy, no sandboard-specific egress).
    pub create_defaults: SandboxProfileCreateDefaults,
}

fn sandbox_profiles_out(b: &crate::store::SharedBoard) -> SandboxProfilesOut {
    b.ensure_minimal_policy();
    SandboxProfilesOut {
        profiles: b.list_sandbox_profiles(),
        default_sandbox_profile_id: b.default_sandbox_profile_id(),
        cockpit_sandbox_profile_id: b.cockpit_sandbox_profile_id(),
        create_defaults: crate::model::sandbox_profile_create_defaults(),
    }
}

async fn list_sandbox_profiles(AxState(b): AxState<SharedBoard>) -> Json<SandboxProfilesOut> {
    Json(sandbox_profiles_out(&b))
}

async fn get_sandbox_profile(
    AxState(b): AxState<SharedBoard>,
    Path(id): Path<String>,
) -> ApiResult<SandboxProfile> {
    b.get_sandbox_profile(&id)
        .map(Json)
        .ok_or_else(|| ApiError(format!("no sandbox profile `{id}`")))
}

#[derive(Deserialize, Default)]
pub struct UpsertSandboxProfileReq {
    /// Omit or leave empty on create — board derives a slug from `name`.
    /// Pass the existing id when editing.
    #[serde(default)]
    pub id: Option<String>,
    pub name: String,
    pub image: String,
    /// Policies catalog id (required).
    pub policy_id: String,
    #[serde(default)]
    pub cpu: Option<String>,
    #[serde(default)]
    pub memory: Option<String>,
    #[serde(default)]
    pub engine: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub provider_names: Vec<String>,
    #[serde(default)]
    pub mcp_server_ids: Vec<String>,
    #[serde(default)]
    pub env: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    pub prompt: Option<String>,
}

async fn upsert_sandbox_profile(
    AxState(b): AxState<SharedBoard>,
    Json(req): Json<UpsertSandboxProfileReq>,
) -> ApiResult<SandboxProfile> {
    Ok(Json(
        b.upsert_sandbox_profile(SandboxProfile {
            id: req.id.unwrap_or_default(),
            name: req.name,
            image: req.image,
            policy_id: req.policy_id,
            policy_inline_legacy: None,
            cpu: req.cpu,
            memory: req.memory,
            engine: req.engine,
            model: req.model,
            provider_names: req.provider_names,
            mcp_server_ids: req.mcp_server_ids,
            env: req.env,
            prompt: req.prompt,
            shipped: false,
        })
        .map_err(ApiError)?,
    ))
}

async fn delete_sandbox_profile(
    AxState(b): AxState<SharedBoard>,
    Path(id): Path<String>,
) -> ApiResult<serde_json::Value> {
    b.delete_sandbox_profile(&id).map_err(ApiError)?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

async fn set_default_sandbox_profile(
    AxState(b): AxState<SharedBoard>,
    Path(id): Path<String>,
) -> ApiResult<SandboxProfilesOut> {
    b.set_default_sandbox_profile(&id).map_err(ApiError)?;
    Ok(Json(sandbox_profiles_out(&b)))
}

async fn set_cockpit_sandbox_profile(
    AxState(b): AxState<SharedBoard>,
    Path(id): Path<String>,
) -> ApiResult<SandboxProfilesOut> {
    b.set_cockpit_sandbox_profile(&id).map_err(ApiError)?;
    Ok(Json(sandbox_profiles_out(&b)))
}

async fn clear_cockpit_sandbox_profile(
    AxState(b): AxState<SharedBoard>,
) -> ApiResult<SandboxProfilesOut> {
    b.clear_cockpit_sandbox_profile();
    Ok(Json(sandbox_profiles_out(&b)))
}

#[derive(Serialize)]
pub struct OpenShellPoliciesOut {
    pub policies: Vec<OpenShellPolicy>,
    /// Prefill id for create forms (seeded minimal policy).
    pub create_default_policy_id: String,
}

fn openshell_policies_out(b: &crate::store::SharedBoard) -> OpenShellPoliciesOut {
    b.ensure_minimal_policy();
    OpenShellPoliciesOut {
        policies: b.list_openshell_policies(),
        create_default_policy_id: crate::seed_policies::MINIMAL_POLICY_ID.to_string(),
    }
}

async fn list_openshell_policies(AxState(b): AxState<SharedBoard>) -> Json<OpenShellPoliciesOut> {
    Json(openshell_policies_out(&b))
}

async fn get_openshell_policy(
    AxState(b): AxState<SharedBoard>,
    Path(id): Path<String>,
) -> ApiResult<OpenShellPolicy> {
    b.get_openshell_policy(&id)
        .map(Json)
        .ok_or_else(|| ApiError(format!("no policy `{id}`")))
}

#[derive(Deserialize, Default)]
pub struct UpsertOpenShellPolicyReq {
    /// Omit or leave empty on create — board derives a slug from `name`.
    #[serde(default)]
    pub id: Option<String>,
    pub name: String,
    pub yaml: String,
}

async fn upsert_openshell_policy(
    AxState(b): AxState<SharedBoard>,
    Json(req): Json<UpsertOpenShellPolicyReq>,
) -> ApiResult<OpenShellPolicy> {
    Ok(Json(
        b.upsert_openshell_policy(OpenShellPolicy {
            id: req.id.unwrap_or_default(),
            name: req.name,
            yaml: req.yaml,
        })
        .map_err(ApiError)?,
    ))
}

async fn delete_openshell_policy(
    AxState(b): AxState<SharedBoard>,
    Path(id): Path<String>,
) -> ApiResult<serde_json::Value> {
    b.delete_openshell_policy(&id).map_err(ApiError)?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

#[derive(Serialize)]
pub struct McpServersOut {
    pub servers: Vec<McpServerDesired>,
}

async fn list_mcp_servers(AxState(b): AxState<SharedBoard>) -> Json<McpServersOut> {
    Json(McpServersOut {
        servers: b.list_mcp_servers(),
    })
}

async fn get_mcp_server(
    AxState(b): AxState<SharedBoard>,
    Path(id): Path<String>,
) -> ApiResult<McpServerDesired> {
    b.get_mcp_server(&id)
        .map(Json)
        .ok_or_else(|| ApiError(format!("no mcp server `{id}`")))
}

#[derive(Deserialize)]
pub struct UpsertMcpServerReq {
    #[serde(default)]
    pub id: Option<String>,
    pub name: String,
    pub transport: crate::model::McpTransport,
    #[serde(default)]
    pub policy_fragment_yaml: Option<String>,
    #[serde(default)]
    pub provider_names: Vec<String>,
    #[serde(default)]
    pub env: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    pub audience: crate::model::McpAudience,
}

async fn upsert_mcp_server(
    AxState(b): AxState<SharedBoard>,
    Json(req): Json<UpsertMcpServerReq>,
) -> ApiResult<McpServerDesired> {
    let existing_shipped = req
        .id
        .as_deref()
        .and_then(|id| b.get_mcp_server(id))
        .map(|s| s.shipped)
        .unwrap_or(false);
    Ok(Json(
        b.upsert_mcp_server(McpServerDesired {
            id: req.id.unwrap_or_default(),
            name: req.name,
            transport: req.transport,
            policy_fragment_yaml: req.policy_fragment_yaml,
            provider_names: req.provider_names,
            env: req.env,
            audience: req.audience,
            shipped: existing_shipped,
        })
        .map_err(ApiError)?,
    ))
}

async fn delete_mcp_server(
    AxState(b): AxState<SharedBoard>,
    Path(id): Path<String>,
) -> ApiResult<serde_json::Value> {
    b.delete_mcp_server(&id).map_err(ApiError)?;
    crate::mcp_client_oauth::cleanup_for_deleted_server(&b, &id).await;
    Ok(Json(serde_json::json!({ "ok": true })))
}

#[derive(Deserialize)]
pub struct SetProjectSandboxProfileReq {
    /// `null` / omitted / empty clears the override (inherit global default).
    #[serde(default)]
    pub sandbox_profile_id: Option<String>,
}

async fn set_item_sandbox_profile(
    AxState(b): AxState<SharedBoard>,
    Path(id): Path<ItemId>,
    Json(req): Json<SetProjectSandboxProfileReq>,
) -> ApiResult<WorkItem> {
    Ok(Json(
        b.set_project_sandbox_profile(id, req.sandbox_profile_id)
            .map_err(ApiError)?,
    ))
}

// ---------------------------------------------------------------- cockpit session
// Thin face: every rule lives on Board / machine.rs.

#[derive(Serialize)]
pub struct CockpitSessionOut {
    pub session: Option<CockpitSession>,
}

#[derive(Deserialize)]
pub struct CreateCockpitSessionReq {
    #[serde(default)]
    pub environment: Option<String>,
    #[serde(default)]
    pub conversation_id: Option<String>,
}

#[derive(Deserialize)]
pub struct UpdateCockpitSessionReq {
    /// When present, sets (blank clears). Omitted leaves unchanged.
    #[serde(default)]
    pub environment: Option<String>,
    #[serde(default)]
    pub conversation_id: Option<String>,
}

async fn get_cockpit_session(AxState(b): AxState<SharedBoard>) -> Json<CockpitSessionOut> {
    Json(CockpitSessionOut {
        session: b.cockpit_session(),
    })
}

async fn create_cockpit_session(
    AxState(b): AxState<SharedBoard>,
    jar: CookieJar,
    Json(req): Json<CreateCockpitSessionReq>,
) -> ApiResult<CockpitSession> {
    let session = b
        .create_cockpit_session(req.environment, req.conversation_id)
        .map_err(ApiError)?;
    // Best-effort MCP inject when the environment is already known (rare on
    // create — supervisor usually fills it). Cockpit also calls mcp-cred.
    if let Some(env) = session
        .environment
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let sub = crate::auth::session_user_from_jar(&b, &jar)
            .map(|u| u.login)
            .unwrap_or_else(|| crate::cockpit_mcp::COCKPIT_FALLBACK_SUB.to_string());
        let os = b.openshell_client();
        if let Err(e) = crate::cockpit_mcp::provision_cockpit_mcp(&b, &os, env, &sub).await {
            tracing::warn!("cockpit MCP provision on start: {e}");
        }
    }
    Ok(Json(session))
}

#[derive(Debug, Serialize)]
pub struct OpsMcpCredOut {
    pub ok: bool,
    pub environment: String,
    pub resource: String,
    pub client_id: String,
    pub sub: String,
    pub expires_at: u64,
    /// True when files were written into the sandbox.
    pub injected: bool,
}

/// Mint `sandboard-cockpit` tokens for the logged-in user and inject MCP config into
/// the Board-named cockpit sandbox. Does not return refresh/access to the browser.
async fn provision_cockpit_mcp_cred(
    AxState(b): AxState<SharedBoard>,
    jar: CookieJar,
) -> Result<Json<OpsMcpCredOut>, ApiError> {
    let user = crate::auth::session_user_from_jar(&b, &jar)
        .ok_or_else(|| ApiError("authentication required".into()))?;
    let session = b
        .cockpit_session()
        .ok_or_else(|| ApiError("no cockpit session".into()))?;
    if session.status != crate::model::CockpitSessionStatus::Running {
        return Err(ApiError(
            "cockpit session is not Running — Start first".into(),
        ));
    }
    let env = session
        .environment
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ApiError("cockpit session has no environment yet".into()))?
        .to_string();

    let os = b.openshell_client();
    crate::cockpit_mcp_tunnel::ensure_cockpit_mcp_tunnel(&os, &b, &env)
        .await
        .map_err(ApiError)?;
    let tokens = crate::cockpit_mcp::provision_cockpit_mcp(&b, &os, &env, &user.login)
        .await
        .map_err(|e| ApiError(e.to_string()))?;

    Ok(Json(OpsMcpCredOut {
        ok: true,
        environment: env,
        resource: tokens.resource,
        client_id: tokens.client_id,
        sub: tokens.sub,
        expires_at: tokens.expires_at,
        injected: true,
    }))
}

async fn update_cockpit_session(
    AxState(b): AxState<SharedBoard>,
    Json(req): Json<UpdateCockpitSessionReq>,
) -> ApiResult<CockpitSession> {
    Ok(Json(
        b.update_cockpit_session(req.environment, req.conversation_id)
            .map_err(ApiError)?,
    ))
}

async fn park_cockpit_session(AxState(b): AxState<SharedBoard>) -> ApiResult<CockpitSession> {
    Ok(Json(b.park_cockpit_session().map_err(ApiError)?))
}

async fn resume_cockpit_session(AxState(b): AxState<SharedBoard>) -> ApiResult<CockpitSession> {
    Ok(Json(b.resume_cockpit_session().map_err(ApiError)?))
}

async fn stop_cockpit_session(AxState(b): AxState<SharedBoard>) -> Result<StatusCode, ApiError> {
    if let Some(env) = b
        .cockpit_session()
        .and_then(|s| s.environment)
        .filter(|e| !e.trim().is_empty())
    {
        let os = b.openshell_client();
        if let Err(e) = crate::cockpit_mcp::clear_cockpit_mcp(&os, env.trim()).await {
            tracing::debug!("cockpit MCP clear on stop: {e}");
        }
    }
    b.stop_cockpit_session().map_err(ApiError)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
pub struct GithubWebhookPayload {
    pub r#ref: Option<String>,
    pub after: Option<String>,
    #[serde(default)]
    pub head_commit: Option<GithubCommit>,

    pub action: Option<String>,
    #[serde(default)]
    pub pull_request: Option<GithubPullRequest>,
    /// Present on `pull_request_review` events. Only `state` is read — review
    /// bodies are never forwarded into Board notes (Board writes the pointer steer).
    #[serde(default)]
    pub review: Option<GithubReview>,

    #[serde(default)]
    pub repository: Option<GithubRepository>,
}

#[derive(Debug, Deserialize)]
pub struct GithubReview {
    /// GitHub review state (`approved`, `changes_requested`, `commented`, …).
    pub state: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct GithubCommit {
    pub id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct GithubPullRequest {
    pub merged: Option<bool>,
    pub merge_commit_sha: Option<String>,
    pub html_url: Option<String>,
    pub number: Option<u64>,
    pub base: Option<GithubBranchRef>,
}

#[derive(Debug, Deserialize)]
pub struct GithubBranchRef {
    pub r#ref: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct GithubRepository {
    pub default_branch: Option<String>,
    pub full_name: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct WebhookResponse {
    pub status: String,
    pub main_advanced: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ref_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit_sha: Option<String>,
    /// Board cards moved to Done because their `pr_url` matched a merged PR.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub completed_item_ids: Vec<u64>,
    /// Board cards steered to Backlog by submitted PR review feedback.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub steered_item_ids: Vec<u64>,
}

fn resolve_pr_url(payload: &GithubWebhookPayload) -> Option<String> {
    let pr = payload.pull_request.as_ref()?;
    if let Some(url) = pr
        .html_url
        .as_ref()
        .map(|u| u.trim())
        .filter(|u| !u.is_empty())
    {
        return Some(url.to_string());
    }
    let number = pr.number?;
    let full_name = payload
        .repository
        .as_ref()
        .and_then(|r| r.full_name.as_deref())
        .filter(|s| !s.is_empty())?;
    Some(format!("https://github.com/{full_name}/pull/{number}"))
}

fn resolve_merged_pr_url(payload: &GithubWebhookPayload) -> Option<String> {
    resolve_pr_url(payload)
}

async fn github_webhook(
    AxState(b): AxState<SharedBoard>,
    headers: HeaderMap,
    Json(payload): Json<GithubWebhookPayload>,
) -> ApiResult<WebhookResponse> {
    let event_type = headers
        .get("x-github-event")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");

    if event_type == "ping" {
        return Ok(Json(WebhookResponse {
            status: "pong".into(),
            main_advanced: false,
            ref_name: None,
            commit_sha: None,
            completed_item_ids: Vec::new(),
            steered_item_ids: Vec::new(),
        }));
    }

    // Transport only: parse review + PR identity, invoke Board. Board owns
    // steer note / Backlog / APPROVED no-op. Never forward review bodies.
    if event_type == "pull_request_review" {
        let mut steered_item_ids = Vec::new();
        if payload.action.as_deref() == Some("submitted") {
            if let (Some(state), Some(pr_url)) = (
                payload
                    .review
                    .as_ref()
                    .and_then(|r| r.state.as_deref())
                    .map(str::trim)
                    .filter(|s| !s.is_empty()),
                resolve_pr_url(&payload),
            ) {
                let number = payload.pull_request.as_ref().and_then(|pr| pr.number);
                if let Some(id) = b.apply_pr_review_feedback(&pr_url, number, state) {
                    steered_item_ids.push(id);
                }
            }
        }
        return Ok(Json(WebhookResponse {
            status: if steered_item_ids.is_empty() {
                "ignored".into()
            } else {
                "ok".into()
            },
            main_advanced: false,
            ref_name: None,
            commit_sha: None,
            completed_item_ids: Vec::new(),
            steered_item_ids,
        }));
    }

    let default_branch = payload
        .repository
        .as_ref()
        .and_then(|r| r.default_branch.as_deref())
        .unwrap_or("main");

    let is_main_ref = |r: &str| -> bool {
        r == default_branch
            || r == "main"
            || r == format!("refs/heads/{default_branch}")
            || r == "refs/heads/main"
            || r.ends_with(&format!("/{default_branch}"))
            || r.ends_with("/main")
    };

    let is_push_main = if let Some(ref_str) = &payload.r#ref {
        is_main_ref(ref_str)
    } else {
        false
    };

    let is_pr_main_merge = if let Some(pr) = &payload.pull_request {
        let is_closed_or_merged =
            payload.action.as_deref() == Some("closed") || pr.merged == Some(true);
        let merged = pr.merged == Some(true);
        let base_is_main = pr
            .base
            .as_ref()
            .and_then(|b| b.r#ref.as_deref())
            .is_some_and(is_main_ref);
        is_closed_or_merged && merged && base_is_main
    } else {
        false
    };

    let mut completed_item_ids = Vec::new();
    if is_pr_main_merge {
        if let Some(pr_url) = resolve_merged_pr_url(&payload) {
            let number = payload.pull_request.as_ref().and_then(|pr| pr.number);
            if let Some(id) = b.complete_for_merged_pr(&pr_url, number) {
                completed_item_ids.push(id);
            }
        }
    }

    if is_push_main || is_pr_main_merge {
        let ref_name = payload
            .r#ref
            .clone()
            .or_else(|| {
                payload
                    .pull_request
                    .as_ref()
                    .and_then(|pr| pr.base.as_ref())
                    .and_then(|b| b.r#ref.clone())
            })
            .unwrap_or_else(|| format!("refs/heads/{default_branch}"));

        let commit_sha = if is_push_main {
            payload
                .after
                .clone()
                .filter(|s| s != "0000000000000000000000000000000000000000")
                .or_else(|| payload.head_commit.as_ref().and_then(|c| c.id.clone()))
        } else {
            payload
                .pull_request
                .as_ref()
                .and_then(|pr| pr.merge_commit_sha.clone())
        };

        let advanced_repo = payload
            .repository
            .as_ref()
            .and_then(|r| r.full_name.as_deref())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("");
        let mut steered_item_ids: Vec<u64> = b
            .notify_main_advanced(advanced_repo, &ref_name, commit_sha.clone())
            .into_iter()
            .collect();
        steered_item_ids.extend(
            crate::supervisor::process_main_advanced_review_catch_up(&b, advanced_repo).await,
        );
        steered_item_ids.sort_unstable();
        steered_item_ids.dedup();

        Ok(Json(WebhookResponse {
            status: "ok".into(),
            main_advanced: true,
            ref_name: Some(ref_name),
            commit_sha,
            completed_item_ids,
            steered_item_ids,
        }))
    } else {
        Ok(Json(WebhookResponse {
            status: "ignored".into(),
            main_advanced: false,
            ref_name: None,
            commit_sha: None,
            completed_item_ids,
            steered_item_ids: Vec::new(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_view_is_not_synced_when_gateway_type_is_generic() {
        let provider = OpenShellProviderDesired {
            name: "sandboard-openrouter".into(),
            provider_type: "openrouter".into(),
            config: BTreeMap::new(),
            credentials_sealed: Some("sealed".into()),
            credential_keys: vec!["OPENROUTER_API_KEY".into()],
            refresh: None,
        };
        let gateway_types = BTreeMap::from([(
            "sandboard-openrouter".into(),
            "generic".into(),
        )]);

        let view = provider_view(&provider, Some(&gateway_types));
        assert_eq!(view.gateway_synced, Some(false));
    }

    #[tokio::test]
    async fn item_detail_and_snapshot_show_task_repo_when_set() {
        let path = std::env::temp_dir().join(format!(
            "sandboard-test-api-task-repo-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let b: SharedBoard = std::sync::Arc::new(crate::store::Board::new(
            crate::schema::Schema::default(),
            path,
        ));
        let project = b
            .create(
                None,
                "API Proj",
                "why",
                None,
                crate::model::Origin::Human,
                true,
                None,
            )
            .expect("project");
        let task = b
            .create(
                Some(project.id),
                "API Task",
                "do",
                Some("shipped".into()),
                crate::model::Origin::Human,
                false,
                None,
            )
            .expect("task");
        b.set_task_repo(
            task.id,
            Some(crate::schema::RepoConfig {
                upstream: "acme/api".into(),
                fork: String::new(),
                base: "main".into(),
            }),
        )
        .expect("bind");

        let Ok(Json(detail)) = item_detail(AxState(b.clone()), Path(task.id)).await else {
            panic!("item_detail");
        };
        let detail_v = serde_json::to_value(&detail).expect("detail json");
        assert_eq!(detail_v["repo"]["upstream"], "acme/api");
        assert_eq!(detail_v["repo"]["base"], "main");

        let Ok(Json(proj_detail)) = item_detail(AxState(b.clone()), Path(project.id)).await else {
            panic!("project detail");
        };
        let proj_v = serde_json::to_value(&proj_detail).expect("proj json");
        assert!(
            proj_v.get("repo").is_none() || proj_v["repo"].is_null(),
            "Project detail JSON should omit unused product_repo: {proj_v}"
        );

        // create Project requires clone_repo; product_repo / task repo body ignored.
        let Ok(Json(created)) = create_item(
            AxState(b.clone()),
            Json(CreateItem {
                parent: None,
                title: "Accidental".into(),
                intent: "why".into(),
                definition_of_done: None,
                capability: None,
                above_line: true,
                clone_repo: Some("acme/widgets".into()),
                blocked_by: vec![],
                repo: Some(crate::schema::RepoConfig {
                    upstream: "should/ignore".into(),
                    fork: String::new(),
                    base: "main".into(),
                }),
                product_repo: Some(serde_json::json!({"upstream": "also/ignore"})),
                project_prompt: None,
            }),
        )
        .await
        else {
            panic!("create project");
        };
        assert!(created.is_project());
        assert!(created.repo.is_none());
        assert!(
            created.intent.contains("Clone repository: acme/widgets"),
            "Project intent must stamp clone_repo: {}",
            created.intent
        );

        let Ok(Json(custom_prompt)) = create_item(
            AxState(b.clone()),
            Json(CreateItem {
                parent: None,
                title: "Custom prompt".into(),
                intent: "why".into(),
                definition_of_done: None,
                capability: None,
                above_line: true,
                clone_repo: Some("acme/widgets".into()),
                blocked_by: vec![],
                repo: None,
                product_repo: None,
                project_prompt: Some("Always run make test.".into()),
            }),
        )
        .await
        else {
            panic!("create project with project_prompt");
        };
        assert!(custom_prompt.is_project());
        let prompt = custom_prompt.project_prompt.as_deref().unwrap_or("");
        assert!(
            prompt.contains("Always run make test."),
            "REST create must seed custom project_prompt: {prompt}"
        );
        assert!(
            custom_prompt
                .intent
                .contains("Clone repository: acme/widgets"),
            "clone_repo must stamp into Project intent: {}",
            custom_prompt.intent
        );

        let Err(ApiError(missing)) = create_item(
            AxState(b.clone()),
            Json(CreateItem {
                parent: None,
                title: "Missing clone".into(),
                intent: "why".into(),
                definition_of_done: None,
                capability: None,
                above_line: true,
                clone_repo: None,
                blocked_by: vec![],
                repo: None,
                product_repo: None,
                project_prompt: None,
            }),
        )
        .await
        else {
            panic!("Project create without clone_repo must fail");
        };
        assert!(
            missing.contains("clone_repo"),
            "expected clone_repo error, got {missing}"
        );

        // update Project with product_repo / repo — both ignored.
        let Ok(Json(updated)) = update_item(
            AxState(b.clone()),
            Path(project.id),
            Json(UpdateItemReq {
                title: None,
                intent: None,
                definition_of_done: None,
                engine: None,
                project_prompt: None,
                repo: Some(crate::schema::RepoConfig {
                    upstream: "acme/nope".into(),
                    fork: String::new(),
                    base: "main".into(),
                }),
                product_repo: Some(serde_json::json!("acme/nope")),
            }),
        )
        .await
        else {
            panic!("update ignore product_repo");
        };
        assert!(updated.repo.is_none());

        // REST init_plan is idempotent; create_project already auto-seeded.
        let Ok(Json(seed)) = init_plan(
            AxState(b.clone()),
            Path(created.id),
            Json(InitPlanReq { repo: None }),
        )
        .await
        else {
            panic!("init_plan");
        };
        assert!(seed.is_initial_plan_task());
        assert!(
            seed.repo.is_none(),
            "Initial plan carries clone targets in prose"
        );
        assert!(b.children_of(created.id).contains(&seed.id));
    }

    #[tokio::test]
    async fn unarchive_scope_restores_retired_project() {
        let path = std::env::temp_dir().join(format!(
            "sandboard-test-api-unarchive-ok-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let b: SharedBoard = std::sync::Arc::new(crate::store::Board::new(
            crate::schema::Schema::default(),
            path,
        ));
        let project = b
            .create(
                None,
                "Archive then restore",
                "why",
                None,
                crate::model::Origin::Human,
                true,
                None,
            )
            .expect("project");
        let _ = b.transition(project.id, State::Shaping, "human", None);
        b.cut_scope(project.id, Some("archived".into()))
            .expect("cut");
        assert_eq!(b.get(project.id).unwrap().state, State::Retired);

        let Ok(Json(ids)) = unarchive_scope(
            AxState(b.clone()),
            Path(project.id),
            Json(ReasonReq {
                reason: Some("restored".into()),
            }),
        )
        .await
        else {
            panic!("unarchive_scope should succeed");
        };
        assert!(ids.contains(&project.id));
        assert_eq!(b.get(project.id).unwrap().state, State::Shaping);
        assert!(
            b.digest().goals.iter().any(|g| g.goal_id == project.id),
            "restored Project must reappear in digest"
        );
    }

    #[tokio::test]
    async fn unarchive_scope_rejects_non_retired() {
        let path = std::env::temp_dir().join(format!(
            "sandboard-test-api-unarchive-reject-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let b: SharedBoard = std::sync::Arc::new(crate::store::Board::new(
            crate::schema::Schema::default(),
            path,
        ));
        let project = b
            .create(
                None,
                "Still live",
                "why",
                None,
                crate::model::Origin::Human,
                true,
                None,
            )
            .expect("project");

        let Err(ApiError(msg)) = unarchive_scope(
            AxState(b.clone()),
            Path(project.id),
            Json(ReasonReq { reason: None }),
        )
        .await
        else {
            panic!("unarchive on non-retired must fail");
        };
        assert!(
            msg.contains("not retired"),
            "expected not-retired error, got {msg}"
        );
    }

    #[tokio::test]
    async fn create_task_api_lands_in_backlog_and_stamps_project_clone() {
        let b: SharedBoard = std::sync::Arc::new(crate::store::Board::new(
            crate::schema::Schema::default(),
            std::env::temp_dir().join(format!(
                "sandboard-test-create-task-repo-{}-{}.json",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            )),
        ));
        let project = b
            .create_project("P", "why", "acme/widgets", true, None)
            .expect("project");

        let Ok(Json(created)) = create_item(
            AxState(b.clone()),
            Json(CreateItem {
                parent: Some(project.id),
                title: "Prose clone target".into(),
                intent: "Clone repository: other/repo. Ship it.".into(),
                definition_of_done: Some("done".into()),
                capability: None,
                above_line: false,
                clone_repo: None,
                blocked_by: vec![],
                repo: None,
                product_repo: None,
                project_prompt: None,
            }),
        )
        .await
        else {
            panic!("Task create without repo field must succeed");
        };
        assert_eq!(created.state, State::Backlog);
        assert!(created.repo.is_none());
        assert!(
            created.intent.contains("Clone repository: other/repo"),
            "explicit Task clone must win: {}",
            created.intent
        );
        assert!(b.resolve_card_repo(created.id).unwrap().is_none());

        // Omit clone line — stamp Project default into intent.
        let Ok(Json(stamped)) = create_item(
            AxState(b.clone()),
            Json(CreateItem {
                parent: Some(project.id),
                title: "Inherit project clone".into(),
                intent: "Ship the feature".into(),
                definition_of_done: Some("done".into()),
                capability: None,
                above_line: false,
                clone_repo: None,
                blocked_by: vec![],
                repo: None,
                product_repo: None,
                project_prompt: None,
            }),
        )
        .await
        else {
            panic!("Task create should stamp Project clone");
        };
        assert_eq!(stamped.state, State::Backlog);
        assert!(
            stamped.intent.contains("Clone repository: acme/widgets"),
            "expected Project default stamped: {}",
            stamped.intent
        );

        // Extra repo body on create is accepted and unused.
        let Ok(Json(ignored)) = create_item(
            AxState(b.clone()),
            Json(CreateItem {
                parent: Some(project.id),
                title: "Ignored repo body".into(),
                intent: "Clone repository: acme/widgets. ok".into(),
                definition_of_done: Some("done".into()),
                capability: None,
                above_line: false,
                clone_repo: None,
                blocked_by: vec![],
                repo: Some(crate::schema::RepoConfig {
                    upstream: "acme/widgets".into(),
                    fork: String::new(),
                    base: "main".into(),
                }),
                product_repo: None,
                project_prompt: None,
            }),
        )
        .await
        else {
            panic!("Task create with repo body must still succeed");
        };
        assert!(ignored.repo.is_none());
        assert_eq!(ignored.state, State::Backlog);
    }

    #[tokio::test]
    async fn create_task_api_refuses_nest_under_task_and_applies_blockers() {
        let b: SharedBoard = std::sync::Arc::new(crate::store::Board::new(
            crate::schema::Schema::default(),
            std::env::temp_dir().join(format!(
                "sandboard-test-create-task-nest-{}-{}.json",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            )),
        ));
        let project = b
            .create_project("P", "why", "sandboard-app/sandboard", true, None)
            .expect("project");
        let Ok(Json(blocker)) = create_item(
            AxState(b.clone()),
            Json(CreateItem {
                parent: Some(project.id),
                title: "Blocker".into(),
                intent: "first".into(),
                definition_of_done: Some("done".into()),
                capability: None,
                above_line: false,
                clone_repo: None,
                blocked_by: vec![],
                repo: None,
                product_repo: None,
                project_prompt: None,
            }),
        )
        .await
        else {
            panic!("blocker create");
        };

        let Ok(Json(blocked)) = create_item(
            AxState(b.clone()),
            Json(CreateItem {
                parent: Some(project.id),
                title: "Blocked".into(),
                intent: "second".into(),
                definition_of_done: Some("done".into()),
                capability: None,
                above_line: false,
                clone_repo: None,
                blocked_by: vec![blocker.id],
                repo: None,
                product_repo: None,
                project_prompt: None,
            }),
        )
        .await
        else {
            panic!("blocked create");
        };
        assert_eq!(blocked.state, State::Backlog);
        assert_eq!(blocked.blocked_by, vec![blocker.id]);

        let Err(ApiError(nest)) = create_item(
            AxState(b.clone()),
            Json(CreateItem {
                parent: Some(blocker.id),
                title: "Nested".into(),
                intent: "no".into(),
                definition_of_done: Some("done".into()),
                capability: None,
                above_line: false,
                clone_repo: None,
                blocked_by: vec![],
                repo: None,
                product_repo: None,
                project_prompt: None,
            }),
        )
        .await
        else {
            panic!("nest under Task must fail");
        };
        assert!(
            nest.contains("flat under a Project") || nest.contains("parent must be a Project"),
            "expected nest refusal, got {nest}"
        );

        let Err(ApiError(missing_dod)) = create_item(
            AxState(b.clone()),
            Json(CreateItem {
                parent: Some(project.id),
                title: "No DoD".into(),
                intent: "oops".into(),
                definition_of_done: None,
                capability: None,
                above_line: false,
                clone_repo: None,
                blocked_by: vec![],
                repo: None,
                product_repo: None,
                project_prompt: None,
            }),
        )
        .await
        else {
            panic!("Task without DoD must fail");
        };
        assert!(
            missing_dod.contains("definition_of_done"),
            "expected DoD error, got {missing_dod}"
        );
    }

    /// Where a card ran and what it produced have to survive the trip to the
    /// browser. The card face reads them off the board snapshot and the drawer
    /// off the detail payload, where the item is `#[serde(flatten)]`ed — so
    /// either can stop carrying them without a single type changing.
    #[tokio::test]
    async fn a_finished_card_carries_its_pr_and_sandbox_to_the_ui() {
        let b: SharedBoard = std::sync::Arc::new(crate::store::Board::new(
            crate::schema::Schema::default(),
            std::env::temp_dir().join("sandboard-test-nowrite.json"),
        ));
        let id = b
            .create(
                None,
                "t",
                "i",
                None,
                crate::model::Origin::Human,
                false,
                None,
            )
            .expect("create")
            .id;
        b.set_environment(id, Some("sandboard-card-8-a1".into()));
        b.set_pr_url(id, Some("https://github.com/sandboard-app/sandboard/pull/1".into()));

        let Json(snap) = board(AxState(b.clone())).await;
        let on_the_card = serde_json::to_value(&snap).unwrap();
        assert_eq!(
            on_the_card["items"][0]["pull_requests"][0]["url"],
            "https://github.com/sandboard-app/sandboard/pull/1"
        );
        assert_eq!(on_the_card["items"][0]["environment"], "sandboard-card-8-a1");

        let Ok(Json(detail)) = item_detail(AxState(b), Path(id)).await else {
            panic!("no detail for the card we just created");
        };
        let in_the_drawer = serde_json::to_value(&detail).unwrap();
        assert_eq!(
            in_the_drawer["pull_requests"][0]["url"],
            "https://github.com/sandboard-app/sandboard/pull/1"
        );
        assert_eq!(in_the_drawer["environment"], "sandboard-card-8-a1");
    }

    #[tokio::test]
    async fn version_reports_the_crate_version() {
        let Json(v) = version().await;
        assert_eq!(v.version, env!("CARGO_PKG_VERSION"));
        assert_eq!(
            serde_json::to_value(&v).unwrap(),
            serde_json::json!({ "version": env!("CARGO_PKG_VERSION") }),
        );
    }

    #[tokio::test]
    async fn item_detail_and_board_snapshot_include_resolved_blockers() {
        let b: SharedBoard = std::sync::Arc::new(crate::store::Board::new(
            crate::schema::Schema::default(),
            std::env::temp_dir().join("sandboard-test-blockers.json"),
        ));
        let project = b
            .create(
                None,
                "Proj",
                "why",
                None,
                crate::model::Origin::Human,
                true,
                None,
            )
            .expect("project");
        let blocker = b
            .create(
                Some(project.id),
                "Blocker Task",
                "Must be done first",
                Some("done".into()),
                crate::model::Origin::Human,
                false,
                None,
            )
            .expect("blocker");
        let blocked = b
            .create(
                Some(project.id),
                "Blocked Task",
                "Waiting on blocker",
                Some("done".into()),
                crate::model::Origin::Human,
                false,
                None,
            )
            .expect("blocked");
        b.set_blocked_by(blocked.id, vec![blocker.id]);

        let Json(snap) = board(AxState(b.clone())).await;
        let snap_val = serde_json::to_value(&snap).unwrap();
        let blocked_item = snap_val["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["id"] == blocked.id)
            .expect("blocked item in snapshot");

        assert_eq!(blocked_item["blocked_by"], serde_json::json!([blocker.id]));
        assert_eq!(
            blocked_item["blockers"],
            serde_json::json!([
                {
                    "id": blocker.id,
                    "title": "Blocker Task",
                    "state": "draft"
                }
            ])
        );

        let Ok(Json(detail)) = item_detail(AxState(b), Path(blocked.id)).await else {
            panic!("no detail for blocked task");
        };
        let detail_val = serde_json::to_value(&detail).unwrap();
        assert_eq!(detail_val["blocked_by"], serde_json::json!([blocker.id]));
        assert_eq!(
            detail_val["blockers"],
            serde_json::json!([
                {
                    "id": blocker.id,
                    "title": "Blocker Task",
                    "state": "draft"
                }
            ])
        );
    }

    #[tokio::test]
    async fn github_webhook_accepts_valid_payload_and_emits_main_advanced() {
        use crate::events::BoardEvent;

        let b: SharedBoard = std::sync::Arc::new(crate::store::Board::new(
            crate::schema::Schema::default(),
            std::env::temp_dir().join("sandboard-test-webhook.json"),
        ));

        let mut rx = b.subscribe();

        // 1. Push to main branch
        let push_payload = serde_json::json!({
            "ref": "refs/heads/main",
            "after": "1234567890abcdef1234567890abcdef12345678",
            "repository": {
                "default_branch": "main"
            }
        });

        let mut headers = HeaderMap::new();
        headers.insert("x-github-event", "push".parse().unwrap());

        let Json(resp) = github_webhook(
            AxState(b.clone()),
            headers.clone(),
            Json(serde_json::from_value(push_payload).unwrap()),
        )
        .await
        .expect("webhook response");

        assert_eq!(resp.status, "ok");
        assert!(resp.main_advanced);
        assert_eq!(
            resp.commit_sha.as_deref(),
            Some("1234567890abcdef1234567890abcdef12345678")
        );

        let event = rx.try_recv().expect("event emitted");
        match event {
            BoardEvent::MainAdvanced {
                seq: _,
                ref_name,
                commit_sha,
            } => {
                assert_eq!(ref_name, "refs/heads/main");
                assert_eq!(
                    commit_sha.as_deref(),
                    Some("1234567890abcdef1234567890abcdef12345678")
                );
            }
            other => panic!("expected MainAdvanced, got {other:?}"),
        }

        // 2. PR merged into main
        let pr_payload = serde_json::json!({
            "action": "closed",
            "pull_request": {
                "merged": true,
                "merge_commit_sha": "fedcba0987654321fedcba0987654321fedcba09",
                "base": {
                    "ref": "main"
                }
            },
            "repository": {
                "default_branch": "main"
            }
        });

        let mut headers = HeaderMap::new();
        headers.insert("x-github-event", "pull_request".parse().unwrap());

        let Json(resp) = github_webhook(
            AxState(b.clone()),
            headers,
            Json(serde_json::from_value(pr_payload).unwrap()),
        )
        .await
        .expect("webhook response");

        assert_eq!(resp.status, "ok");
        assert!(resp.main_advanced);

        let event = rx.try_recv().expect("event emitted");
        match event {
            BoardEvent::MainAdvanced {
                seq: _,
                ref_name,
                commit_sha,
            } => {
                assert_eq!(ref_name, "main");
                assert_eq!(
                    commit_sha.as_deref(),
                    Some("fedcba0987654321fedcba0987654321fedcba09")
                );
            }
            other => panic!("expected MainAdvanced, got {other:?}"),
        }

        // 3. Push to feature branch (filtered out, no event emitted)
        let feature_push = serde_json::json!({
            "ref": "refs/heads/feature/my-branch",
            "after": "9999999999999999999999999999999999999999",
            "repository": {
                "default_branch": "main"
            }
        });

        let mut headers = HeaderMap::new();
        headers.insert("x-github-event", "push".parse().unwrap());

        let Json(resp) = github_webhook(
            AxState(b.clone()),
            headers,
            Json(serde_json::from_value(feature_push).unwrap()),
        )
        .await
        .expect("webhook response");

        assert_eq!(resp.status, "ignored");
        assert!(!resp.main_advanced);
        assert!(
            rx.try_recv().is_err(),
            "no event should be emitted for feature branch push"
        );

        // 4. Ping event (no event emitted)
        let ping_payload = serde_json::json!({
            "zen": "Non-blocking is better than blocking."
        });

        let mut headers = HeaderMap::new();
        headers.insert("x-github-event", "ping".parse().unwrap());

        let Json(resp) = github_webhook(
            AxState(b.clone()),
            headers,
            Json(serde_json::from_value(ping_payload).unwrap()),
        )
        .await
        .expect("webhook response");

        assert_eq!(resp.status, "pong");
        assert!(!resp.main_advanced);
        assert!(
            rx.try_recv().is_err(),
            "no event should be emitted for ping"
        );
    }

    #[tokio::test]
    async fn github_webhook_endpoint_route_integration() {
        use tower_service::Service;

        let b: SharedBoard = std::sync::Arc::new(crate::store::Board::new(
            crate::schema::Schema::default(),
            std::env::temp_dir().join("sandboard-test-route.json"),
        ));

        let mut app = Router::new().nest("/api", routes()).with_state(b.clone());
        let mut rx = b.subscribe();

        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/api/webhooks/github")
            .header("content-type", "application/json")
            .header("x-github-event", "push")
            .body(axum::body::Body::from(
                serde_json::json!({
                    "ref": "refs/heads/main",
                    "after": "11223344556677889900aabbccddeeff11223344"
                })
                .to_string(),
            ))
            .unwrap();

        let response = app.call(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let event = rx.try_recv().expect("event emitted over route");
        match event {
            crate::events::BoardEvent::MainAdvanced { commit_sha, .. } => {
                assert_eq!(
                    commit_sha.as_deref(),
                    Some("11223344556677889900aabbccddeeff11223344")
                );
            }
            other => panic!("expected MainAdvanced event, got {other:?}"),
        }
    }

    fn review_card_with_pr(b: &SharedBoard, pr_url: &str) -> u64 {
        use crate::model::{Origin, State};
        let p = b
            .create(
                None,
                "Webhook Proj",
                "intent",
                None,
                Origin::Human,
                true,
                None,
            )
            .unwrap();
        let t = b
            .create(
                Some(p.id),
                "Impl",
                "intent",
                Some("dod".into()),
                Origin::Human,
                false,
                None,
            )
            .unwrap();
        let _ = b.transition(t.id, State::Shaping, "human", None);
        let _ = b.transition(t.id, State::Backlog, "human", None);
        let _ = b.transition(t.id, State::Claimed, "agent", None);
        let _ = b.transition(t.id, State::Running, "agent", None);
        let _ = b.transition(t.id, State::Review, "agent", None);
        b.set_pr_url(t.id, Some(pr_url.to_string()));
        t.id
    }

    fn running_card_with_pr(b: &SharedBoard, pr_url: &str) -> u64 {
        use crate::model::{Origin, State};
        let p = b
            .create(
                None,
                "Webhook Running Proj",
                "intent",
                None,
                Origin::Human,
                true,
                None,
            )
            .unwrap();
        let t = b
            .create(
                Some(p.id),
                "Live Impl",
                "intent",
                Some("dod".into()),
                Origin::Human,
                false,
                None,
            )
            .unwrap();
        let _ = b.transition(t.id, State::Shaping, "human", None);
        let _ = b.transition(t.id, State::Backlog, "human", None);
        let _ = b.transition(t.id, State::Claimed, "agent", None);
        let _ = b.transition(t.id, State::Running, "agent", None);
        b.set_pr_url(t.id, Some(pr_url.to_string()));
        t.id
    }

    #[tokio::test]
    async fn github_webhook_main_advanced_lists_steered_same_repo_running_ids() {
        use crate::model::State;

        let b: SharedBoard = std::sync::Arc::new(crate::store::Board::new(
            crate::schema::Schema::default(),
            std::env::temp_dir().join(format!(
                "sandboard-test-webhook-steer-same-{}.json",
                std::process::id()
            )),
        ));
        let pr_url = "https://github.com/sandboard-app/sandboard/pull/5501";
        let id = running_card_with_pr(&b, pr_url);

        let push_payload = serde_json::json!({
            "ref": "refs/heads/main",
            "after": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "repository": {
                "default_branch": "main",
                "full_name": "sandboard-app/sandboard"
            }
        });

        let mut headers = HeaderMap::new();
        headers.insert("x-github-event", "push".parse().unwrap());

        let Json(resp) = github_webhook(
            AxState(b.clone()),
            headers,
            Json(serde_json::from_value(push_payload).unwrap()),
        )
        .await
        .expect("webhook response");

        assert!(resp.main_advanced);
        assert_eq!(resp.steered_item_ids, vec![id]);
        let item = b.get(id).unwrap();
        assert_eq!(item.state, State::Backlog);
        assert!(item.awaiting_dispatch);
        assert!(
            item.notes.iter().any(|n| {
                n.text.contains("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
                    && n.text.contains("Main advanced")
                    && n.text.to_lowercase().contains("rebase")
            }),
            "steer note must describe main-advanced rebase: {:?}",
            item.notes
        );
    }

    #[tokio::test]
    async fn github_webhook_main_advanced_skips_cross_repo_running_cards() {
        use crate::model::State;

        let b: SharedBoard = std::sync::Arc::new(crate::store::Board::new(
            crate::schema::Schema::default(),
            std::env::temp_dir().join(format!(
                "sandboard-test-webhook-steer-skip-{}.json",
                std::process::id()
            )),
        ));
        let pr_url = "https://github.com/other/widgets/pull/5502";
        let id = running_card_with_pr(&b, pr_url);

        let push_payload = serde_json::json!({
            "ref": "refs/heads/main",
            "after": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            "repository": {
                "default_branch": "main",
                "full_name": "sandboard-app/sandboard"
            }
        });

        let mut headers = HeaderMap::new();
        headers.insert("x-github-event", "push".parse().unwrap());

        let Json(resp) = github_webhook(
            AxState(b.clone()),
            headers,
            Json(serde_json::from_value(push_payload).unwrap()),
        )
        .await
        .expect("webhook response");

        assert!(resp.main_advanced);
        assert!(resp.steered_item_ids.is_empty());
        let item = b.get(id).unwrap();
        assert_eq!(item.state, State::Running);
        assert!(!item.awaiting_dispatch);
        assert!(item.notes.is_empty());
    }

    #[tokio::test]
    async fn github_webhook_merged_pr_completes_matching_review_card() {
        use crate::model::State;

        let b: SharedBoard = std::sync::Arc::new(crate::store::Board::new(
            crate::schema::Schema::default(),
            std::env::temp_dir().join(format!(
                "sandboard-test-webhook-complete-{}.json",
                std::process::id()
            )),
        ));
        let mut rx = b.subscribe();

        let pr_url = "https://github.com/sandboard-app/sandboard/pull/4242";
        let id = review_card_with_pr(&b, pr_url);
        // Drain create/transition noise.
        while rx.try_recv().is_ok() {}

        let pr_payload = serde_json::json!({
            "action": "closed",
            "pull_request": {
                "merged": true,
                "html_url": pr_url,
                "number": 4242,
                "merge_commit_sha": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "base": { "ref": "main" }
            },
            "repository": {
                "default_branch": "main",
                "full_name": "sandboard-app/sandboard"
            }
        });

        let mut headers = HeaderMap::new();
        headers.insert("x-github-event", "pull_request".parse().unwrap());

        let Json(resp) = github_webhook(
            AxState(b.clone()),
            headers,
            Json(serde_json::from_value(pr_payload).unwrap()),
        )
        .await
        .expect("webhook response");

        assert_eq!(resp.status, "ok");
        assert!(resp.main_advanced);
        assert_eq!(resp.completed_item_ids, vec![id]);
        assert_eq!(b.get(id).unwrap().state, State::Done);

        let mut saw_main = false;
        while let Ok(ev) = rx.try_recv() {
            if matches!(ev, crate::events::BoardEvent::MainAdvanced { .. }) {
                saw_main = true;
            }
        }
        assert!(saw_main, "MainAdvanced should still fire on merge");
    }

    #[tokio::test]
    async fn github_webhook_closed_unmerged_pr_does_not_complete_card() {
        use crate::model::State;

        let b: SharedBoard = std::sync::Arc::new(crate::store::Board::new(
            crate::schema::Schema::default(),
            std::env::temp_dir().join(format!(
                "sandboard-test-webhook-unmerged-{}.json",
                std::process::id()
            )),
        ));
        let pr_url = "https://github.com/sandboard-app/sandboard/pull/4243";
        let id = review_card_with_pr(&b, pr_url);

        let pr_payload = serde_json::json!({
            "action": "closed",
            "pull_request": {
                "merged": false,
                "html_url": pr_url,
                "number": 4243,
                "base": { "ref": "main" }
            },
            "repository": {
                "default_branch": "main",
                "full_name": "sandboard-app/sandboard"
            }
        });

        let mut headers = HeaderMap::new();
        headers.insert("x-github-event", "pull_request".parse().unwrap());

        let Json(resp) = github_webhook(
            AxState(b.clone()),
            headers,
            Json(serde_json::from_value(pr_payload).unwrap()),
        )
        .await
        .expect("webhook response");

        assert_eq!(resp.status, "ignored");
        assert!(!resp.main_advanced);
        assert!(resp.completed_item_ids.is_empty());
        assert_eq!(b.get(id).unwrap().state, State::Review);
    }

    #[tokio::test]
    async fn github_webhook_merged_pr_no_matching_card_still_advances_main() {
        let b: SharedBoard = std::sync::Arc::new(crate::store::Board::new(
            crate::schema::Schema::default(),
            std::env::temp_dir().join(format!(
                "sandboard-test-webhook-nomatch-{}.json",
                std::process::id()
            )),
        ));
        let mut rx = b.subscribe();

        let pr_payload = serde_json::json!({
            "action": "closed",
            "pull_request": {
                "merged": true,
                "html_url": "https://github.com/sandboard-app/sandboard/pull/99999",
                "number": 99999,
                "merge_commit_sha": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                "base": { "ref": "main" }
            },
            "repository": {
                "default_branch": "main",
                "full_name": "sandboard-app/sandboard"
            }
        });

        let mut headers = HeaderMap::new();
        headers.insert("x-github-event", "pull_request".parse().unwrap());

        let Json(resp) = github_webhook(
            AxState(b.clone()),
            headers,
            Json(serde_json::from_value(pr_payload).unwrap()),
        )
        .await
        .expect("webhook response");

        assert_eq!(resp.status, "ok");
        assert!(resp.main_advanced);
        assert!(resp.completed_item_ids.is_empty());
        assert!(matches!(
            rx.try_recv().expect("MainAdvanced"),
            crate::events::BoardEvent::MainAdvanced { .. }
        ));
    }

    #[tokio::test]
    async fn github_webhook_merged_pr_complete_is_idempotent() {
        use crate::model::State;

        let b: SharedBoard = std::sync::Arc::new(crate::store::Board::new(
            crate::schema::Schema::default(),
            std::env::temp_dir().join(format!(
                "sandboard-test-webhook-idempotent-{}.json",
                std::process::id()
            )),
        ));
        let pr_url = "https://github.com/sandboard-app/sandboard/pull/4244";
        let id = review_card_with_pr(&b, pr_url);

        let pr_payload = serde_json::json!({
            "action": "closed",
            "pull_request": {
                "merged": true,
                "number": 4244,
                "merge_commit_sha": "cccccccccccccccccccccccccccccccccccccccc",
                "base": { "ref": "main" }
            },
            "repository": {
                "default_branch": "main",
                "full_name": "sandboard-app/sandboard"
            }
        });

        let mut headers = HeaderMap::new();
        headers.insert("x-github-event", "pull_request".parse().unwrap());

        let Json(resp1) = github_webhook(
            AxState(b.clone()),
            headers.clone(),
            Json(serde_json::from_value(pr_payload.clone()).unwrap()),
        )
        .await
        .expect("first");
        assert_eq!(resp1.completed_item_ids, vec![id]);
        assert_eq!(b.get(id).unwrap().state, State::Done);

        let Json(resp2) = github_webhook(
            AxState(b.clone()),
            headers,
            Json(serde_json::from_value(pr_payload).unwrap()),
        )
        .await
        .expect("second");
        assert!(resp2.main_advanced);
        assert!(
            resp2.completed_item_ids.is_empty(),
            "already-Done card must not re-complete"
        );
        assert_eq!(b.get(id).unwrap().state, State::Done);
    }

    #[tokio::test]
    async fn github_webhook_merge_leaves_sibling_review_for_mergeable_observation() {
        use crate::model::{Origin, State};

        let b: SharedBoard = std::sync::Arc::new(crate::store::Board::new(
            crate::schema::Schema::default(),
            std::env::temp_dir().join(format!(
                "sandboard-test-webhook-rebase-{}.json",
                std::process::id()
            )),
        ));

        let p = b
            .create(
                None,
                "Webhook Rebase Proj",
                "intent",
                None,
                Origin::Human,
                true,
                None,
            )
            .unwrap();

        let t1 = b
            .create(
                Some(p.id),
                "Impl 1",
                "intent 1",
                Some("dod 1".into()),
                Origin::Human,
                false,
                None,
            )
            .unwrap();
        let t2 = b
            .create(
                Some(p.id),
                "Impl 2",
                "intent 2",
                Some("dod 2".into()),
                Origin::Human,
                false,
                None,
            )
            .unwrap();

        let pr1_url = "https://github.com/sandboard-app/sandboard/pull/5001";
        let pr2_url = "https://github.com/sandboard-app/sandboard/pull/5002";

        for (id, url) in [(t1.id, pr1_url), (t2.id, pr2_url)] {
            let _ = b.transition(id, State::Shaping, "human", None);
            let _ = b.transition(id, State::Backlog, "human", None);
            let _ = b.transition(id, State::Claimed, "agent", None);
            let _ = b.transition(id, State::Review, "agent", None);
            b.set_pr_url(id, Some(url.to_string()));
        }

        let pr_payload = serde_json::json!({
            "action": "closed",
            "pull_request": {
                "merged": true,
                "html_url": pr1_url,
                "number": 5001,
                "merge_commit_sha": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                "base": { "ref": "main" }
            },
            "repository": {
                "default_branch": "main",
                "full_name": "sandboard-app/sandboard"
            }
        });

        let mut headers = HeaderMap::new();
        headers.insert("x-github-event", "pull_request".parse().unwrap());

        let Json(resp) = github_webhook(
            AxState(b.clone()),
            headers,
            Json(serde_json::from_value(pr_payload).unwrap()),
        )
        .await
        .expect("webhook response");

        assert_eq!(resp.completed_item_ids, vec![t1.id]);
        assert_eq!(b.get(t1.id).unwrap().state, State::Done);

        let t2_card = b.get(t2.id).unwrap();
        assert_eq!(t2_card.state, State::Review);
        // Without an App token catch-up may defer and queue retry; merge→Done
        // itself must not be the only path that treats Review as rebase work.
        assert!(
            b.identify_behind_sibling_prs(t1.id)
                .iter()
                .any(|i| i.id == t2.id)
                || t2_card.rebase_requested,
            "sibling must remain a catch-up target (candidate and/or deferred retry)"
        );
    }

    fn review_feedback_payload(
        action: &str,
        review_state: &str,
        review_body: &str,
        pr_url: &str,
        pr_number: u64,
    ) -> serde_json::Value {
        serde_json::json!({
            "action": action,
            "review": {
                "state": review_state,
                "body": review_body,
            },
            "pull_request": {
                "html_url": pr_url,
                "number": pr_number,
                "merged": false,
                "base": { "ref": "main" }
            },
            "repository": {
                "default_branch": "main",
                "full_name": "sandboard-app/sandboard"
            }
        })
    }

    #[tokio::test]
    async fn github_webhook_pr_review_changes_requested_steers_to_backlog() {
        use crate::model::State;

        let b: SharedBoard = std::sync::Arc::new(crate::store::Board::new(
            crate::schema::Schema::default(),
            std::env::temp_dir().join(format!(
                "sandboard-test-webhook-review-cr-{}.json",
                std::process::id()
            )),
        ));
        let pr_url = "https://github.com/sandboard-app/sandboard/pull/4243";
        let id = review_card_with_pr(&b, pr_url);

        let mut headers = HeaderMap::new();
        headers.insert("x-github-event", "pull_request_review".parse().unwrap());

        let Json(resp) = github_webhook(
            AxState(b.clone()),
            headers,
            Json(
                serde_json::from_value(review_feedback_payload(
                    "submitted",
                    "changes_requested",
                    "please dump this body into the note — must not appear",
                    pr_url,
                    4243,
                ))
                .unwrap(),
            ),
        )
        .await
        .expect("webhook response");

        assert_eq!(resp.status, "ok");
        assert!(!resp.main_advanced);
        assert!(resp.completed_item_ids.is_empty());
        assert_eq!(resp.steered_item_ids, vec![id]);

        let item = b.get(id).unwrap();
        assert_eq!(item.state, State::Backlog);
        let note = &item.notes.last().expect("pointer steer note").text;
        assert!(
            note.contains("PR review feedback") && note.contains("gh"),
            "pointer-style note expected, got: {note}"
        );
        assert!(
            !note.contains("please dump") && !note.contains("must not appear"),
            "must not forward review body into steer note: {note}"
        );
    }

    #[tokio::test]
    async fn github_webhook_pr_review_comment_same_steer_path() {
        use crate::model::State;

        let b: SharedBoard = std::sync::Arc::new(crate::store::Board::new(
            crate::schema::Schema::default(),
            std::env::temp_dir().join(format!(
                "sandboard-test-webhook-review-comment-{}.json",
                std::process::id()
            )),
        ));
        let pr_url = "https://github.com/sandboard-app/sandboard/pull/4244";
        let id = review_card_with_pr(&b, pr_url);

        let mut headers = HeaderMap::new();
        headers.insert("x-github-event", "pull_request_review".parse().unwrap());

        let Json(resp) = github_webhook(
            AxState(b.clone()),
            headers,
            Json(
                serde_json::from_value(review_feedback_payload(
                    "submitted",
                    "commented",
                    "nit: rename foo — must not be summarized into the note",
                    pr_url,
                    4244,
                ))
                .unwrap(),
            ),
        )
        .await
        .expect("webhook response");

        assert_eq!(resp.status, "ok");
        assert_eq!(resp.steered_item_ids, vec![id]);
        let item = b.get(id).unwrap();
        assert_eq!(item.state, State::Backlog);
        let note = &item.notes.last().unwrap().text;
        assert!(note.contains("PR review feedback") && note.contains("gh"));
        assert!(!note.contains("rename foo"));
    }

    #[tokio::test]
    async fn github_webhook_pr_review_approved_is_board_noop() {
        use crate::model::State;

        let b: SharedBoard = std::sync::Arc::new(crate::store::Board::new(
            crate::schema::Schema::default(),
            std::env::temp_dir().join(format!(
                "sandboard-test-webhook-review-approved-{}.json",
                std::process::id()
            )),
        ));
        let pr_url = "https://github.com/sandboard-app/sandboard/pull/4245";
        let id = review_card_with_pr(&b, pr_url);
        let notes_before = b.get(id).unwrap().notes.len();

        let mut headers = HeaderMap::new();
        headers.insert("x-github-event", "pull_request_review".parse().unwrap());

        let Json(resp) = github_webhook(
            AxState(b.clone()),
            headers,
            Json(
                serde_json::from_value(review_feedback_payload(
                    "submitted",
                    "approved",
                    "LGTM",
                    pr_url,
                    4245,
                ))
                .unwrap(),
            ),
        )
        .await
        .expect("webhook response");

        assert_eq!(resp.status, "ignored");
        assert!(!resp.main_advanced);
        assert!(resp.steered_item_ids.is_empty());
        assert_eq!(b.get(id).unwrap().state, State::Review);
        assert_eq!(b.get(id).unwrap().notes.len(), notes_before);
    }

    #[tokio::test]
    async fn github_webhook_pr_review_malformed_and_unknown_ignored() {
        use crate::model::State;

        let b: SharedBoard = std::sync::Arc::new(crate::store::Board::new(
            crate::schema::Schema::default(),
            std::env::temp_dir().join(format!(
                "sandboard-test-webhook-review-malformed-{}.json",
                std::process::id()
            )),
        ));
        let pr_url = "https://github.com/sandboard-app/sandboard/pull/4246";
        let id = review_card_with_pr(&b, pr_url);

        let mut headers = HeaderMap::new();
        headers.insert("x-github-event", "pull_request_review".parse().unwrap());

        // Non-submitted action — ignore.
        let Json(resp) = github_webhook(
            AxState(b.clone()),
            headers.clone(),
            Json(
                serde_json::from_value(review_feedback_payload(
                    "edited",
                    "changes_requested",
                    "body",
                    pr_url,
                    4246,
                ))
                .unwrap(),
            ),
        )
        .await
        .expect("webhook response");
        assert_eq!(resp.status, "ignored");
        assert!(resp.steered_item_ids.is_empty());
        assert_eq!(b.get(id).unwrap().state, State::Review);

        // Unknown PR URL — Board no-op.
        let Json(resp) = github_webhook(
            AxState(b.clone()),
            headers.clone(),
            Json(
                serde_json::from_value(review_feedback_payload(
                    "submitted",
                    "changes_requested",
                    "body",
                    "https://github.com/sandboard-app/sandboard/pull/99999",
                    99999,
                ))
                .unwrap(),
            ),
        )
        .await
        .expect("webhook response");
        assert_eq!(resp.status, "ignored");
        assert!(resp.steered_item_ids.is_empty());
        assert_eq!(b.get(id).unwrap().state, State::Review);

        // Malformed: submitted but missing review.state.
        let Json(resp) = github_webhook(
            AxState(b.clone()),
            headers,
            Json(
                serde_json::from_value(serde_json::json!({
                    "action": "submitted",
                    "review": { "body": "no state" },
                    "pull_request": {
                        "html_url": pr_url,
                        "number": 4246,
                        "merged": false
                    },
                    "repository": { "full_name": "sandboard-app/sandboard" }
                }))
                .unwrap(),
            ),
        )
        .await
        .expect("webhook response");
        assert_eq!(resp.status, "ignored");
        assert!(resp.steered_item_ids.is_empty());
        assert_eq!(b.get(id).unwrap().state, State::Review);
    }

    fn sandbox_profiles_board() -> SharedBoard {
        std::sync::Arc::new(crate::store::Board::new(
            crate::schema::Schema::default(),
            std::env::temp_dir().join(format!(
                "sandboard-test-api-sbx-{}.json",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            )),
        ))
    }

    #[tokio::test]
    async fn sandbox_profiles_list_create_update_default_and_project_assign() {
        use crate::model::Origin;

        let b = sandbox_profiles_board();

        let Json(empty) = list_sandbox_profiles(AxState(b.clone())).await;
        assert!(empty.profiles.is_empty());
        assert!(empty.default_sandbox_profile_id.is_none());
        assert!(empty.cockpit_sandbox_profile_id.is_none());
        assert_eq!(
            empty.create_defaults.policy_id,
            crate::seed_policies::MINIMAL_POLICY_ID
        );
        // Listing seeds the minimal Policies catalog row.
        assert!(
            b.get_openshell_policy(crate::seed_policies::MINIMAL_POLICY_ID)
                .is_some()
        );

        let Ok(Json(pol)) = upsert_openshell_policy(
            AxState(b.clone()),
            Json(UpsertOpenShellPolicyReq {
                id: Some("api-test".into()),
                name: "API test".into(),
                yaml: "version: 1\n# api-test\n".into(),
            }),
        )
        .await
        else {
            panic!("create policy");
        };
        assert_eq!(pol.id, "api-test");

        let Ok(Json(created)) = upsert_sandbox_profile(
            AxState(b.clone()),
            Json(UpsertSandboxProfileReq {
                id: Some("default".into()),
                name: "Default".into(),
                image: "sandboard-sandbox:latest".into(),
                policy_id: "api-test".into(),
                cpu: Some("2".into()),
                memory: Some("4Gi".into()),
                engine: None,
                ..Default::default()
            }),
        )
        .await
        else {
            panic!("create default profile");
        };
        assert_eq!(created.id, "default");
        assert_eq!(created.image, "sandboard-sandbox:latest");
        assert_eq!(created.policy_id, "api-test");

        let Ok(Json(heavy)) = upsert_sandbox_profile(
            AxState(b.clone()),
            Json(UpsertSandboxProfileReq {
                id: Some("heavy".into()),
                name: "Heavy".into(),
                image: "sandboard-sandbox:heavy".into(),
                policy_id: "api-test".into(),
                cpu: Some("8".into()),
                memory: Some("16Gi".into()),
                engine: None,
                ..Default::default()
            }),
        )
        .await
        else {
            panic!("create heavy profile");
        };
        assert_eq!(heavy.id, "heavy");

        // Update via upsert.
        let Ok(Json(updated)) = upsert_sandbox_profile(
            AxState(b.clone()),
            Json(UpsertSandboxProfileReq {
                id: Some("heavy".into()),
                name: "Heavy+".into(),
                image: "sandboard-sandbox:heavy2".into(),
                policy_id: "api-test".into(),
                cpu: Some("8".into()),
                memory: Some("32Gi".into()),
                engine: None,
                ..Default::default()
            }),
        )
        .await
        else {
            panic!("update heavy profile");
        };
        assert_eq!(updated.name, "Heavy+");
        assert_eq!(updated.memory.as_deref(), Some("32Gi"));

        let Ok(Json(listed)) =
            set_default_sandbox_profile(AxState(b.clone()), Path("default".into())).await
        else {
            panic!("set default");
        };
        assert_eq!(
            listed.default_sandbox_profile_id.as_deref(),
            Some("default")
        );
        assert_eq!(listed.profiles.len(), 2);

        let Ok(Json(cockpit)) =
            set_cockpit_sandbox_profile(AxState(b.clone()), Path("heavy".into())).await
        else {
            panic!("set cockpit");
        };
        assert_eq!(cockpit.cockpit_sandbox_profile_id.as_deref(), Some("heavy"));
        assert_eq!(
            cockpit.default_sandbox_profile_id.as_deref(),
            Some("default"),
            "setting Cockpit must not clear the worker default"
        );

        let Ok(Json(got)) = get_sandbox_profile(AxState(b.clone()), Path("heavy".into())).await
        else {
            panic!("get heavy");
        };
        assert_eq!(got.image, "sandboard-sandbox:heavy2");

        let Ok(Json(with_env)) = upsert_sandbox_profile(
            AxState(b.clone()),
            Json(UpsertSandboxProfileReq {
                id: Some("env-notes".into()),
                name: "Env notes".into(),
                image: "sandboard-sandbox:env".into(),
                policy_id: "api-test".into(),
                cpu: None,
                memory: None,
                engine: None,
                model: None,
                provider_names: Vec::new(),
                mcp_server_ids: Vec::new(),
                env: std::collections::BTreeMap::from([(
                    "API_URL".into(),
                    "https://example.test".into(),
                )]),
                prompt: Some("Use the API URL from env.".into()),
            }),
        )
        .await
        else {
            panic!("create env profile");
        };
        assert_eq!(
            with_env.env.get("API_URL").map(String::as_str),
            Some("https://example.test")
        );
        assert_eq!(
            with_env.prompt.as_deref(),
            Some("Use the API URL from env.")
        );
        let Json(list_out) = list_sandbox_profiles(AxState(b.clone())).await;
        let listed_env = list_out
            .profiles
            .iter()
            .find(|p| p.id == "env-notes")
            .expect("env profile listed");
        assert_eq!(
            listed_env.env.get("API_URL").map(String::as_str),
            Some("https://example.test")
        );
        assert_eq!(
            listed_env.prompt.as_deref(),
            Some("Use the API URL from env.")
        );

        let project = b
            .create(None, "Sbx Proj", "why", None, Origin::Human, true, None)
            .expect("project");
        let Ok(Json(assigned)) = set_item_sandbox_profile(
            AxState(b.clone()),
            Path(project.id),
            Json(SetProjectSandboxProfileReq {
                sandbox_profile_id: Some("heavy".into()),
            }),
        )
        .await
        else {
            panic!("assign project profile");
        };
        assert_eq!(assigned.sandbox_profile_id.as_deref(), Some("heavy"));

        let Ok(Json(cleared)) = set_item_sandbox_profile(
            AxState(b.clone()),
            Path(project.id),
            Json(SetProjectSandboxProfileReq {
                sandbox_profile_id: None,
            }),
        )
        .await
        else {
            panic!("clear project profile");
        };
        assert!(cleared.sandbox_profile_id.is_none());

        // Task assign must fail (Projects only).
        let task = b
            .create(
                Some(project.id),
                "task",
                "do",
                Some("done".into()),
                Origin::Human,
                false,
                None,
            )
            .expect("task");
        assert!(set_item_sandbox_profile(
            AxState(b.clone()),
            Path(task.id),
            Json(SetProjectSandboxProfileReq {
                sandbox_profile_id: Some("default".into()),
            }),
        )
        .await
        .is_err());

        // In-use policy delete fails.
        let err = delete_openshell_policy(AxState(b.clone()), Path("api-test".into()))
            .await
            .unwrap_err();
        assert!(err.0.contains("in use"), "got {}", err.0);

        // Unknown policy_id refused on profile upsert.
        assert!(upsert_sandbox_profile(
            AxState(b.clone()),
            Json(UpsertSandboxProfileReq {
                id: Some("bad".into()),
                name: "Bad".into(),
                image: "img:x".into(),
                policy_id: "missing".into(),
                ..Default::default()
            }),
        )
        .await
        .is_err());
    }

    #[tokio::test]
    async fn sandbox_profiles_create_omits_id_and_returns_slug() {
        let b = sandbox_profiles_board();
        let _ = list_sandbox_profiles(AxState(b.clone())).await; // seed minimal
        let policy_id = crate::seed_policies::MINIMAL_POLICY_ID.to_string();

        let Ok(Json(created)) = upsert_sandbox_profile(
            AxState(b.clone()),
            Json(UpsertSandboxProfileReq {
                id: None,
                name: "Heavy CI".into(),
                image: "img:ci".into(),
                policy_id: policy_id.clone(),
                cpu: None,
                memory: None,
                engine: None,
                ..Default::default()
            }),
        )
        .await
        else {
            panic!("create without id");
        };
        assert_eq!(created.id, "heavy-ci");
        assert_eq!(created.name, "Heavy CI");

        // Empty string id is also treated as auto-slug.
        let Ok(Json(second)) = upsert_sandbox_profile(
            AxState(b.clone()),
            Json(UpsertSandboxProfileReq {
                id: Some("".into()),
                name: "Heavy CI".into(),
                image: "img:ci2".into(),
                policy_id,
                cpu: None,
                memory: None,
                engine: None,
                ..Default::default()
            }),
        )
        .await
        else {
            panic!("create colliding name");
        };
        assert_eq!(second.id, "heavy-ci-2");
    }

    #[tokio::test]
    async fn openshell_policies_list_upsert_get_delete() {
        let b = sandbox_profiles_board();
        let Json(listed) = list_openshell_policies(AxState(b.clone())).await;
        assert_eq!(
            listed.create_default_policy_id,
            crate::seed_policies::MINIMAL_POLICY_ID
        );
        assert!(
            listed
                .policies
                .iter()
                .any(|p| p.id == crate::seed_policies::MINIMAL_POLICY_ID)
        );

        let Ok(Json(created)) = upsert_openshell_policy(
            AxState(b.clone()),
            Json(UpsertOpenShellPolicyReq {
                id: None,
                name: "Allow npm".into(),
                yaml: "version: 1\n# npm\n".into(),
            }),
        )
        .await
        else {
            panic!("create");
        };
        assert_eq!(created.id, "allow-npm");

        let Ok(Json(got)) =
            get_openshell_policy(AxState(b.clone()), Path("allow-npm".into())).await
        else {
            panic!("get");
        };
        assert_eq!(got.yaml, "version: 1\n# npm\n");

        let Ok(Json(ok)) =
            delete_openshell_policy(AxState(b.clone()), Path("allow-npm".into())).await
        else {
            panic!("delete");
        };
        assert_eq!(ok["ok"], true);
        assert!(get_openshell_policy(AxState(b.clone()), Path("allow-npm".into()))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn webhook_poll_get_put_clamps_interval() {
        let path = std::env::temp_dir().join(format!(
            "sandboard-test-api-whpoll-{}.json",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let b = std::sync::Arc::new(crate::store::Board::new(
            crate::schema::Schema::default(),
            path,
        ));
        let Json(defaults) = get_webhook_poll(AxState(b.clone())).await;
        assert!(!defaults.enabled);
        assert_eq!(defaults.interval_secs, 60);

        let Json(saved) = put_webhook_poll(
            AxState(b.clone()),
            Json(WebhookPollConfig {
                enabled: true,
                interval_secs: 5,
                provider_name: None,
            }),
        )
        .await;
        assert_eq!(
            saved.interval_secs,
            crate::model::MIN_WEBHOOK_POLL_INTERVAL_SECS
        );
        let Json(got) = get_webhook_poll(AxState(b.clone())).await;
        assert_eq!(got, saved);
    }

    #[tokio::test]
    async fn workspace_get_put_persists_forge_only() {
        let path = std::env::temp_dir().join(format!(
            "sandboard-test-api-ws-{}.json",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let schema = crate::schema::Schema::default();
        let b = std::sync::Arc::new(crate::store::Board::new(schema, path));

        let Json(empty) = get_workspace(AxState(b.clone())).await;
        assert_eq!(empty.forge, "github");

        let Ok(Json(saved)) = put_workspace(
            AxState(b.clone()),
            Json(WorkspaceBinding {
                forge: "github".into(),
            }),
        )
        .await
        else {
            panic!("put workspace");
        };
        assert_eq!(saved.forge, "github");
        let Json(got) = get_workspace(AxState(b.clone())).await;
        assert_eq!(got, saved);

        // Work remotes are yaml-only; Settings forge does not supply them.
        assert!(b.yaml_work_repo().is_none());
        // Unsupported forge is refused.
        assert!(put_workspace(
            AxState(b.clone()),
            Json(WorkspaceBinding {
                forge: "gitlab".into(),
            }),
        )
        .await
        .is_err());
    }

    #[tokio::test]
    async fn openshell_status_not_configured_without_endpoint() {
        let path = std::env::temp_dir().join(format!(
            "sandboard-test-api-os-miss-{}.json",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let b = std::sync::Arc::new(crate::store::Board::new(
            crate::schema::Schema::default(),
            path,
        ));
        let Json(st) = openshell_status(AxState(b.clone())).await;
        assert!(!st.healthy);
        assert!(st.not_configured, "summary={}", st.summary);
        assert!(
            st.summary.contains("endpoint") || st.summary.contains("not configured"),
            "summary={}",
            st.summary
        );
    }

    #[tokio::test]
    async fn openshell_status_healthy_when_injected_mock_ok() {
        let path = std::env::temp_dir().join(format!(
            "sandboard-test-api-os-ok-{}.json",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut board = crate::store::Board::new(crate::schema::Schema::default(), path);
        board.openshell = Some(crate::openshell::OpenShell::mock(
            |_| crate::openshell::Output {
                code: 0,
                stdout: "Connected".into(),
                stderr: String::new(),
            },
            std::time::Duration::from_secs(5),
        ));
        let b = std::sync::Arc::new(board);
        let Json(st) = openshell_status(AxState(b.clone())).await;
        assert!(st.healthy, "summary={}", st.summary);
        assert!(!st.not_configured);
    }

    #[tokio::test]
    async fn openshell_status_unhealthy_when_injected_mock_fails() {
        let path = std::env::temp_dir().join(format!(
            "sandboard-test-api-os-bad-{}.json",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut board = crate::store::Board::new(crate::schema::Schema::default(), path);
        board.openshell = Some(crate::openshell::OpenShell::mock(
            |_| crate::openshell::Output {
                code: 1,
                stdout: String::new(),
                stderr: "gateway unreachable".into(),
            },
            std::time::Duration::from_secs(5),
        ));
        let b = std::sync::Arc::new(board);
        let Json(st) = openshell_status(AxState(b.clone())).await;
        assert!(!st.healthy);
        assert!(!st.not_configured);
    }

    #[tokio::test]
    async fn openshell_put_seals_mtls_and_never_echoes_pems() {
        let dir = std::env::temp_dir().join(format!(
            "sandboard-test-api-os-mtls-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::create_dir_all(&dir);
        let key_path = dir.join("master.key");
        // Share the secrets test lock — SANDBOARD_MASTER_KEY* is process-global.
        let _env = crate::secrets::master_key_env::Guard::with_key_path(&key_path);

        let path = dir.join("board.json");
        let b = std::sync::Arc::new(crate::store::Board::new(
            crate::schema::Schema::default(),
            path,
        ));
        let Json(saved) = put_openshell(
            AxState(b.clone()),
            Json(OpenShellSettings {
                gateway_endpoint: Some("https://127.0.0.1:17670".into()),
                auth_mode: Some(crate::model::OpenShellAuthMode::Mtls),
                ca_pem: Some("-----BEGIN CERTIFICATE-----\nCA\n-----END CERTIFICATE-----\n".into()),
                client_cert_pem: Some(
                    "-----BEGIN CERTIFICATE-----\nCERT\n-----END CERTIFICATE-----\n".into(),
                ),
                client_key_pem: Some(
                    "-----BEGIN PRIVATE KEY-----\nKEY\n-----END PRIVATE KEY-----\n".into(),
                ),
                ..Default::default()
            }),
        )
        .await
        .expect("put mtls");
        assert_eq!(
            saved.gateway_endpoint.as_deref(),
            Some("https://127.0.0.1:17670")
        );
        assert!(saved.mtls.complete);
        assert!(saved.ca_pem.is_none());
        assert!(saved.client_cert_pem.is_none());
        assert!(saved.client_key_pem.is_none());
        let sealed = b.openshell_mtls_sealed().expect("sealed stored");
        assert!(!sealed.contains("BEGIN"));
        let Json(got) = get_openshell(AxState(b.clone())).await;
        assert!(got.mtls.complete);
        assert!(got.ca_pem.is_none());

        drop(_env);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn github_app_put_seals_and_never_echoes_secrets() {
        let dir = std::env::temp_dir().join(format!(
            "sandboard-test-api-gh-app-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::create_dir_all(&dir);
        let key_path = dir.join("master.key");
        let _env = crate::secrets::master_key_env::Guard::with_key_path(&key_path);

        let path = dir.join("board.json");
        let b = std::sync::Arc::new(crate::store::Board::new(
            crate::schema::Schema::default(),
            path,
        ));
        let Json(saved) = put_github_app(
            AxState(b.clone()),
            Json(GitHubAppSettings {
                app_id: Some("424242".into()),
                client_id: Some("Iv1.test".into()),
                private_key_pem: Some(
                    "-----BEGIN RSA PRIVATE KEY-----\nKEY\n-----END RSA PRIVATE KEY-----\n".into(),
                ),
                webhook_secret: Some("whsec_never_echo".into()),
                client_secret: Some("cs_never_echo".into()),
                installation_id: Some(7777),
                ..Default::default()
            }),
        )
        .await
        .expect("put github app");
        assert_eq!(saved.app_id.as_deref(), Some("424242"));
        assert_eq!(saved.client_id.as_deref(), Some("Iv1.test"));
        assert_eq!(saved.installation_id, Some(7777));
        assert!(saved.status.complete);
        assert!(saved.status.webhook_secret);
        assert!(saved.private_key_pem.is_none());
        assert!(saved.webhook_secret.is_none());
        assert!(saved.client_secret.is_none());
        assert_eq!(b.github_app_installation_id(), Some(7777));
        let p = b
            .openshell_providers()
            .into_iter()
            .find(|p| p.name == crate::github_app::PROVIDER_NAME)
            .expect("github-app provider row");
        let sealed = p.credentials_sealed.as_deref().expect("sealed on provider");
        assert!(!sealed.contains("BEGIN"));
        assert!(!sealed.contains("whsec_never_echo"));
        assert!(b.github_app_sealed().is_none());
        let Json(got) = get_github_app(AxState(b.clone())).await;
        assert_eq!(got.app_id.as_deref(), Some("424242"));
        assert_eq!(got.installation_id, Some(7777));
        assert!(got.private_key_pem.is_none());

        let Json(cleared_inst) = put_github_app(
            AxState(b.clone()),
            Json(GitHubAppSettings {
                clear_installation_id: true,
                ..Default::default()
            }),
        )
        .await
        .expect("clear installation");
        assert!(cleared_inst.installation_id.is_none());
        assert!(b.github_app_installation_id().is_none());

        let Json(cleared) = put_github_app(
            AxState(b.clone()),
            Json(GitHubAppSettings {
                clear: true,
                ..Default::default()
            }),
        )
        .await
        .expect("clear");
        assert!(!cleared.status.complete);
        assert!(!b.github_app_status().complete);

        drop(_env);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn github_repo_access_view_groups_repos_under_installations() {
        let path = std::env::temp_dir().join(format!(
            "sandboard-test-api-repo-access-{}.json",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let b = std::sync::Arc::new(crate::store::Board::new(
            crate::schema::Schema::default(),
            path,
        ));
        let now = chrono::Utc::now();
        let mut cache = crate::github_app::GitHubRepoAccessCache {
            refreshed_at: Some(now),
            last_error: None,
            installations: vec![crate::github_app::InstallationInfo {
                id: 99,
                account_login: "acme".into(),
                account_type: "Organization".into(),
            }],
            repos: Default::default(),
        };
        cache.repos.insert(
            "acme/widgets".into(),
            crate::github_app::GitHubRepoAccessEntry {
                installation_id: 99,
                permissions: {
                    let mut m = BTreeMap::new();
                    m.insert("push".into(), "true".into());
                    m
                },
                last_seen_at: now,
            },
        );
        b.set_github_repo_access_cache(cache);
        b.set_github_app_installation_id(Some(99));
        let view = github_repo_access_view(&b);
        assert_eq!(view.install_url, crate::github_app::INSTALLATIONS_MANAGE_URL);
        assert_eq!(view.token_installation_id, Some(99));
        assert_eq!(view.installations.len(), 1);
        assert_eq!(view.installations[0].account_login, "acme");
        assert_eq!(
            view.installations[0].manage_url,
            "https://github.com/organizations/acme/settings/installations/99"
        );
        assert_eq!(view.installations[0].repos.len(), 1);
        assert_eq!(view.installations[0].repos[0].full_name, "acme/widgets");
        assert_eq!(view.installations[0].repos[0].installation_id, 99);
    }

    #[tokio::test]
    async fn openshell_providers_create_list_never_echo_secrets_and_sync_delete() {
        let dir = std::env::temp_dir().join(format!(
            "sandboard-test-api-os-providers-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::create_dir_all(&dir);
        let key_path = dir.join("master.key");
        let _env = crate::secrets::master_key_env::Guard::with_key_path(&key_path);

        let path = dir.join("board.json");
        let b = std::sync::Arc::new(crate::store::Board::new(
            crate::schema::Schema::default(),
            path,
        ));

        let mut creds = BTreeMap::new();
        creds.insert("GITHUB_TOKEN".into(), "ghp_never_echo_me".into());
        let Json(created) = create_openshell_provider(
            AxState(b.clone()),
            Json(OpenShellProviderWrite {
                name: "gh-clankr".into(),
                provider_type: "github".into(),
                config: BTreeMap::new(),
                credentials: Some(creds),
                refresh: None,
            }),
        )
        .await
        .expect("create provider");
        assert_eq!(created.name, "gh-clankr");
        assert_eq!(created.provider_type, "github");
        assert!(created.has_credentials);
        assert_eq!(created.credential_keys, vec!["GITHUB_TOKEN".to_string()]);
        let created_json = serde_json::to_string(&created).expect("json");
        assert!(
            !created_json.contains("ghp_never_echo_me"),
            "create must not echo secrets: {created_json}"
        );

        let Json(listed) = list_openshell_providers(AxState(b.clone())).await;
        assert_eq!(listed.providers.len(), 1);
        let listed_json = serde_json::to_string(&listed).expect("list json");
        assert!(!listed_json.contains("ghp_never_echo_me"));
        assert!(!listed.gateway_reachable);

        let Json(synced) = sync_openshell_providers(AxState(b.clone())).await;
        // Gateway not configured — sync records an error but desired state remains.
        assert!(synced.applied.is_empty());
        assert_eq!(synced.errors.len(), 1);
        assert_eq!(synced.errors[0].name, "gh-clankr");

        assert_eq!(
            delete_openshell_provider(AxState(b.clone()), Path("gh-clankr".into()))
                .await
                .expect("delete"),
            StatusCode::NO_CONTENT
        );
        let Json(after) = list_openshell_providers(AxState(b.clone())).await;
        assert!(after.providers.is_empty());

        drop(_env);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn agent_runtime_get_put_persists_and_overlays_effective_agents() {
        let path = std::env::temp_dir().join(format!(
            "sandboard-test-api-agent-rt-{}.json",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut schema = crate::schema::Schema::default();
        schema.execution.agents.engine = "cursor".into();
        let b = std::sync::Arc::new(crate::store::Board::new(schema, path));

        let Json(seeded) = get_agent_runtime(AxState(b.clone())).await;
        assert_eq!(seeded.engine, "cursor");

        let Json(saved) = put_agent_runtime(
            AxState(b.clone()),
            Json(crate::model::AgentRuntimeConfig {
                engine: "agy".into(),
                max_concurrent: 1,
                agent_timeout_secs: 600,
                max_attempts: 2,
                ..Default::default()
            }),
        )
        .await;
        assert_eq!(saved.engine, "agy");
        assert_eq!(saved.max_concurrent, 1);

        let Json(again) = get_agent_runtime(AxState(b.clone())).await;
        assert_eq!(again, saved);

        let effective = b.effective_agents();
        assert_eq!(effective.engine, "agy");
        assert_eq!(effective.agent_timeout_secs, 600);
    }

    /// Board with sealed admin auth (session + JWT mint). Holds master-key env.
    fn board_with_admin_auth(
        label: &str,
        openshell: Option<crate::openshell::OpenShell>,
    ) -> (SharedBoard, crate::secrets::master_key_env::Guard) {
        use crate::secrets::{seal_auth, AuthBundle};
        use base64::Engine;
        let hex = "cd".repeat(32);
        let env = crate::secrets::master_key_env::Guard::with_hex_key(&hex);
        let path = std::env::temp_dir().join(format!(
            "sandboard-test-api-{label}-{}.json",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut board = crate::store::Board::new(crate::schema::Schema::default(), path);
        board.openshell = openshell;
        let board = std::sync::Arc::new(board);
        let bundle = AuthBundle {
            admin_username: "admin".into(),
            password_hash: "unused-for-mcp-cred-tests".into(),
            session_key_b64: base64::engine::general_purpose::STANDARD.encode([9u8; 32]),
        };
        let sealed = seal_auth(&bundle).expect("seal");
        board.set_auth_sealed(Some(sealed));
        (board, env)
    }

    fn admin_jar(board: &SharedBoard) -> CookieJar {
        use crate::auth::{mint_session_cookie_value, SessionKind, SessionUser};
        use axum_extra::extract::cookie::Cookie;
        let value = mint_session_cookie_value(
            board,
            &SessionUser {
                kind: SessionKind::Admin,
                login: "admin".into(),
            },
        )
        .expect("mint session");
        CookieJar::new().add(Cookie::new("sandboard_session", value))
    }

    #[tokio::test]
    async fn mcp_cred_refuses_without_session_cookie() {
        let (b, _env) = board_with_admin_auth("mcp-cred-nosess", None);
        let err = provision_cockpit_mcp_cred(AxState(b), CookieJar::new())
            .await
            .expect_err("auth required");
        assert!(err.0.contains("authentication"), "error={}", err.0);
    }

    #[tokio::test]
    async fn mcp_cred_refuses_without_cockpit_session() {
        let (b, _env) = board_with_admin_auth("mcp-cred-nocockpit", None);
        let err = provision_cockpit_mcp_cred(AxState(b.clone()), admin_jar(&b))
            .await
            .expect_err("no cockpit session");
        assert!(err.0.contains("no cockpit session"), "error={}", err.0);
    }

    #[tokio::test]
    async fn mcp_cred_refuses_without_environment() {
        let (b, _env) = board_with_admin_auth("mcp-cred-noenv", None);
        b.create_cockpit_session(None, None).expect("create");
        let err = provision_cockpit_mcp_cred(AxState(b.clone()), admin_jar(&b))
            .await
            .expect_err("no environment");
        assert!(err.0.contains("no environment"), "error={}", err.0);
    }

    #[tokio::test]
    async fn mcp_cred_refuses_when_parked() {
        let (b, _env) = board_with_admin_auth("mcp-cred-parked", None);
        b.create_cockpit_session(Some("sandboard-cockpit".into()), None)
            .expect("create");
        b.park_cockpit_session().expect("park");
        let err = provision_cockpit_mcp_cred(AxState(b.clone()), admin_jar(&b))
            .await
            .expect_err("not Running");
        assert!(err.0.contains("not Running"), "error={}", err.0);
    }

    #[tokio::test]
    async fn mcp_cred_succeeds_with_cookie_and_mock_openshell() {
        let os = crate::openshell::OpenShell::mock(
            |args| {
                // Cockpit MCP relay readiness probe (`test -S <sock> && echo
                // LISTEN`) — everything else is a no-op success.
                let stdout = if args.iter().any(|a| a.contains("agent.sock")) {
                    "LISTEN\n".to_string()
                } else {
                    String::new()
                };
                crate::openshell::Output {
                    code: 0,
                    stdout,
                    stderr: String::new(),
                }
            },
            std::time::Duration::from_secs(5),
        );
        let (b, _env) = board_with_admin_auth("mcp-cred-ok", Some(os));
        b.create_cockpit_session(Some("sandboard-cockpit".into()), None)
            .expect("create");

        let Json(out) = provision_cockpit_mcp_cred(AxState(b.clone()), admin_jar(&b))
            .await
            .expect("provision");
        assert!(out.ok);
        assert!(out.injected);
        assert_eq!(out.environment, "sandboard-cockpit");
        assert_eq!(out.client_id, crate::mcp_oauth::COCKPIT_CLIENT_ID);
        assert_eq!(out.sub, "admin");
        assert!(!out.resource.is_empty());
        assert!(out.expires_at > 0);
    }

    #[tokio::test]
    async fn sync_imports_board_provider_types_before_create() {
        use crate::model::OpenShellProviderDesired;
        use crate::secrets::seal_string_map;
        use std::sync::Arc;

        let dir = std::env::temp_dir().join(format!(
            "sandboard-test-agy-sync-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::create_dir_all(&dir);
        let key_path = dir.join("master.key");
        let _env = crate::secrets::master_key_env::Guard::with_key_path(&key_path);

        let seen = Arc::new(parking_lot::Mutex::new(Vec::<Vec<String>>::new()));
        let seen_c = seen.clone();
        let os = crate::openshell::OpenShell::mock(
            move |args| {
                seen_c.lock().push(args.to_vec());
                let argv: Vec<&str> = args.iter().map(String::as_str).collect();
                if argv == ["status"] {
                    return crate::openshell::Output {
                        code: 0,
                        stdout: "Connected".into(),
                        stderr: String::new(),
                    };
                }
                if argv.starts_with(&["provider", "list-profiles"]) {
                    return crate::openshell::Output {
                        code: 0,
                        stdout: "[]".into(),
                        stderr: String::new(),
                    };
                }
                if argv.starts_with(&["provider", "profile", "import"]) {
                    return crate::openshell::Output {
                        code: 0,
                        stdout: String::new(),
                        stderr: String::new(),
                    };
                }
                if argv.starts_with(&["provider", "list"]) {
                    return crate::openshell::Output {
                        code: 0,
                        stdout: "[]".into(),
                        stderr: String::new(),
                    };
                }
                if argv.starts_with(&["provider", "create"]) {
                    return crate::openshell::Output {
                        code: 0,
                        stdout: String::new(),
                        stderr: String::new(),
                    };
                }
                if argv.starts_with(&["sandbox", "provider", "attach"]) {
                    return crate::openshell::Output {
                        code: 0,
                        stdout: String::new(),
                        stderr: String::new(),
                    };
                }
                crate::openshell::Output {
                    code: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                }
            },
            std::time::Duration::from_secs(5),
        );

        let path = dir.join("board.json");
        let mut board = crate::store::Board::new(crate::schema::Schema::default(), path);
        board.openshell = Some(os);
        let b = std::sync::Arc::new(board);
        // Boot seeds shipped types; Board::new alone does not.
        assert!(crate::provider_types::ensure_shipped_on_board(&b) >= 1);

        let mut creds = BTreeMap::new();
        creds.insert("ANTIGRAVITY_ACCESS_TOKEN".into(), "openshell:resolve:test".into());
        let sealed = seal_string_map(&creds).expect("seal");
        b.upsert_openshell_provider(
            OpenShellProviderDesired {
                name: "antigravity".into(),
                provider_type: "antigravity".into(),
                config: BTreeMap::new(),
                credentials_sealed: Some(sealed),
                credential_keys: vec!["ANTIGRAVITY_ACCESS_TOKEN".into()],
                refresh: None,
            }
            .normalized(),
        );

        let Json(synced) = sync_openshell_providers(AxState(b.clone())).await;
        assert!(
            synced.errors.is_empty(),
            "unexpected sync errors: {:?}",
            synced.errors
        );
        assert!(synced.applied.iter().any(|n| n == "antigravity"));

        let calls = seen.lock().clone();
        let import_idx = calls
            .iter()
            .position(|a| a.windows(3).any(|w| w == ["provider", "profile", "import"]))
            .expect("import before create");
        let create_idx = calls
            .iter()
            .position(|a| a.windows(2).any(|w| w == ["provider", "create"]))
            .expect("provider create");
        assert!(
            import_idx < create_idx,
            "import must precede create: {calls:?}"
        );
        assert!(
            calls.iter().any(|a| {
                a.windows(4).any(|w| {
                    w == ["provider", "profile", "import", "antigravity"]
                        || w == ["provider", "profile", "import", "cursor-agent"]
                })
            }),
            "sync should import shipped board types: {calls:?}"
        );
    }

    /// Operator helper (not a unit test): seal a Vertex credential into board
    /// `vertex` and apply to the gateway. Stop the running sandboard process first so
    /// flush is not overwritten by in-memory state.
    ///
    /// The credential file is named explicitly. sandboard does not guess at host
    /// config locations, and a helper that reaches into `~/.config` teaches the
    /// habit back into the product.
    ///
    /// ```bash
    /// SANDBOARD_TEST_VERTEX_ADC=~/.config/gcloud/application_default_credentials.json \
    ///   cargo test --offline upsert_live_vertex_provider -- --ignored --nocapture
    /// ```
    #[tokio::test]
    #[ignore = "writes the live board DB + gateway"]
    async fn upsert_live_vertex_provider() {
        use crate::db::DurableBoardStore;
        use crate::model::{OpenShellProviderDesired, OpenShellProviderRefreshDesired};
        use crate::secrets::seal_string_map;
        use crate::store::Board;
        use std::sync::Arc;

        let adc_path = std::env::var("SANDBOARD_TEST_VERTEX_ADC")
            .expect("set SANDBOARD_TEST_VERTEX_ADC to a credential JSON path");
        let adc: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&adc_path).expect("read ADC"))
                .expect("parse ADC");
        assert_eq!(adc["type"], "authorized_user");

        let mut material = BTreeMap::new();
        material.insert(
            "client_id".into(),
            adc["client_id"].as_str().expect("client_id").into(),
        );
        material.insert(
            "client_secret".into(),
            adc["client_secret"].as_str().expect("client_secret").into(),
        );
        material.insert(
            "refresh_token".into(),
            adc["refresh_token"].as_str().expect("refresh_token").into(),
        );
        let material_sealed = seal_string_map(&material).expect("seal refresh");

        let project = std::env::var("ANTHROPIC_VERTEX_PROJECT_ID")
            .unwrap_or_else(|_| "itpc-gcp-hcm-pe-eng-claude".into());
        let region = std::env::var("VERTEX_AI_REGION").unwrap_or_else(|_| "global".into());
        let mut config = BTreeMap::new();
        config.insert("VERTEX_AI_PROJECT_ID".into(), project.clone());
        config.insert("VERTEX_AI_REGION".into(), region.clone());
        config.insert("VERTEX_AI_LOCATION".into(), region.clone());

        let desired = OpenShellProviderDesired {
            name: "vertex".into(),
            provider_type: "google-vertex-ai".into(),
            config,
            credentials_sealed: None,
            credential_keys: vec!["GOOGLE_VERTEX_AI_TOKEN".into()],
            refresh: Some(OpenShellProviderRefreshDesired {
                credential_key: "GOOGLE_VERTEX_AI_TOKEN".into(),
                strategy: "oauth2_refresh_token".into(),
                material_sealed,
                secret_material_keys: vec!["client_secret".into(), "refresh_token".into()],
            }),
        }
        .normalized();

        let mut schema = crate::schema::Schema::default();
        crate::db::apply_database_url_override(&mut schema.board.database);
        let url = schema.board.database.parsed().expect("database url");
        let store = Arc::new(
            DurableBoardStore::connect(url.as_str())
                .await
                .expect("open board db"),
        );
        let board: SharedBoard = Arc::new(
            Board::load_with_store(schema, std::path::PathBuf::from("sandboard.json"), store)
                .await
                .expect("load board"),
        );

        let stored = board.upsert_openshell_provider(desired);
        assert!(stored.refresh.is_some());
        apply_desired_to_gateway(&board, &stored)
            .await
            .expect("gateway apply");
        board.flush();
        eprintln!(
            "upserted board+gateway provider vertex (project={project} region={region})"
        );
    }

}
