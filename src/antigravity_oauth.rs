//! Host-mediated Google OAuth for the Antigravity (`agy`) OpenShell provider.
//!
//! Distinct from [`crate::mcp_client_oauth`] (outbound MCP servers). Here sandboard
//! is the OAuth client for Google's Antigravity **consumer** installed-app client:
//! open Google auth → paste the short authorization code from the hosted
//! callback page → pick a GCP project → seal refresh on board provider
//! `antigravity` → gateway `oauth2_refresh_token` so the seat only sees
//! `openshell:resolve:…`.
//!
//! Redirect is `https://antigravity.google/oauth-callback` (paste-code), not
//! loopback — so a remote board works from the operator's browser.
//!
//! The business / Cloud Code client (`1071006060591-…`) also accepts this
//! redirect, but `fetchAvailableModels` then returns Gemini Flash rows without
//! `vertexModelId` → cockpit dies with "Could not determine Vertex model ID".

use crate::antigravity::{self, CONFIG_LOCATION, CONFIG_PROJECT};
use crate::model::{
    OpenShellProviderDesired, OpenShellProviderRefreshDesired, OpenShellProviderTypeDesired,
    CockpitSessionStatus, ANTIGRAVITY_PROVIDER,
};
use crate::provider_types;
use crate::secrets::{open_string_map, seal_string_map};
use crate::store::SharedBoard;
use crate::supervisor::setup_agy_auth;

use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::routing::post;
use axum::{Json, Router};
use base64::Engine;
use parking_lot::Mutex;
use rand::Rng;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

const PENDING_TTL_SECS: u64 = 600;
const DEFAULT_RETURN_PATH: &str = "/settings/openshell/providers";

/// Consumer Antigravity client embedded in the CLI (`agy`). Installed-app
/// credential — not a sandboard secret. Prefer this over the business client so
/// seat `fetchAvailableModels` rows include `vertexModelId`.
const AGY_CLIENT_ID: &str =
    "884354919052-36trc1jjb3tguiac32ov6cod268c5blh.apps.googleusercontent.com";
/// Matching client secret from the same installed-app client (public in `agy`).
const AGY_CLIENT_SECRET: &str = "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";

/// Hosted page that displays the authorization code for paste-back.
const REDIRECT_URI: &str = "https://antigravity.google/oauth-callback";

const AGY_SCOPES: &[&str] = &[
    "https://www.googleapis.com/auth/cloud-platform",
    "https://www.googleapis.com/auth/userinfo.email",
    "https://www.googleapis.com/auth/userinfo.profile",
    "https://www.googleapis.com/auth/cclog",
    "https://www.googleapis.com/auth/experimentsandconfigs",
];

/// Standard Google OAuth authorize + hosted paste-code redirect.
/// (`auth.cloud.google/authorize` returns `invalid_client` for these ids.)
const AUTHORIZE_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const LIST_PROJECTS_URL: &str =
    "https://cloudcode-pa.googleapis.com/v1internal:listCloudAICompanionProjects";

/// Single in-flight login — the paste-code page returns only `code`, not `state`.
fn pending_slot() -> &'static Mutex<Option<PendingOAuth>> {
    static STORE: OnceLock<Mutex<Option<PendingOAuth>>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(None))
}

#[derive(Clone)]
struct PendingOAuth {
    code_verifier: String,
    created_at: u64,
}

pub fn api_routes() -> Router<SharedBoard> {
    Router::new()
        .route("/start", post(oauth_start))
        .route("/complete", post(oauth_complete))
        .route("/select-project", post(oauth_select_project))
        .route("/disconnect", post(oauth_disconnect))
}

/// No loopback callback — Cloud paste-code redirects to antigravity.google.
pub fn callback_routes() -> Router<SharedBoard> {
    Router::new()
}

