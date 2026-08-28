//! Outbound MCP OAuth client — host-mediated connect for catalog HTTP MCP servers.
//!
//! Distinct from [`crate::mcp_oauth`] (sandboard as AS/RS for `/mcp`). Here sandboard is the
//! OAuth **client**: discover PRM → AS → DCR → auth code + PKCE, then park tokens
//! on an OpenShell provider (`oauth2_refresh_token`) so the gateway refreshes
//! mid-flight and rewrites `openshell:resolve:…` placeholders on egress.

use crate::model::{
    McpAudience, McpHttpAuth, McpServerDesired, McpTransport, OpenShellProviderDesired,
    OpenShellProviderRefreshDesired, OpenShellProviderTypeDesired,
};
use crate::provider_types;
use crate::secrets::{open_string_map, seal_string_map};
use crate::store::SharedBoard;

use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine;
use parking_lot::Mutex;
use rand::Rng;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

const CALLBACK_PATH: &str = "/oauth/mcp-client/callback";
const PENDING_TTL_SECS: u64 = 600;

fn pending() -> &'static Mutex<HashMap<String, PendingOAuth>> {
    static STORE: OnceLock<Mutex<HashMap<String, PendingOAuth>>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(HashMap::new()))
}

#[derive(Clone)]
struct PendingOAuth {
    server_id: String,
    name: String,
    mcp_url: String,
    code_verifier: String,
    client_id: String,
    token_endpoint: String,
    /// Exact redirect_uri used at authorize — must match token exchange.
    redirect_uri: String,
    resource: String,
    scopes: Vec<String>,
    mcp_hosts: Vec<String>,
    created_at: u64,
    return_path: String,
}

/// Stable OpenShell provider instance name for an MCP catalog id.
pub fn provider_name_for(server_id: &str) -> String {
    format!("mcp-{server_id}")
}

/// Stable OpenShell provider type id for an MCP catalog id.
pub fn provider_type_id_for(server_id: &str) -> String {
    format!("mcp-oauth-{server_id}")
}

/// Credential env injected into sandboxes (unique per server).
pub fn access_token_env_for(server_id: &str) -> String {
    let slug = env_slug(server_id);
    format!("MCP_OAUTH_{slug}_ACCESS_TOKEN")
}

fn env_slug(server_id: &str) -> String {
    let mut out = String::with_capacity(server_id.len());
    for c in server_id.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_uppercase());
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        out.push_str("SERVER");
    }
    out
}

pub fn api_routes() -> Router<SharedBoard> {
    Router::new()
        .route("/discover", post(oauth_discover))
        .route("/start", post(oauth_start))
        .route("/disconnect", post(oauth_disconnect))
}

/// Callback lives under `/oauth/mcp-client/…` (not `/api`) so DCR redirect_uris stay short.
pub fn callback_routes() -> Router<SharedBoard> {
    Router::new().route("/callback", get(oauth_callback))
}

#[derive(Debug, Deserialize)]
pub struct OAuthDiscoverReq {
    pub url: String,
}

