//! Host-mediated OpenShell gateway OIDC login (Authorization Code + PKCE).
//!
//! The IdP client (OpenShell CLI / Keycloak) already allows loopback
//! `redirect_uri`s. Sandboard asks for `http://127.0.0.1:<port>/callback` — the
//! same shape as `openshell gateway login` / Hermes — and the operator pastes
//! the callback URL (the loopback page will not load on a Tailscale board).
//! Token exchange still uses that exact redirect_uri. Distinct from
//! [`crate::antigravity_oauth`] (Google hosted paste-code) and from MCP
//! client OAuth (sandboard as the authorization server).

use crate::secrets::OpenShellOidcBundle;
use crate::store::SharedBoard;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use oauth2::basic::BasicClient;
use oauth2::{
    AuthUrl, AuthorizationCode, ClientId, CsrfToken, PkceCodeChallenge, PkceCodeVerifier,
    RedirectUrl, TokenResponse, TokenUrl,
};
use parking_lot::Mutex;
use rand::RngExt;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

const RETURN_PATH: &str = "/settings/openshell/connectivity";
const PENDING_TTL_SECS: u64 = 600;
const LOOPBACK_PORT_MIN: u16 = 49152;
const LOOPBACK_PORT_MAX: u16 = 65535;

// Do not send `scope` on authorize. Requesting `openid` / `offline_access`
// returns `invalid_scope` on IdP clients that do not assign those as
// optional client scopes (the OpenShell-provisioned Keycloak client is
// one). The IdP then uses its default client scopes. A refresh token is
// still required at exchange — Keycloak often issues a session-bound
// refresh without `offline_access`; if it does not, complete will say so.

fn pending() -> &'static Mutex<HashMap<String, PendingOAuth>> {
    static STORE: OnceLock<Mutex<HashMap<String, PendingOAuth>>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(HashMap::new()))
}

#[derive(Clone)]
struct PendingOAuth {
    code_verifier: String,
    client_id: String,
    issuer: String,
    /// Exact redirect_uri used at authorize — must match token exchange.
    redirect_uri: String,
    auth_url: String,
    token_url: String,
    created_at: u64,
}

/// Session-gated API under `/api/openshell/oidc`.
pub fn routes() -> Router<SharedBoard> {
    Router::new()
        .route("/login", post(oauth_login))
        .route("/complete", post(oauth_complete))
        .route("/logout", post(oauth_logout))
}