#[derive(Debug, Deserialize)]
pub struct OAuthStartReq {
    #[serde(default)]
    pub return_path: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct OAuthStartOut {
    pub authorize_url: String,
    pub redirect_uri: String,
}

#[derive(Debug, Deserialize)]
pub struct OAuthCompleteReq {
    /// Short authorization code from the Google / Antigravity paste page.
    /// Also accepts a full redirect URL containing `code=` (tolerant).
    pub authorization_code: String,
}

#[derive(Debug, Serialize)]
pub struct CloudProject {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct OAuthCompleteOut {
    pub ok: bool,
    pub projects: Vec<CloudProject>,
    pub needs_project: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selected_project: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct OAuthSelectProjectReq {
    pub project_id: String,
}

type ApiErr = (StatusCode, Json<serde_json::Value>);

fn api_err(status: StatusCode, msg: impl Into<String>) -> ApiErr {
    (status, Json(serde_json::json!({ "error": msg.into() })))
}

async fn oauth_start(
    Json(req): Json<OAuthStartReq>,
) -> Result<Json<OAuthStartOut>, ApiErr> {
    let _ = sanitize_return_path(req.return_path.as_deref());

    let code_verifier = pkce_verifier();
    let code_challenge = pkce_challenge_s256(&code_verifier);
    let state = random_token(32);

    {
        let mut slot = pending_slot().lock();
        *slot = Some(PendingOAuth {
            code_verifier,
            created_at: now_secs(),
        });
    }

    let scope = AGY_SCOPES.join(" ");
    // `state` is sent for Google's CSRF check; paste-complete does not echo it back.
    let authorize_url = format!(
        "{AUTHORIZE_URL}?response_type=code&client_id={}&redirect_uri={}&state={}&code_challenge={}&code_challenge_method=S256&scope={}&access_type=offline&prompt=consent",
        urlencoding(AGY_CLIENT_ID),
        urlencoding(REDIRECT_URI),
        urlencoding(&state),
        urlencoding(&code_challenge),
        urlencoding(&scope),
    );
    Ok(Json(OAuthStartOut {
        authorize_url,
        redirect_uri: REDIRECT_URI.into(),
    }))
}

async fn oauth_complete(
    State(board): State<SharedBoard>,
    Json(req): Json<OAuthCompleteReq>,
) -> Result<Json<OAuthCompleteOut>, ApiErr> {
    let code = parse_authorization_code(&req.authorization_code)
        .map_err(|e| api_err(StatusCode::BAD_REQUEST, e))?;

    let pending = {
        let mut slot = pending_slot().lock();
        let Some(p) = slot.take() else {
            return Err(api_err(
                StatusCode::BAD_REQUEST,
                "no in-flight login — click Log in with Google Cloud again",
            ));
        };
        if now_secs().saturating_sub(p.created_at) >= PENDING_TTL_SECS {
            return Err(api_err(
                StatusCode::BAD_REQUEST,
                "login expired — click Log in with Google Cloud again",
            ));
        }
        p
    };

    let tokens = exchange_code(&code, &pending)
        .await
        .map_err(|e| api_err(StatusCode::BAD_REQUEST, e))?;
    finish_oauth_connect(&board, &tokens)
        .await
        .map_err(|e| api_err(StatusCode::BAD_REQUEST, e))?;

    let access = tokens
        .access_token
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or("");
    let projects = if access.is_empty() {
        Vec::new()
    } else {
        list_cloud_projects(access).await.unwrap_or_else(|e| {
            tracing::warn!(error = %e, "agy oauth: listCloudAICompanionProjects failed");
            Vec::new()
        })
    };

    let selected = board
        .openshell_providers()
        .into_iter()
        .find(|p| p.name == ANTIGRAVITY_PROVIDER)
        .and_then(|p| {
            p.config
                .get(CONFIG_PROJECT)
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        });
    let needs_project = selected.is_none();

    Ok(Json(OAuthCompleteOut {
        ok: true,
        projects,
        needs_project,
        selected_project: selected,
    }))
}

async fn oauth_select_project(
    State(board): State<SharedBoard>,
    Json(req): Json<OAuthSelectProjectReq>,
) -> Result<Json<serde_json::Value>, ApiErr> {
    let project_id = req.project_id.trim();
    if project_id.is_empty() {
        return Err(api_err(StatusCode::BAD_REQUEST, "project_id is required"));
    }
    select_project(&board, project_id)
        .await
        .map_err(|e| api_err(StatusCode::BAD_REQUEST, e))?;
    Ok(Json(serde_json::json!({ "ok": true, "project_id": project_id })))
}

/// Pull a bare authorization code, or `code=` from a pasted redirect URL.
fn parse_authorization_code(raw: &str) -> Result<String, String> {
    let raw = raw.trim().trim_matches(|c| c == '"' || c == '\'');
    if raw.is_empty() {
        return Err("paste the authorization code from Google".into());
    }
    if raw.contains("://") || raw.contains('?') {
        let uri: axum::http::Uri = raw
            .parse()
            .map_err(|e| format!("not a URL: {e}"))?;
        let query = uri.query().unwrap_or("");
        if query.is_empty() {
            return Err("URL has no code — paste the authorization code shown on the page".into());
        }
        let q: HashMap<String, String> =
            serde_urlencoded::from_str(query).map_err(|e| format!("query parse: {e}"))?;
        return q
            .get("code")
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "URL missing code=".into());
    }
    Ok(raw.to_string())
}

async fn oauth_disconnect(
    State(board): State<SharedBoard>,
) -> Result<Json<serde_json::Value>, ApiErr> {
    disconnect_oauth(&board)
        .await
        .map_err(|e| api_err(StatusCode::BAD_REQUEST, e))?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

/// Clear Antigravity credentials + refresh; keep project/location config.
pub async fn disconnect_oauth(board: &SharedBoard) -> Result<(), String> {
    let existing = board
        .openshell_providers()
        .into_iter()
        .find(|p| p.name == ANTIGRAVITY_PROVIDER);
    let config = existing
        .as_ref()
        .map(|p| p.config.clone())
        .unwrap_or_default();

    let desired = OpenShellProviderDesired {
        name: ANTIGRAVITY_PROVIDER.into(),
        provider_type: ANTIGRAVITY_PROVIDER.into(),
        config,
        credentials_sealed: None,
        credential_keys: vec![],
        refresh: None,
    }
    .normalized();
    let stored = board.upsert_openshell_provider(desired);

    let os = board.openshell_client();
    let _ = os
        .apply_provider(
            &stored.name,
            &stored.provider_type,
            BTreeMap::new(),
            stored.config.clone(),
            None,
        )
        .await;
    Ok(())
}

async fn select_project(board: &SharedBoard, project_id: &str) -> Result<(), String> {
    let existing = board
        .openshell_providers()
        .into_iter()
        .find(|p| p.name == ANTIGRAVITY_PROVIDER)
        .ok_or_else(|| {
            "no Board provider `antigravity` — complete Google login first".to_string()
        })?;

    let mut config = existing.config.clone();
    config.insert(CONFIG_PROJECT.into(), project_id.to_string());
    if config
        .get(CONFIG_LOCATION)
        .map(|s| s.trim().is_empty())
        .unwrap_or(true)
    {
        config.insert(CONFIG_LOCATION.into(), "global".into());
    }

    let desired = OpenShellProviderDesired {
        name: existing.name.clone(),
        provider_type: existing.provider_type.clone(),
        config,
        credentials_sealed: existing.credentials_sealed.clone(),
        credential_keys: existing.credential_keys.clone(),
        refresh: existing.refresh.clone(),
    }
    .normalized();
    let stored = board.upsert_openshell_provider(desired);

    let os = board.openshell_client();
    let credentials = match stored.credentials_sealed.as_deref() {
        Some(sealed) => open_string_map(sealed).map_err(|e| e.to_string())?,
        None => BTreeMap::new(),
    };
    let refresh_spec = stored.refresh.as_ref().map(|r| {
        let material = open_string_map(&r.material_sealed).unwrap_or_default();
        crate::openshell::ProviderRefreshSpec {
            credential_key: r.credential_key.clone(),
            strategy: r.strategy.clone(),
            material,
            secret_material_keys: r.secret_material_keys.clone(),
        }
    });
    os.apply_provider(
        &stored.name,
        &stored.provider_type,
        credentials,
        stored.config.clone(),
        refresh_spec.as_ref(),
    )
    .await
    .map_err(|e| format!("gateway apply antigravity: {e}"))?;

    refresh_cockpit_agy_auth(board).await;
    Ok(())
}

async fn finish_oauth_connect(board: &SharedBoard, tokens: &TokenResponse) -> Result<(), String> {
    let access = tokens
        .access_token
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "token response missing access_token".to_string())?;
    let refresh = tokens
        .refresh_token
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            "token response missing refresh_token (need access_type=offline + consent)".to_string()
        })?;