#[derive(Debug, Serialize)]
pub struct OAuthDiscoverOut {
    /// True when RFC 9728 PRM + AS metadata (+ DCR endpoint) resolve for this URL.
    pub supported: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct OAuthStartReq {
    pub url: String,
    #[serde(default)]
    pub server_id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub return_path: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct OAuthStartOut {
    pub authorize_url: String,
    pub server_id: String,
}

#[derive(Debug, Deserialize)]
pub struct OAuthDisconnectReq {
    pub server_id: String,
}

type ApiErr = (StatusCode, Json<serde_json::Value>);

fn api_err(status: StatusCode, msg: impl Into<String>) -> ApiErr {
    (status, Json(serde_json::json!({ "error": msg.into() })))
}

/// Probe whether an MCP HTTP URL advertises OAuth (WWW-Authenticate /
/// `/.well-known/oauth-protected-resource…` → AS metadata with DCR).
///
/// Soft-fails: always 200 with `supported: false` when discovery cannot complete.
async fn oauth_discover(Json(req): Json<OAuthDiscoverReq>) -> Result<Json<OAuthDiscoverOut>, ApiErr> {
    let mcp_url = req.url.trim().trim_end_matches('/').to_string();
    if mcp_url.is_empty() {
        return Err(api_err(StatusCode::BAD_REQUEST, "url required"));
    }
    let uri: Uri = mcp_url
        .parse()
        .map_err(|_| api_err(StatusCode::BAD_REQUEST, "url is not a valid URI"))?;
    if uri.scheme_str() != Some("https") && uri.scheme_str() != Some("http") {
        return Err(api_err(StatusCode::BAD_REQUEST, "url must be http(s)"));
    }
    if uri.host().is_none() {
        return Err(api_err(StatusCode::BAD_REQUEST, "url missing host"));
    }
    match discover_mcp_oauth(&mcp_url).await {
        Ok(_) => Ok(Json(OAuthDiscoverOut {
            supported: true,
            error: None,
        })),
        Err(e) => Ok(Json(OAuthDiscoverOut {
            supported: false,
            error: Some(e),
        })),
    }
}

async fn oauth_start(
    State(board): State<SharedBoard>,
    headers: HeaderMap,
    Json(req): Json<OAuthStartReq>,
) -> Result<Json<OAuthStartOut>, ApiErr> {
    let mcp_url = req.url.trim().trim_end_matches('/').to_string();
    if mcp_url.is_empty() {
        return Err(api_err(StatusCode::BAD_REQUEST, "url required"));
    }
    let uri: Uri = mcp_url
        .parse()
        .map_err(|_| api_err(StatusCode::BAD_REQUEST, "url is not a valid URI"))?;
    if uri.scheme_str() != Some("https") && uri.scheme_str() != Some("http") {
        return Err(api_err(StatusCode::BAD_REQUEST, "url must be http(s)"));
    }
    let host = uri
        .host()
        .ok_or_else(|| api_err(StatusCode::BAD_REQUEST, "url missing host"))?
        .to_string();

    let server_id = match req
        .server_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(id) => sanitize_server_id(id).map_err(|(_, m)| api_err(StatusCode::BAD_REQUEST, m))?,
        None => {
            let base = crate::model::slugify_sandbox_profile_id(
                req.name.as_deref().unwrap_or("mcp-oauth"),
            );
            let existing = board.list_mcp_servers();
            let mut id = base.clone();
            let mut n = 2u32;
            while existing.iter().any(|s| s.id == id) {
                id = format!("{base}-{n}");
                n += 1;
            }
            id
        }
    };
    let name = req
        .name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(&server_id)
        .to_string();
    let return_path = sanitize_return_path(req.return_path.as_deref());

    let discovered = discover_mcp_oauth(&mcp_url)
        .await
        .map_err(|e| api_err(StatusCode::BAD_GATEWAY, e))?;

    let origin = crate::mcp_oauth::public_origin(&headers);
    if origin.is_empty() {
        return Err(api_err(
            StatusCode::BAD_REQUEST,
            "cannot resolve public origin (Host / Origin / X-Forwarded-Host, or SANDBOARD_PUBLIC_URL)",
        ));
    }
    let redirect_uri = format!("{}{CALLBACK_PATH}", origin.trim_end_matches('/'));

    let client_id = register_public_client(
        &discovered.registration_endpoint,
        &redirect_uri,
        &format!("sandboard mcp ({server_id})"),
    )
    .await
    .map_err(|e| api_err(StatusCode::BAD_GATEWAY, e))?;

    let code_verifier = pkce_verifier();
    let code_challenge = pkce_challenge_s256(&code_verifier);
    let state = random_token(32);

    let mut scopes = discovered.scopes_supported;
    if scopes.is_empty() {
        scopes = vec!["offline_access".into()];
    } else if !scopes.iter().any(|s| s == "offline_access") {
        scopes.push("offline_access".into());
    }

    let mut mcp_hosts = vec![host];
    if let Some(ashost) = discovered.as_host {
        if !mcp_hosts.iter().any(|h| h == &ashost) {
            mcp_hosts.push(ashost);
        }
    }

    {
        let mut st = pending().lock();
        st.retain(|_, p| now_secs().saturating_sub(p.created_at) < PENDING_TTL_SECS);
        st.insert(
            state.clone(),
            PendingOAuth {
                server_id: server_id.clone(),
                name,
                mcp_url: mcp_url.clone(),
                code_verifier,
                client_id: client_id.clone(),
                token_endpoint: discovered.token_endpoint.clone(),
                redirect_uri: redirect_uri.clone(),
                resource: discovered.resource.clone(),
                scopes: scopes.clone(),
                mcp_hosts,
                created_at: now_secs(),
                return_path,
            },
        );
    }

    let scope = scopes.join(" ");
    let authorize_url = format!(
        "{}?response_type=code&client_id={}&redirect_uri={}&state={}&code_challenge={}&code_challenge_method=S256&scope={}&resource={}",
        discovered.authorization_endpoint,
        urlencoding(&client_id),
        urlencoding(&redirect_uri),
        urlencoding(&state),
        urlencoding(&code_challenge),
        urlencoding(&scope),
        urlencoding(&discovered.resource),
    );

    Ok(Json(OAuthStartOut {
        authorize_url,
        server_id,
    }))
}

#[derive(Debug, Deserialize)]
struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

async fn oauth_callback(
    State(board): State<SharedBoard>,
    Query(q): Query<CallbackQuery>,
) -> Response {
    if let Some(err) = q.error.as_deref() {
        let desc = q.error_description.as_deref().unwrap_or(err);
        return Redirect::to(&format!(
            "/settings/mcp-servers?mcp_oauth=error&message={}",
            urlencoding(desc)
        ))
        .into_response();
    }
    let (Some(code), Some(state)) = (q.code.as_deref(), q.state.as_deref()) else {
        return (StatusCode::BAD_REQUEST, "missing code or state").into_response();
    };

    let pending_row = {
        let mut st = pending().lock();
        st.remove(state)
    };
    let Some(p) = pending_row else {
        return Redirect::to(
            "/settings/mcp-servers?mcp_oauth=error&message=expired_or_unknown_state",
        )
        .into_response();
    };

    let tokens = match exchange_code(
        &p.token_endpoint,
        &p.client_id,
        code,
        &p.code_verifier,
        &p.redirect_uri,
        &p.resource,
    )
    .await
    {
        Ok(t) => t,
        Err(e) => {
            return Redirect::to(&format!(
                "{}{}mcp_oauth=error&message={}",
                p.return_path,
                if p.return_path.contains('?') {
                    "&"
                } else {
                    "?"
                },
                urlencoding(&e)
            ))
            .into_response();
        }
    };

    if let Err(e) = finish_oauth_connect(&board, &p, &tokens).await {
        return Redirect::to(&format!(
            "{}{}mcp_oauth=error&message={}",
            p.return_path,
            if p.return_path.contains('?') {
                "&"
            } else {
                "?"
            },
            urlencoding(&e)
        ))
        .into_response();
    }

    let sep = if p.return_path.contains('?') {
        "&"
    } else {
        "?"
    };
    Redirect::to(&format!(
        "{}{}mcp_oauth=ok&id={}",
        p.return_path,
        sep,
        urlencoding(&p.server_id)
    ))
    .into_response()
}

async fn oauth_disconnect(
    State(board): State<SharedBoard>,
    Json(req): Json<OAuthDisconnectReq>,
) -> Result<Json<serde_json::Value>, ApiErr> {
    let server_id = sanitize_server_id(req.server_id.trim())
        .map_err(|(_, m)| api_err(StatusCode::BAD_REQUEST, m))?;
    disconnect_oauth(&board, &server_id)
        .await
        .map_err(|e| api_err(StatusCode::BAD_REQUEST, e))?;
    Ok(Json(serde_json::json!({ "ok": true, "server_id": server_id })))
}

/// Tear down provider + type and clear MCP oauth auth (best-effort gateway).
pub async fn disconnect_oauth(board: &SharedBoard, server_id: &str) -> Result<(), String> {
    let pname = provider_name_for(server_id);
    let tid = provider_type_id_for(server_id);

    if let Some(mut server) = board.get_mcp_server(server_id) {
        if let McpTransport::Http { auth, .. } = &mut server.transport {
            *auth = McpHttpAuth::None;
        }
        server.provider_names.retain(|n| n != &pname);
        board.upsert_mcp_server(server)?;
    }

    let _ = board.delete_openshell_provider(&pname);
    let _ = board.delete_openshell_provider_type(&tid);
    let os = board.openshell_client();
    let _ = os.delete_provider(&pname).await;
    let _ = os.delete_provider_type(&tid).await;
    Ok(())
}

/// Called from MCP server delete to drop linked OAuth provider resources.
pub async fn cleanup_for_deleted_server(board: &SharedBoard, server_id: &str) {
    let pname = provider_name_for(server_id);
    let tid = provider_type_id_for(server_id);
    let _ = board.delete_openshell_provider(&pname);
    let _ = board.delete_openshell_provider_type(&tid);
    let os = board.openshell_client();
    let _ = os.delete_provider(&pname).await;
    let _ = os.delete_provider_type(&tid).await;
}

async fn finish_oauth_connect(
    board: &SharedBoard,
    p: &PendingOAuth,
    tokens: &TokenResponse,
) -> Result<(), String> {
    let access = tokens
        .access_token
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "token response missing access_token".to_string())?;
    let refresh = tokens
        .refresh_token
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "token response missing refresh_token (need offline_access)".to_string())?;

    let env_key = access_token_env_for(&p.server_id);
    let type_id = provider_type_id_for(&p.server_id);
    let pname = provider_name_for(&p.server_id);

    // Prefer a small refresh scope set — PRM can advertise dozens; the gateway
    // only needs offline_access (+ whatever the AS requires) for refresh.
    let refresh_scopes: Vec<String> = {
        let mut s: Vec<String> = p
            .scopes
            .iter()
            .filter(|x| {
                *x == "offline_access" || x.starts_with("read:me") || x.starts_with("read:account")
            })
            .cloned()
            .collect();
        if s.is_empty() {
            s = vec!["offline_access".into()];
        }
        s
    };

    let yaml = render_provider_type_yaml(
        &type_id,
        &p.name,
        &env_key,
        &p.token_endpoint,
        &p.mcp_hosts,
        &refresh_scopes,
    );
    provider_types::parse_provider_type_yaml(&yaml, Some(&type_id))?;
    board.upsert_openshell_provider_type(OpenShellProviderTypeDesired {
        id: type_id.clone(),
        yaml: yaml.clone(),
        shipped: false,
        form_config_keys: vec![],
    })?;

    let os = board.openshell_client();
    // Import must succeed before configure_refresh — otherwise token_url is empty
    // and mid-flight mint fails with a misleading client_credentials error.
    os.upsert_provider_type_yaml(&type_id, &yaml)
        .await
        .map_err(|e| format!("import provider type {type_id}: {e}"))?;

    let mut creds = BTreeMap::new();
    creds.insert(env_key.clone(), access.to_string());
    let credentials_sealed = seal_string_map(&creds).map_err(|e| e.to_string())?;
    let credential_keys = vec![env_key.clone()];

    let mut material = BTreeMap::new();
    material.insert("client_id".into(), p.client_id.clone());
    material.insert("refresh_token".into(), refresh.to_string());
    let material_sealed = seal_string_map(&material).map_err(|e| e.to_string())?;
    let refresh_desired = OpenShellProviderRefreshDesired {
        credential_key: env_key.clone(),
        strategy: "oauth2_refresh_token".into(),
        material_sealed,
        secret_material_keys: vec!["refresh_token".into()],
    };

    let desired = OpenShellProviderDesired {
        name: pname.clone(),
        provider_type: type_id,
        config: BTreeMap::new(),
        credentials_sealed: Some(credentials_sealed),
        credential_keys,
        refresh: Some(refresh_desired),
    }
    .normalized();
    let stored = board.upsert_openshell_provider(desired);
    if let Err(e) = apply_provider_to_gateway(board, &stored).await {
        tracing::warn!(
            provider = %stored.name,
            error = %e,
            "mcp oauth provider saved; gateway apply failed"
        );
    }

    let mut provider_names = vec![pname.clone()];
    let existing = board.get_mcp_server(&p.server_id);
    if let Some(ref e) = existing {
        for n in &e.provider_names {
            if n != &pname && !provider_names.iter().any(|x| x == n) {
                provider_names.push(n.clone());
            }
        }
    }
    let audience = existing
        .as_ref()
        .map(|e| e.audience)
        .unwrap_or(McpAudience::Both);
    let env = existing
        .as_ref()
        .map(|e| e.env.clone())
        .unwrap_or_default();
    let name = existing
        .as_ref()
        .map(|e| e.name.clone())
        .unwrap_or_else(|| p.name.clone());
    // Always write egress for discovered hosts — providers_v2 profile merge is
    // not reliable enough alone (CONNECT 403 without these endpoints).
    let policy = Some(render_mcp_egress_fragment(&p.server_id, &p.mcp_hosts));

    board.upsert_mcp_server(McpServerDesired {
        id: p.server_id.clone(),
        name,
        transport: McpTransport::Http {
            url: p.mcp_url.clone(),
            auth: McpHttpAuth::OAuth {
                provider: pname,
                env: env_key,
            },
        },
        policy_fragment_yaml: policy,
        provider_names,
        env,
        audience,
        shipped: existing.as_ref().map(|e| e.shipped).unwrap_or(false),
    })?;
    Ok(())
}