/// Browser callback under `/oauth/openshell/…` (not `/api` — leftover for
/// an IdP that still redirects at the board origin). Loopback paste is the
/// live path; this GET is a no-op unless something still hits it with code.
pub fn callback_routes() -> Router<SharedBoard> {
    Router::new().route("/callback", get(oauth_callback))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OidcLoginOut {
    pub authorize_url: String,
    pub redirect_uri: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OidcLogoutOut {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OidcCompleteOut {
    pub ok: bool,
}

#[derive(Debug, Deserialize)]
pub struct OidcCompleteReq {
    /// Full loopback redirect URL, or the `?code=…&state=…` query (Hermes).
    pub redirect: String,
}

#[derive(Debug, thiserror::Error)]
enum ApiErr {
    #[error("{0}")]
    Msg(String),
}

impl IntoResponse for ApiErr {
    fn into_response(self) -> Response {
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": self.to_string() })),
        )
            .into_response()
    }
}

async fn oauth_login(State(board): State<SharedBoard>) -> Result<Json<OidcLoginOut>, ApiErr> {
    if board.openshell_auth_mode() != Some(crate::model::OpenShellAuthMode::Oidc) {
        return Err(ApiErr::Msg(
            "auth mode must be OIDC before logging in (Settings → OpenShell)".into(),
        ));
    }
    let cfg = board.openshell_oidc_config().unwrap_or_default().trimmed();
    cfg.validate().map_err(ApiErr::Msg)?;

    let redirect_uri = loopback_redirect_uri();

    let discovery = openshell_sdk::oidc::discover(&cfg.issuer, false)
        .await
        .map_err(|e| ApiErr::Msg(format!("OIDC discovery: {e}")))?;

    let client = BasicClient::new(ClientId::new(cfg.client_id.clone()))
        .set_auth_uri(
            AuthUrl::new(discovery.authorization_endpoint.clone())
                .map_err(|e| ApiErr::Msg(format!("auth URL: {e}")))?,
        )
        .set_token_uri(
            TokenUrl::new(discovery.token_endpoint.clone())
                .map_err(|e| ApiErr::Msg(format!("token URL: {e}")))?,
        )
        .set_redirect_uri(
            RedirectUrl::new(redirect_uri.clone())
                .map_err(|e| ApiErr::Msg(format!("redirect URL: {e}")))?,
        );

    let (pkce_challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();
    let auth_request = client
        .authorize_url(CsrfToken::new_random)
        .set_pkce_challenge(pkce_challenge);
    let (mut auth_url, csrf_token) = auth_request.url();
    let audience = cfg.audience.trim();
    if !audience.is_empty() {
        auth_url
            .query_pairs_mut()
            .append_pair("audience", audience);
    }

    let state = csrf_token.secret().clone();
    {
        let mut st = pending().lock();
        st.retain(|_, p| now_secs().saturating_sub(p.created_at) < PENDING_TTL_SECS);
        st.insert(
            state,
            PendingOAuth {
                code_verifier: pkce_verifier.secret().clone(),
                client_id: cfg.client_id.clone(),
                issuer: cfg.issuer.trim_end_matches('/').to_string(),
                redirect_uri: redirect_uri.clone(),
                auth_url: discovery.authorization_endpoint,
                token_url: discovery.token_endpoint,
                created_at: now_secs(),
            },
        );
    }

    Ok(Json(OidcLoginOut {
        authorize_url: auth_url.to_string(),
        redirect_uri,
    }))
}

async fn oauth_logout(State(board): State<SharedBoard>) -> Json<OidcLogoutOut> {
    board.set_openshell_oidc_sealed(None);
    Json(OidcLogoutOut {
        ok: true,
        error: None,
    })
}

async fn oauth_complete(
    State(board): State<SharedBoard>,
    Json(req): Json<OidcCompleteReq>,
) -> Result<Json<OidcCompleteOut>, ApiErr> {
    let parsed = parse_pasted_callback(&req.redirect).map_err(ApiErr::Msg)?;
    if let Some(err) = parsed.error.as_deref() {
        let desc = parsed.error_description.as_deref().unwrap_or(err);
        return Err(ApiErr::Msg(desc.to_string()));
    }
    let (Some(code), Some(state)) = (parsed.code.as_deref(), parsed.state.as_deref()) else {
        return Err(ApiErr::Msg(
            "paste the redirect URL (or ?code=…&state=…) from the address bar".into(),
        ));
    };
    finish_login(&board, code, state)
        .await
        .map_err(ApiErr::Msg)?;
    Ok(Json(OidcCompleteOut { ok: true }))
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
            "{RETURN_PATH}?openshell_oidc=error&message={}",
            urlencoding(desc)
        ))
        .into_response();
    }
    let (Some(code), Some(state)) = (q.code.as_deref(), q.state.as_deref()) else {
        return (StatusCode::BAD_REQUEST, "missing code or state").into_response();
    };

    match finish_login(&board, code, state).await {
        Ok(()) => Redirect::to(&format!("{RETURN_PATH}?openshell_oidc=ok")).into_response(),
        Err(e) => Redirect::to(&format!(
            "{RETURN_PATH}?openshell_oidc=error&message={}",
            urlencoding(&e)
        ))
        .into_response(),
    }
}

async fn finish_login(board: &SharedBoard, code: &str, state: &str) -> Result<(), String> {
    let pending_row = {
        let mut st = pending().lock();
        st.remove(state)
    };
    let Some(p) = pending_row else {
        return Err("expired_or_unknown_state".into());
    };

    if now_secs().saturating_sub(p.created_at) >= PENDING_TTL_SECS {
        return Err("login_expired".into());
    }

    exchange_and_seal(board, &p, code).await
}

async fn exchange_and_seal(
    board: &SharedBoard,
    p: &PendingOAuth,
    code: &str,
) -> Result<(), String> {
    let client = BasicClient::new(ClientId::new(p.client_id.clone()))
        .set_auth_uri(AuthUrl::new(p.auth_url.clone()).map_err(|e| format!("auth URL: {e}"))?)
        .set_token_uri(TokenUrl::new(p.token_url.clone()).map_err(|e| format!("token URL: {e}"))?)
        .set_redirect_uri(
            RedirectUrl::new(p.redirect_uri.clone()).map_err(|e| format!("redirect URL: {e}"))?,
        );

    let http = openshell_sdk::oidc::http_client(false);
    let token_response = client
        .exchange_code(AuthorizationCode::new(code.to_string()))
        .set_pkce_verifier(PkceCodeVerifier::new(p.code_verifier.clone()))
        .request_async(&http)
        .await
        .map_err(|e| format!("token exchange failed: {e}"))?;

    let now = now_secs();
    let expires_at = token_response
        .expires_in()
        .map(|ei| now.saturating_add(ei.as_secs()))
        .unwrap_or(now.saturating_add(3600));
    let refresh = token_response
        .refresh_token()
        .map(|rt| rt.secret().clone())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "IdP did not return a refresh_token (need offline_access)".to_string())?;

    let bundle = OpenShellOidcBundle {
        access_token: token_response.access_token().secret().clone(),
        refresh_token: refresh,
        expires_at,
        issuer: p.issuer.clone(),
        client_id: p.client_id.clone(),
    };
    let sealed = crate::secrets::seal_oidc(&bundle).map_err(|e| e.to_string())?;
    board.set_openshell_oidc_sealed(Some(sealed));
    Ok(())
}