    let yaml = include_str!("../sandbox/openshell/antigravity.yaml").trim();
    provider_types::parse_provider_type_yaml(yaml, Some(ANTIGRAVITY_PROVIDER))?;
    board.upsert_openshell_provider_type(OpenShellProviderTypeDesired {
        id: ANTIGRAVITY_PROVIDER.into(),
        yaml: yaml.to_string(),
        shipped: true,
        form_config_keys: vec![CONFIG_PROJECT.into(), CONFIG_LOCATION.into()],
    })?;

    let os = board.openshell_client();
    os.upsert_provider_type_yaml(ANTIGRAVITY_PROVIDER, yaml)
        .await
        .map_err(|e| format!("import antigravity provider type: {e}"))?;

    let existing = board
        .openshell_providers()
        .into_iter()
        .find(|p| p.name == ANTIGRAVITY_PROVIDER);
    let mut config = existing
        .as_ref()
        .map(|p| p.config.clone())
        .unwrap_or_default();
    if config
        .get(CONFIG_PROJECT)
        .map(|s| s.trim().is_empty())
        .unwrap_or(true)
    {
        let _ = config
            .entry(CONFIG_PROJECT.into())
            .or_insert_with(String::new);
    }
    if config
        .get(CONFIG_LOCATION)
        .map(|s| s.trim().is_empty())
        .unwrap_or(true)
    {
        config.insert(CONFIG_LOCATION.into(), "global".into());
    }