/// Binaries that may call remote MCP hosts (agent CLIs + curl for probes).
/// OpenShell denies CONNECT when the rule's binary allowlist does not match
/// `/proc/self/exe` — omitting binaries is not "allow all".
const MCP_EGRESS_BINARIES: &[&str] = &[
    "/usr/bin/curl",
    "/usr/local/bin/curl",
    "/usr/local/bin/agent",
    "/usr/bin/agent",
    "/usr/local/bin/cursor-agent",
    "/opt/cursor-agent/versions/**/cursor-agent",
    "/opt/cursor-agent/versions/**/node",
    "/usr/bin/node",
    "/usr/local/bin/node",
    "/usr/local/bin/claude",
    "/usr/local/bin/agy",
    "/usr/bin/agy",
    "/usr/local/bin/opencode",
    "/opt/opencode/bin/opencode",
    "/bin/sh",
    "/usr/bin/sh",
    "/bin/bash",
    "/usr/bin/bash",
];

/// Network policy fragment so the sandbox can reach the MCP (+ AS) hosts.
pub fn render_mcp_egress_fragment(server_id: &str, hosts: &[String]) -> String {
    let policy_name = format!("mcp_oauth_{}", env_slug(server_id).to_ascii_lowercase());
    let mut endpoints = String::new();
    for host in hosts {
        endpoints.push_str(&format!(
            "      - host: {host}\n        port: 443\n        protocol: rest\n        access: full\n        enforcement: enforce\n"
        ));
    }
    let mut binaries = String::new();
    for path in MCP_EGRESS_BINARIES {
        binaries.push_str(&format!("      - {{ path: {path} }}\n"));
    }
    format!(
        "network_policies:\n  {policy_name}:\n    name: {policy_name}\n    endpoints:\n{endpoints}    binaries:\n{binaries}"
    )
}