fn loopback_redirect_uri() -> String {
    let port = rand::rng().random_range(LOOPBACK_PORT_MIN..=LOOPBACK_PORT_MAX);
    format!("http://127.0.0.1:{port}/callback")
}

struct ParsedCallback {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

/// Pull `code` + `state` from a pasted loopback URL or query string.
fn parse_pasted_callback(raw: &str) -> Result<ParsedCallback, String> {
    let raw = raw.trim().trim_matches(|c| c == '"' || c == '\'');
    if raw.is_empty() {
        return Err("paste the redirect URL (or ?code=…&state=…) from the address bar".into());
    }
    let query = if raw.contains("://") {
        let uri: axum::http::Uri = raw.parse().map_err(|e| format!("not a URL: {e}"))?;
        uri.query().unwrap_or("").to_string()
    } else {
        raw.trim_start_matches('?').to_string()
    };
    if query.is_empty() {
        return Err("URL has no query — paste the full redirect from the address bar".into());
    }
    let q: HashMap<String, String> =
        serde_urlencoded::from_str(&query).map_err(|e| format!("query parse: {e}"))?;
    Ok(ParsedCallback {
        code: q.get("code").map(|s| s.trim().to_string()).filter(|s| !s.is_empty()),
        state: q
            .get("state")
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
        error: q.get("error").cloned(),
        error_description: q.get("error_description").cloned(),
    })
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
    fn loopback_redirect_uri_is_cli_shaped() {
        let uri = loopback_redirect_uri();
        let parsed: axum::http::Uri = uri.parse().unwrap();
        assert_eq!(parsed.scheme_str(), Some("http"));
        assert_eq!(parsed.host(), Some("127.0.0.1"));
        assert_eq!(parsed.path(), "/callback");
        let port = parsed.port_u16().unwrap();
        assert!((LOOPBACK_PORT_MIN..=LOOPBACK_PORT_MAX).contains(&port));
    }

    #[test]
    fn parse_pasted_callback_full_url_and_query() {
        let url = "http://127.0.0.1:48539/callback?code=YoXURL&state=o0_LXjFH";
        let a = parse_pasted_callback(url).unwrap();
        assert_eq!(a.code.as_deref(), Some("YoXURL"));
        assert_eq!(a.state.as_deref(), Some("o0_LXjFH"));

        let b = parse_pasted_callback("?code=YoXURL&state=o0_LXjFH").unwrap();
        assert_eq!(b.code, a.code);
        assert_eq!(b.state, a.state);

        let c = parse_pasted_callback("code=YoXURL&state=o0_LXjFH").unwrap();
        assert_eq!(c.code, a.code);
        assert_eq!(c.state, a.state);
    }

    #[test]
    fn parse_pasted_callback_idp_error() {
        let p = parse_pasted_callback(
            "http://127.0.0.1:1/callback?error=access_denied&error_description=nope",
        )
        .unwrap();
        assert_eq!(p.error.as_deref(), Some("access_denied"));
        assert_eq!(p.error_description.as_deref(), Some("nope"));
        assert!(p.code.is_none());
    }

    #[test]
    fn parse_pasted_callback_rejects_empty() {
        assert!(parse_pasted_callback("").is_err());
        assert!(parse_pasted_callback("http://127.0.0.1:1/callback").is_err());
    }

    #[test]
    fn pending_unknown_state_is_absent() {
        let mut st = pending().lock();
        st.clear();
        assert!(st.remove("no-such-state").is_none());
    }

    #[test]
    fn pending_ttl_evicts_stale() {
        let mut st = pending().lock();
        st.clear();
        st.insert(
            "old".into(),
            PendingOAuth {
                code_verifier: "v".into(),
                client_id: "openshell-cli".into(),
                issuer: "https://idp.example/realms/openshell".into(),
                redirect_uri: "http://127.0.0.1:49152/callback".into(),
                auth_url: "https://idp.example/auth".into(),
                token_url: "https://idp.example/token".into(),
                created_at: now_secs().saturating_sub(PENDING_TTL_SECS + 1),
            },
        );
        st.insert(
            "fresh".into(),
            PendingOAuth {
                code_verifier: "v2".into(),
                client_id: "openshell-cli".into(),
                issuer: "https://idp.example/realms/openshell".into(),
                redirect_uri: "http://127.0.0.1:49152/callback".into(),
                auth_url: "https://idp.example/auth".into(),
                token_url: "https://idp.example/token".into(),
                created_at: now_secs(),
            },
        );
        st.retain(|_, p| now_secs().saturating_sub(p.created_at) < PENDING_TTL_SECS);
        assert!(st.get("old").is_none());
        assert!(st.get("fresh").is_some());
        st.clear();
    }

    #[test]
    fn urlencoding_encodes_spaces() {
        assert_eq!(urlencoding("a b"), "a%20b");
        assert_eq!(urlencoding("ok-_.~"), "ok-_.~");
    }
}