    let mut creds = BTreeMap::new();
    creds.insert("ANTIGRAVITY_ACCESS_TOKEN".into(), access.to_string());
    let credentials_sealed = seal_string_map(&creds).map_err(|e| e.to_string())?;

    let mut material = BTreeMap::new();
    material.insert("client_id".into(), AGY_CLIENT_ID.to_string());
    material.insert("client_secret".into(), AGY_CLIENT_SECRET.to_string());
    material.insert("refresh_token".into(), refresh.to_string());
    let material_sealed = seal_string_map(&material).map_err(|e| e.to_string())?;

    let desired = OpenShellProviderDesired {
        name: ANTIGRAVITY_PROVIDER.into(),
        provider_type: ANTIGRAVITY_PROVIDER.into(),
        config,
        credentials_sealed: Some(credentials_sealed),
        credential_keys: vec!["ANTIGRAVITY_ACCESS_TOKEN".into()],
        refresh: Some(OpenShellProviderRefreshDesired {
            credential_key: "ANTIGRAVITY_ACCESS_TOKEN".into(),
            strategy: "oauth2_refresh_token".into(),
            material_sealed,
            secret_material_keys: vec!["client_secret".into(), "refresh_token".into()],
        }),
    }
    .normalized();
    let stored = board.upsert_openshell_provider(desired);

    let credentials = open_string_map(
        stored
            .credentials_sealed
            .as_deref()
            .ok_or_else(|| "missing sealed credentials".to_string())?,
    )
    .map_err(|e| e.to_string())?;
    let refresh_spec = {
        let r = stored
            .refresh
            .as_ref()
            .ok_or_else(|| "missing refresh".to_string())?;
        let material = open_string_map(&r.material_sealed).map_err(|e| e.to_string())?;
        crate::openshell::ProviderRefreshSpec {
            credential_key: r.credential_key.clone(),
            strategy: r.strategy.clone(),
            material,
            secret_material_keys: r.secret_material_keys.clone(),
        }
    };
    os.apply_provider(
        &stored.name,
        &stored.provider_type,
        credentials,
        stored.config.clone(),
        Some(&refresh_spec),
    )
    .await
    .map_err(|e| format!("gateway apply antigravity: {e}"))?;