async fn apply_provider_to_gateway(
    board: &SharedBoard,
    p: &OpenShellProviderDesired,
) -> Result<(), String> {
    let credentials = match p.credentials_sealed.as_deref() {
        None | Some("") => BTreeMap::new(),
        Some(s) => open_string_map(s).map_err(|e| format!("open credentials: {e}"))?,
    };
    let refresh = match &p.refresh {
        None => None,
        Some(r) => {
            let material = open_string_map(&r.material_sealed)
                .map_err(|e| format!("open refresh material: {e}"))?;
            Some(crate::openshell::ProviderRefreshSpec {
                credential_key: r.credential_key.clone(),
                strategy: r.strategy.clone(),
                material,
                secret_material_keys: r.secret_material_keys.clone(),
            })
        }
    };
    board
        .openshell_client()
        .apply_provider(
            &p.name,
            &p.provider_type,
            credentials,
            p.config.clone(),
            refresh.as_ref(),
        )
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

// --- discovery / token -------------------------------------------------------

#[derive(Debug)]
struct Discovered {
    resource: String,
    authorization_endpoint: String,
    token_endpoint: String,
    registration_endpoint: String,
    scopes_supported: Vec<String>,
    as_host: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ProtectedResourceMeta {
    resource: Option<String>,
    authorization_servers: Option<Vec<String>>,
    scopes_supported: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct AsMetadata {
    authorization_endpoint: Option<String>,
    token_endpoint: Option<String>,
    registration_endpoint: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DcrResponse {
    client_id: String,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
    #[allow(dead_code)]
    expires_in: Option<u64>,
    #[allow(dead_code)]
    token_type: Option<String>,
}

async fn discover_mcp_oauth(mcp_url: &str) -> Result<Discovered, String> {
    let client = http_client()?;
    let prm_url = resolve_prm_url(&client, mcp_url).await?;
    let prm: ProtectedResourceMeta = client
        .get(&prm_url)
        .send()
        .await
        .map_err(|e| format!("fetch PRM: {e}"))?
        .error_for_status()
        .map_err(|e| format!("PRM status: {e}"))?
        .json()
        .await
        .map_err(|e| format!("PRM json: {e}"))?;

    let resource = prm
        .resource
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| mcp_url.to_string());
    let as_base = prm
        .authorization_servers
        .and_then(|v| v.into_iter().next())
        .ok_or_else(|| "PRM missing authorization_servers".to_string())?;
    let as_host = Uri::try_from(as_base.as_str())
        .ok()
        .and_then(|u| u.host().map(|h| h.to_string()));

    let as_meta_url = authorization_server_metadata_url(&as_base);
    let as_meta: AsMetadata = client
        .get(&as_meta_url)
        .send()
        .await
        .map_err(|e| format!("fetch AS metadata: {e}"))?
        .error_for_status()
        .map_err(|e| format!("AS metadata status: {e}"))?
        .json()
        .await
        .map_err(|e| format!("AS metadata json: {e}"))?;

    let authorization_endpoint = as_meta
        .authorization_endpoint
        .ok_or_else(|| "AS metadata missing authorization_endpoint".to_string())?;
    let token_endpoint = as_meta
        .token_endpoint
        .ok_or_else(|| "AS metadata missing token_endpoint".to_string())?;
    let registration_endpoint = as_meta
        .registration_endpoint
        .ok_or_else(|| "AS metadata missing registration_endpoint (DCR required)".to_string())?;

    Ok(Discovered {
        resource,
        authorization_endpoint,
        token_endpoint,
        registration_endpoint,
        scopes_supported: prm.scopes_supported.unwrap_or_default(),
        as_host,
    })
}

async fn resolve_prm_url(client: &reqwest::Client, mcp_url: &str) -> Result<String, String> {
    let resp = client
        .post(mcp_url)
        .header(header::ACCEPT, "application/json, text/event-stream")
        .header(header::CONTENT_TYPE, "application/json")
        .body("{}")
        .send()
        .await
        .map_err(|e| format!("probe MCP: {e}"))?;
    if let Some(www) = resp.headers().get(header::WWW_AUTHENTICATE) {
        if let Ok(s) = www.to_str() {
            if let Some(url) = parse_resource_metadata(s) {
                return Ok(url);
            }
        }
    }
    Ok(well_known_prm_fallback(mcp_url))
}

fn well_known_prm_fallback(mcp_url: &str) -> String {
    let uri = mcp_url.parse::<Uri>().ok();
    let Some(uri) = uri else {
        return format!(
            "{}/.well-known/oauth-protected-resource",
            mcp_url.trim_end_matches('/')
        );
    };
    let scheme = uri.scheme_str().unwrap_or("https");
    let authority = uri.authority().map(|a| a.as_str()).unwrap_or("");
    let path = uri.path().trim_end_matches('/');
    if path.is_empty() || path == "/" {
        format!("{scheme}://{authority}/.well-known/oauth-protected-resource")
    } else {
        format!("{scheme}://{authority}/.well-known/oauth-protected-resource{path}")
    }
}

/// Parse `resource_metadata="…"` from a WWW-Authenticate Bearer challenge.
pub fn parse_resource_metadata(www: &str) -> Option<String> {
    let lower = www.to_ascii_lowercase();
    let key = "resource_metadata=";
    let idx = lower.find(key)?;
    let rest = www[idx + key.len()..].trim_start();
    if let Some(stripped) = rest.strip_prefix('"') {
        let end = stripped.find('"')?;
        Some(stripped[..end].to_string())
    } else {
        let end = rest
            .find(|c: char| c == ',' || c.is_whitespace())
            .unwrap_or(rest.len());
        Some(rest[..end].trim_matches('"').to_string())
    }
}

fn authorization_server_metadata_url(as_issuer: &str) -> String {
    let base = as_issuer.trim_end_matches('/');
    format!("{base}/.well-known/oauth-authorization-server")
}

async fn register_public_client(
    registration_endpoint: &str,
    redirect_uri: &str,
    client_name: &str,
) -> Result<String, String> {
    let client = http_client()?;
    let body = serde_json::json!({
        "client_name": client_name,
        "redirect_uris": [redirect_uri],
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none",
    });
    let resp: DcrResponse = client
        .post(registration_endpoint)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("DCR request: {e}"))?
        .error_for_status()
        .map_err(|e| format!("DCR status: {e}"))?
        .json()
        .await
        .map_err(|e| format!("DCR json: {e}"))?;
    if resp.client_id.trim().is_empty() {
        return Err("DCR returned empty client_id".into());
    }
    Ok(resp.client_id)
}

async fn exchange_code(
    token_endpoint: &str,
    client_id: &str,
    code: &str,
    code_verifier: &str,
    redirect_uri: &str,
    resource: &str,
) -> Result<TokenResponse, String> {
    let client = http_client()?;
    let body = serde_urlencoded::to_string([
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", redirect_uri),
        ("client_id", client_id),
        ("code_verifier", code_verifier),
        ("resource", resource),
    ])
    .map_err(|e| format!("token encode: {e}"))?;
    let resp = client
        .post(token_endpoint)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .map_err(|e| format!("token request: {e}"))?;
    let status = resp.status();
    let text = resp.text().await.map_err(|e| format!("token body: {e}"))?;
    if !status.is_success() {
        return Err(format!("token endpoint {status}: {text}"));
    }
    serde_json::from_str(&text).map_err(|e| format!("token json: {e}"))
}

fn http_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| e.to_string())
}