    refresh_cockpit_agy_auth(board).await;
    Ok(())
}

async fn refresh_cockpit_agy_auth(board: &SharedBoard) {
    if let Some(session) = board.cockpit_session() {
        if session.status == CockpitSessionStatus::Running {
            if let Some(env) = session
                .environment
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                let resolved = board.resolve_cockpit_sandbox_create();
                if resolved.engine.as_deref().map(str::trim) == Some("agy") {
                    let os = board.openshell_client();
                    let _ = antigravity::attach_to_running_cockpit(board).await;
                    if let Err(e) = setup_agy_auth(&os, env, board).await {
                        tracing::warn!(error = %e, "agy oauth: setup_agy_auth after login failed");
                    }
                }
            }
        }
    }
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

async fn exchange_code(code: &str, pending: &PendingOAuth) -> Result<TokenResponse, String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| format!("http client: {e}"))?;
    let body = serde_urlencoded::to_string([
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", REDIRECT_URI),
        ("client_id", AGY_CLIENT_ID),
        ("client_secret", AGY_CLIENT_SECRET),
        ("code_verifier", pending.code_verifier.as_str()),
    ])
    .map_err(|e| format!("encode token body: {e}"))?;
    let resp = client
        .post(TOKEN_URL)
        .header(header::ACCEPT, "application/json")
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .map_err(|e| format!("token request: {e}"))?;
    let status = resp.status();
    let text = resp.text().await.map_err(|e| format!("token body: {e}"))?;
    if !status.is_success() {
        return Err(format!("token exchange {status}: {text}"));
    }
    serde_json::from_str(&text).map_err(|e| format!("token json: {e}"))
}

async fn list_cloud_projects(access_token: &str) -> Result<Vec<CloudProject>, String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| format!("http client: {e}"))?;

    // Proto HTTP annotation is GET; some clients POST — try GET then POST.
    let get_resp = client
        .get(LIST_PROJECTS_URL)
        .bearer_auth(access_token)
        .header(header::ACCEPT, "application/json")
        .send()
        .await
        .map_err(|e| format!("list projects GET: {e}"))?;
    let get_status = get_resp.status();
    let get_text = get_resp
        .text()
        .await
        .map_err(|e| format!("list projects GET body: {e}"))?;
    if get_status.is_success() {
        return parse_projects_json(&get_text);
    }

    let post_resp = client
        .post(LIST_PROJECTS_URL)
        .bearer_auth(access_token)
        .header(header::ACCEPT, "application/json")
        .header(header::CONTENT_TYPE, "application/json")
        .body("{}")
        .send()
        .await
        .map_err(|e| format!("list projects POST: {e}"))?;
    let post_status = post_resp.status();
    let post_text = post_resp
        .text()
        .await
        .map_err(|e| format!("list projects POST body: {e}"))?;
    if !post_status.is_success() {
        return Err(format!(
            "list projects failed GET {get_status}: {get_text}; POST {post_status}: {post_text}"
        ));
    }
    parse_projects_json(&post_text)
}