// --- provider type YAML ------------------------------------------------------

/// Render a board OpenShell provider type for one OAuth MCP server.
pub fn render_provider_type_yaml(
    type_id: &str,
    display_name: &str,
    env_key: &str,
    token_url: &str,
    hosts: &[String],
    scopes: &[String],
) -> String {
    let mut endpoints = String::new();
    for host in hosts {
        endpoints.push_str(&format!(
            "  - host: {host}\n    port: 443\n    protocol: rest\n    access: full\n    enforcement: enforce\n"
        ));
    }
    if endpoints.is_empty() {
        endpoints.push_str(
            "  - host: example.invalid\n    port: 443\n    protocol: rest\n    access: full\n    enforcement: enforce\n",
        );
    }
    let scope_yaml = if scopes.is_empty() {
        String::new()
    } else {
        let items: Vec<String> = scopes.iter().map(|s| format!("\"{s}\"")).collect();
        format!("      scopes: [{}]\n", items.join(", "))
    };
    let safe_name = display_name.replace('"', "'");
    let binaries = render_provider_binaries_yaml();
    format!(
        r#"id: {type_id}
display_name: "MCP OAuth ({safe_name})"
description: "Host-mediated MCP OAuth; gateway refreshes access tokens"
category: agent
credentials:
  - name: access_token
    description: MCP OAuth access token
    env_vars:
      - {env_key}
    required: true
    auth_style: bearer
    header_name: authorization
    query_param: ""
    refresh:
      strategy: oauth2_refresh_token
      token_url: "{token_url}"
{scope_yaml}      refresh_before_seconds: 300
      max_lifetime_seconds: 3600
      material:
        - name: client_id
          description: OAuth client id from DCR
          required: true
        - name: refresh_token
          description: OAuth refresh token
          required: true
          secret: true
discovery:
  credentials:
    - access_token
endpoints:
{endpoints}binaries:
{binaries}inference_capable: false
"#
    )
}

fn render_provider_binaries_yaml() -> String {
    let mut binaries = String::new();
    for path in MCP_EGRESS_BINARIES {
        binaries.push_str(&format!("  - {path}\n"));
    }
    binaries
}

// --- misc --------------------------------------------------------------------

fn sanitize_server_id(id: &str) -> Result<String, (StatusCode, String)> {
    let id = id.trim();
    if id.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "server_id required".into()));
    }
    if id.len() > 64
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err((
            StatusCode::BAD_REQUEST,
            "server_id must be [A-Za-z0-9_-]{{1,64}}".into(),
        ));
    }
    Ok(id.to_string())
}

fn sanitize_return_path(raw: Option<&str>) -> String {
    let d = "/settings/mcp-servers";
    let Some(r) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return d.into();
    };
    if r.starts_with("/settings/") && !r.contains("://") && !r.contains('\n') {
        r.to_string()
    } else {
        d.into()
    }
}

fn pkce_verifier() -> String {
    random_token(64)
}

fn pkce_challenge_s256(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
}

fn random_token(nbytes: usize) -> String {
    let mut bytes = vec![0u8; nbytes];
    rand::rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn urlencoding(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.as_bytes() {
        match *b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_resource_metadata_quoted() {
        let www = r#"Bearer resource_metadata="https://mcp.atlassian.com/.well-known/oauth-protected-resource/v1/mcp/authv2", scope="read:me""#;
        assert_eq!(
            parse_resource_metadata(www).as_deref(),
            Some(
                "https://mcp.atlassian.com/.well-known/oauth-protected-resource/v1/mcp/authv2"
            )
        );
    }

    #[test]
    fn well_known_prm_inserts_path() {
        let u = well_known_prm_fallback("https://mcp.atlassian.com/v1/mcp/authv2");
        assert_eq!(
            u,
            "https://mcp.atlassian.com/.well-known/oauth-protected-resource/v1/mcp/authv2"
        );
    }

    #[test]
    fn provider_names_stable() {
        assert_eq!(provider_name_for("jira"), "mcp-jira");
        assert_eq!(provider_type_id_for("jira"), "mcp-oauth-jira");
        assert_eq!(
            access_token_env_for("my-jira"),
            "MCP_OAUTH_MY_JIRA_ACCESS_TOKEN"
        );
    }

    #[test]
    fn render_yaml_parses() {
        let yaml = render_provider_type_yaml(
            "mcp-oauth-demo",
            "Demo",
            "MCP_OAUTH_DEMO_ACCESS_TOKEN",
            "https://auth.example.com/oauth/token",
            &["mcp.example.com".into(), "auth.example.com".into()],
            &["offline_access".into(), "read:me".into()],
        );
        let parsed = provider_types::parse_provider_type_yaml(&yaml, Some("mcp-oauth-demo"))
            .expect("yaml");
        assert_eq!(parsed.id, "mcp-oauth-demo");
        assert!(parsed
            .credential_env_vars
            .iter()
            .any(|e| e == "MCP_OAUTH_DEMO_ACCESS_TOKEN"));
    }

    #[test]
    fn pkce_challenge_is_s256() {
        let v = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(
            pkce_challenge_s256(v),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn egress_fragment_lists_hosts() {
        let frag = render_mcp_egress_fragment(
            "jira",
            &["mcp.atlassian.com".into(), "auth.atlassian.com".into()],
        );
        assert!(frag.contains("mcp.atlassian.com"));
        assert!(frag.contains("auth.atlassian.com"));
        assert!(frag.contains("mcp_oauth_jira"));
        assert!(frag.contains("/usr/local/bin/agent"));
        assert!(frag.contains("binaries:"));
        crate::mcp_policy::merge_policy_fragments(
            "version: 1\nnetwork_policies: {}\n",
            [frag.as_str()],
        )
        .expect("merge");
    }
}