fn parse_projects_json(text: &str) -> Result<Vec<CloudProject>, String> {
    let v: serde_json::Value =
        serde_json::from_str(text).map_err(|e| format!("projects json: {e}"))?;
    let arr = v
        .get("projects")
        .and_then(|p| p.as_array())
        .cloned()
        .unwrap_or_default();
    let mut out = Vec::new();
    for item in arr {
        if let Some(id) = item.as_str().map(str::trim).filter(|s| !s.is_empty()) {
            out.push(CloudProject {
                id: id.to_string(),
                name: None,
            });
            continue;
        }
        let id = item
            .get("id")
            .or_else(|| item.get("projectId"))
            .or_else(|| item.get("project_id"))
            .and_then(|x| x.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let Some(id) = id else {
            continue;
        };
        let name = item
            .get("name")
            .or_else(|| item.get("displayName"))
            .or_else(|| item.get("display_name"))
            .and_then(|x| x.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());
        out.push(CloudProject {
            id: id.to_string(),
            name,
        });
    }
    Ok(out)
}

fn sanitize_return_path(raw: Option<&str>) -> String {
    let Some(r) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return DEFAULT_RETURN_PATH.into();
    };
    if r.starts_with("/settings/") && !r.contains("://") && !r.contains('\n') {
        r.to_string()
    } else {
        DEFAULT_RETURN_PATH.into()
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

/// Build the Cloud-project authorize URL (exported for tests).
#[cfg(test)]
fn build_authorize_url(state: &str, code_challenge: &str) -> String {
    let scope = AGY_SCOPES.join(" ");
    format!(
        "{AUTHORIZE_URL}?response_type=code&client_id={}&redirect_uri={}&state={}&code_challenge={}&code_challenge_method=S256&scope={}&access_type=offline&prompt=consent",
        urlencoding(AGY_CLIENT_ID),
        urlencoding(REDIRECT_URI),
        urlencoding(state),
        urlencoding(code_challenge),
        urlencoding(&scope),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_challenge_is_s256() {
        let v = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(
            pkce_challenge_s256(v),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn sanitize_return_path_settings_only() {
        assert_eq!(
            sanitize_return_path(Some("/settings/openshell")),
            "/settings/openshell"
        );
        assert_eq!(
            sanitize_return_path(Some("https://evil.example/")),
            DEFAULT_RETURN_PATH
        );
        assert_eq!(sanitize_return_path(None), DEFAULT_RETURN_PATH);
    }

    #[test]
    fn authorize_url_uses_accounts_google_and_paste_redirect() {
        let url = build_authorize_url("st", "ch");
        assert!(url.starts_with("https://accounts.google.com/o/oauth2/v2/auth?"));
        assert!(url.contains(&urlencoding(AGY_CLIENT_ID)));
        assert!(url.contains(&urlencoding(REDIRECT_URI)));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("access_type=offline"));
        assert!(!url.contains("127.0.0.1"));
        assert!(!url.contains("auth.cloud.google"));
        assert!(url.contains("884354919052"));
        assert!(!url.contains("1071006060591"));
    }

    #[test]
    fn parse_authorization_code_bare_and_url() {
        assert_eq!(
            parse_authorization_code("4/0AeanS").unwrap(),
            "4/0AeanS"
        );
        assert_eq!(
            parse_authorization_code(
                "https://antigravity.google/oauth-callback?code=4%2Fxyz&scope=email"
            )
            .unwrap(),
            "4/xyz"
        );
        assert!(parse_authorization_code("").is_err());
        assert!(parse_authorization_code("https://antigravity.google/oauth-callback").is_err());
    }

    #[test]
    fn parse_projects_json_shapes() {
        let a = parse_projects_json(
            r#"{"projects":[{"id":"p1","displayName":"One"},{"projectId":"p2"}]}"#,
        )
        .unwrap();
        assert_eq!(a.len(), 2);
        assert_eq!(a[0].id, "p1");
        assert_eq!(a[0].name.as_deref(), Some("One"));
        assert_eq!(a[1].id, "p2");

        let b = parse_projects_json(r#"{"projects":["plain-id"]}"#).unwrap();
        assert_eq!(b[0].id, "plain-id");
    }

    #[test]
    fn shipped_yaml_parses_with_refresh() {
        let yaml = include_str!("../sandbox/openshell/antigravity.yaml");
        let parsed = provider_types::parse_provider_type_yaml(yaml, Some(ANTIGRAVITY_PROVIDER))
            .expect("yaml");
        assert_eq!(parsed.id, ANTIGRAVITY_PROVIDER);
        assert!(parsed
            .credential_env_vars
            .iter()
            .any(|e| e == "ANTIGRAVITY_ACCESS_TOKEN"));
        assert!(yaml.contains("oauth2_refresh_token"));
        assert!(yaml.contains("oauth2.googleapis.com/token"));
    }
}
