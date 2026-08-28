//! Operator authentication: local admin + GitHub App OAuth allowlist.
//!
//! `/api` (except webhooks) accepts a `sandboard_session` cookie **or** HTTP Basic
//! with the local admin username/password. `/auth/*`, `/healthz`, and MCP OAuth
//! discovery/token endpoints stay reachable without a session. `/mcp` itself
//! uses Bearer tokens (see `mcp_oauth`) once admin exists.

use crate::secrets::{seal_auth, AuthBundle};
use crate::store::SharedBoard;

use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use axum::extract::{Query, Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use base64::Engine;
use hmac::{Hmac, KeyInit, Mac};
use rand::RngExt;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::time::{SystemTime, UNIX_EPOCH};
use time::Duration as TimeDuration;

const SESSION_COOKIE: &str = "sandboard_session";
const SESSION_TTL_SECS: u64 = 60 * 60 * 24 * 14; // 14 days
const OAUTH_STATE_TTL_SECS: u64 = 600;

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionKind {
    Admin,
    Github,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionUser {
    pub kind: SessionKind,
    pub login: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SessionPayload {
    kind: SessionKind,
    login: String,
    exp: u64,
}

#[derive(Debug, Serialize)]
pub struct AuthStatus {
    /// True when local admin password is configured.
    pub enabled: bool,
    /// True when no admin exists yet — only bootstrap is allowed.
    pub bootstrap: bool,
    /// True when GitHub App has client_id + client_secret.
    pub github_login_enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<SessionUser>,
}

#[derive(Debug, Deserialize)]
pub struct BootstrapBody {
    pub username: String,
    pub password: String,
}

#[derive(Debug, Deserialize)]
pub struct LoginBody {
    pub username: String,
    pub password: String,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct AuthSettingsView {
    pub admin_username: String,
    pub allowed_users: Vec<String>,
    pub allowed_teams: Vec<String>,
    pub github_login_enabled: bool,
    pub has_client_secret: bool,
}

#[derive(Debug, Deserialize)]
pub struct AuthSettingsWrite {
    #[serde(default)]
    pub allowed_users: Option<Vec<String>>,
    #[serde(default)]
    pub allowed_teams: Option<Vec<String>>,
    #[serde(default)]
    pub new_password: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct GithubStartQuery {
    /// Browser origin (`http://localhost:5173`). Preferred over proxy Host headers.
    pub return_origin: Option<String>,
    /// Optional same-origin path to land on after GitHub login (MCP authorize).
    pub next: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct GithubCallbackQuery {
    pub code: Option<String>,
    pub state: Option<String>,
    pub error: Option<String>,
    pub error_description: Option<String>,
    /// Present when GitHub finishes App install/update (Setup URL = this callback).
    /// That flow has no OAuth `state` — do not treat it as Sign in with GitHub.
    pub installation_id: Option<u64>,
    pub setup_action: Option<String>,
}

pub fn routes() -> Router<SharedBoard> {
    Router::new()
        .route("/status", get(auth_status))
        .route("/bootstrap", post(bootstrap))
        .route("/login", post(login))
        .route("/logout", post(logout))
        .route("/github", get(github_start))
        .route("/github/callback", get(github_callback))
}

/// Settings routes live under `/api/auth/settings` (session-gated).
pub fn api_settings_routes() -> Router<SharedBoard> {
    Router::new().route("/settings", get(get_auth_settings).put(put_auth_settings))
}

pub fn path_exempt(path: &str) -> bool {
    path == "/healthz"
        || path == "/llms.txt"
        || path.starts_with("/auth/")
        || path == "/auth"
        || path == "/api/webhooks/github"
        // MCP OAuth AS + discovery (Bearer gate lives on `/mcp` separately).
        || path.starts_with("/.well-known/oauth-")
        || path == "/oauth/register"
        || path == "/oauth/token"
        || path.starts_with("/oauth/authorize")
}

/// Axum middleware: require a valid session for non-exempt paths.
pub async fn require_session(
    State(board): State<SharedBoard>,
    jar: CookieJar,
    headers: HeaderMap,
    mut req: Request,
    next: Next,
) -> Response {
    let path = req.uri().path();
    if path_exempt(path) {
        return next.run(req).await;
    }
    // Static assets / SPA shell — allow; the frontend calls /auth/status.
    if !path.starts_with("/api/") {
        return next.run(req).await;
    }

    match operator_from_jar_or_basic(&board, &jar, &headers) {
        Some(user) => {
            req.extensions_mut().insert(user);
            next.run(req).await
        }
        None => (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({
                "error": "authentication required",
                "bootstrap": board.auth_bundle().is_none(),
            })),
        )
            .into_response(),
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn new_session_key_b64() -> String {
    let mut key = [0u8; 32];
    rand::rng().fill(&mut key);
    base64::engine::general_purpose::STANDARD.encode(key)
}

fn hash_password(password: &str) -> Result<String, String> {
    use argon2::password_hash::rand_core::OsRng;
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| e.to_string())
}

fn verify_password(password: &str, hash: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(hash) else {
        return false;
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

fn sign_payload(key: &[u8], payload_b64: &str) -> Result<String, String> {
    let mut mac = HmacSha256::new_from_slice(key).map_err(|e| e.to_string())?;
    mac.update(payload_b64.as_bytes());
    Ok(hex::encode(mac.finalize().into_bytes()))
}

fn encode_session(key: &[u8], user: &SessionUser, ttl_secs: u64) -> Result<String, String> {
    let payload = SessionPayload {
        kind: user.kind.clone(),
        login: user.login.clone(),
        exp: now_secs().saturating_add(ttl_secs),
    };
    let json = serde_json::to_vec(&payload).map_err(|e| e.to_string())?;
    let payload_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json);
    let sig = sign_payload(key, &payload_b64)?;
    Ok(format!("{payload_b64}.{sig}"))
}

fn decode_session(key: &[u8], cookie: &str) -> Option<SessionUser> {
    let (payload_b64, sig) = cookie.split_once('.')?;
    let expect = sign_payload(key, payload_b64).ok()?;
    if !constant_time_eq(sig.as_bytes(), expect.as_bytes()) {
        return None;
    }
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload_b64)
        .ok()?;
    let payload: SessionPayload = serde_json::from_slice(&raw).ok()?;
    if payload.exp < now_secs() {
        return None;
    }
    if payload.login.trim().is_empty() {
        return None;
    }
    Some(SessionUser {
        kind: payload.kind,
        login: payload.login,
    })
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b.iter()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn session_from_jar(board: &SharedBoard, jar: &CookieJar) -> Option<SessionUser> {
    let auth = board.auth_bundle()?;
    let key = auth.session_key_bytes().ok()?;
    let cookie = jar.get(SESSION_COOKIE)?.value().to_string();
    decode_session(&key, &cookie)
}

/// Local admin via `Authorization: Basic …`. Cookie sessions stay preferred for
/// browsers; this is for scripts/`curl` against `/api`.
fn admin_from_basic(board: &SharedBoard, headers: &HeaderMap) -> Option<SessionUser> {
    let auth = board.auth_bundle()?;
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, param) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("basic") {
        return None;
    }
    let raw = base64::engine::general_purpose::STANDARD
        .decode(param.trim())
        .ok()?;
    let decoded = std::str::from_utf8(&raw).ok()?;
    let (username, password) = decoded.split_once(':')?;
    if !username
        .trim()
        .eq_ignore_ascii_case(auth.admin_username.trim())
        || !verify_password(password, &auth.password_hash)
    {
        return None;
    }
    Some(SessionUser {
        kind: SessionKind::Admin,
        login: auth.admin_username.clone(),
    })
}

fn operator_from_jar_or_basic(
    board: &SharedBoard,
    jar: &CookieJar,
    headers: &HeaderMap,
) -> Option<SessionUser> {
    session_from_jar(board, jar).or_else(|| admin_from_basic(board, headers))
}

/// Session for MCP authorize consent (and other non-API callers).
pub fn session_user_from_jar(board: &SharedBoard, jar: &CookieJar) -> Option<SessionUser> {
    session_from_jar(board, jar)
}

/// Mint a `sandboard_session` cookie value for tests / local tooling.
#[cfg(test)]
pub fn mint_session_cookie_value(
    board: &SharedBoard,
    user: &SessionUser,
) -> Result<String, String> {
    let auth = board.auth_bundle().ok_or_else(|| "auth not configured".to_string())?;
    let key = auth.session_key_bytes().map_err(|e| e.to_string())?;
    encode_session(&key, user, SESSION_TTL_SECS)
}

fn session_cookie(value: String) -> Cookie<'static> {
    Cookie::build((SESSION_COOKIE, value))
        .path("/")
        .http_only(true)
        .same_site(SameSite::Lax)
        .max_age(TimeDuration::seconds(SESSION_TTL_SECS as i64))
        .build()
}

fn clear_session_cookie() -> Cookie<'static> {
    Cookie::build((SESSION_COOKIE, ""))
        .path("/")
        .http_only(true)
        .same_site(SameSite::Lax)
        .max_age(TimeDuration::seconds(0))
        .build()
}

fn github_login_enabled(board: &SharedBoard) -> bool {
    board
        .github_app_bundle()
        .map(|b| !b.client_id.trim().is_empty() && !b.client_secret.trim().is_empty())
        .unwrap_or(false)
}

/// Prefer proxy-forwarded host so Vite (`localhost:5173`) and direct `:8080`
/// both round-trip OAuth on the origin the browser actually uses.
fn public_base_url(headers: &HeaderMap) -> String {
    crate::mcp_oauth::public_origin(headers)
}

fn redirect_uri(headers: &HeaderMap) -> String {
    format!("{}/auth/github/callback", public_base_url(headers))
}

/// Only loopback http(s) origins — blocks open redirects via `return_origin`.
fn sanitize_return_origin(raw: &str) -> Option<String> {
    let raw = raw.trim().trim_end_matches('/');
    let uri = raw.parse::<axum::http::Uri>().ok()?;
    let scheme = uri.scheme_str()?;
    if scheme != "http" && scheme != "https" {
        return None;
    }
    let auth = uri.authority()?;
    let host = auth.host();
    if host != "localhost" && host != "127.0.0.1" && host != "[::1]" && host != "::1" {
        return None;
    }
    // Reject paths/queries smuggled into return_origin.
    if uri.path() != "/" && uri.path() != "" {
        return None;
    }
    if uri.query().is_some() {
        return None;
    }
    Some(format!("{scheme}://{auth}"))
}

fn redirect_uri_for_start(headers: &HeaderMap, return_origin: Option<&str>) -> String {
    if let Some(origin) = return_origin.and_then(sanitize_return_origin) {
        return format!("{origin}/auth/github/callback");
    }
    redirect_uri(headers)
}

#[derive(Debug, Serialize, Deserialize)]
struct OAuthStatePayload {
    n: String,
    exp: u64,
    /// Exact redirect_uri used at authorize time (must match token exchange).
    ru: String,
    /// Optional post-login path (e.g. `/oauth/authorize?...`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    next: Option<String>,
}

/// Only MCP authorize return paths — blocks open redirects via `next`.
fn sanitize_login_next(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if !raw.starts_with("/oauth/authorize") {
        return None;
    }
    if raw.contains("://") || raw.contains('\\') || raw.contains('\n') || raw.contains('\r') {
        return None;
    }
    Some(raw.to_string())
}

/// Cookie-less CSRF state — survives Vite↔backend origin splits.
fn encode_oauth_state(
    key: &[u8],
    redirect: &str,
    next: Option<&str>,
) -> Result<String, String> {
    let mut nonce = [0u8; 16];
    rand::rng().fill(&mut nonce);
    let payload = OAuthStatePayload {
        n: hex::encode(nonce),
        exp: now_secs().saturating_add(OAUTH_STATE_TTL_SECS),
        ru: redirect.to_string(),
        next: next.and_then(sanitize_login_next),
    };
    let json = serde_json::to_vec(&payload).map_err(|e| e.to_string())?;
    let payload_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json);
    let sig = sign_payload(key, &payload_b64)?;
    Ok(format!("{payload_b64}.{sig}"))
}

fn decode_oauth_state(key: &[u8], state: &str) -> Option<OAuthStatePayload> {
    let (payload_b64, sig) = state.split_once('.')?;
    let expect = sign_payload(key, payload_b64).ok()?;
    if !constant_time_eq(sig.as_bytes(), expect.as_bytes()) {
        return None;
    }
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload_b64)
        .ok()?;
    let payload: OAuthStatePayload = serde_json::from_slice(&raw).ok()?;
    if payload.exp < now_secs() || payload.ru.trim().is_empty() {
        return None;
    }
    Some(payload)
}

async fn auth_status(State(board): State<SharedBoard>, jar: CookieJar) -> Json<AuthStatus> {
    let enabled = board.auth_bundle().is_some();
    let user = session_from_jar(&board, &jar);
    Json(AuthStatus {
        bootstrap: !enabled,
        enabled,
        github_login_enabled: github_login_enabled(&board),
        user,
    })
}

async fn bootstrap(
    State(board): State<SharedBoard>,
    jar: CookieJar,
    Json(body): Json<BootstrapBody>,
) -> Result<(CookieJar, Json<AuthStatus>), (StatusCode, Json<serde_json::Value>)> {
    if board.auth_bundle().is_some() {
        return Err((
            StatusCode::CONFLICT,
            Json(serde_json::json!({ "error": "already bootstrapped" })),
        ));
    }
    let username = body.username.trim().to_string();
    let password = body.password;
    if username.is_empty() || password.len() < 8 {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "username required; password must be at least 8 characters"
            })),
        ));
    }
    let password_hash = hash_password(&password).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e })),
        )
    })?;
    let bundle = AuthBundle {
        admin_username: username.clone(),
        password_hash,
        session_key_b64: new_session_key_b64(),
    };
    let sealed = seal_auth(&bundle).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": format!("seal auth: {e}") })),
        )
    })?;
    board.set_auth_sealed(Some(sealed));

    let user = SessionUser {
        kind: SessionKind::Admin,
        login: username,
    };
    let key = bundle.session_key_bytes().map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
    })?;
    let token = encode_session(&key, &user, SESSION_TTL_SECS).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e })),
        )
    })?;
    let jar = jar.add(session_cookie(token));
    Ok((
        jar,
        Json(AuthStatus {
            enabled: true,
            bootstrap: false,
            github_login_enabled: github_login_enabled(&board),
            user: Some(user),
        }),
    ))
}

async fn login(
    State(board): State<SharedBoard>,
    jar: CookieJar,
    Json(body): Json<LoginBody>,
) -> Result<(CookieJar, Json<AuthStatus>), (StatusCode, Json<serde_json::Value>)> {
    let Some(auth) = board.auth_bundle() else {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "bootstrap required",
                "bootstrap": true,
            })),
        ));
    };
    let username = body.username.trim();
    if !username.eq_ignore_ascii_case(auth.admin_username.trim())
        || !verify_password(&body.password, &auth.password_hash)
    {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({ "error": "invalid username or password" })),
        ));
    }
    let user = SessionUser {
        kind: SessionKind::Admin,
        login: auth.admin_username.clone(),
    };
    let key = auth.session_key_bytes().map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
    })?;
    let token = encode_session(&key, &user, SESSION_TTL_SECS).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e })),
        )
    })?;
    Ok((
        jar.add(session_cookie(token)),
        Json(AuthStatus {
            enabled: true,
            bootstrap: false,
            github_login_enabled: github_login_enabled(&board),
            user: Some(user),
        }),
    ))
}

async fn logout(jar: CookieJar) -> (CookieJar, StatusCode) {
    (jar.add(clear_session_cookie()), StatusCode::NO_CONTENT)
}

async fn github_start(
    State(board): State<SharedBoard>,
    headers: HeaderMap,
    Query(q): Query<GithubStartQuery>,
) -> Result<Redirect, (StatusCode, Json<serde_json::Value>)> {
    let Some(auth) = board.auth_bundle() else {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "bootstrap local admin before GitHub login",
                "bootstrap": true,
            })),
        ));
    };
    let Some(app) = board.github_app_bundle() else {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "GitHub App not configured" })),
        ));
    };
    if app.client_id.trim().is_empty() || app.client_secret.trim().is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "GitHub App client_id and client_secret required for OAuth"
            })),
        ));
    }
    let key = auth.session_key_bytes().map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
    })?;
    let redirect = redirect_uri_for_start(&headers, q.return_origin.as_deref());
    let state = encode_oauth_state(&key, &redirect, q.next.as_deref()).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e })),
        )
    })?;
    let url = format!(
        "https://github.com/login/oauth/authorize?client_id={}&redirect_uri={}&state={}&scope={}",
        urlencoding_encode(app.client_id.trim()),
        urlencoding_encode(&redirect),
        urlencoding_encode(&state),
        urlencoding_encode("read:user read:org"),
    );
    Ok(Redirect::temporary(&url))
}

async fn github_callback(
    State(board): State<SharedBoard>,
    jar: CookieJar,
    Query(q): Query<GithubCallbackQuery>,
) -> Result<(CookieJar, Redirect), (StatusCode, String)> {
    if let Some(err) = q.error {
        let detail = q.error_description.unwrap_or_default();
        return Err((
            StatusCode::BAD_REQUEST,
            format!("GitHub OAuth error: {err} {detail}"),
        ));
    }

    // App Setup URL / post-install redirect: `?installation_id=&setup_action=install`.
    // GitHub may also send a user `code` here; that is not our Sign-in CSRF flow.
    let setup = q
        .setup_action
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if matches!(setup, Some("install") | Some("update")) || q.installation_id.is_some() {
        let Some(id) = q.installation_id.filter(|&n| n > 0) else {
            return Err((
                StatusCode::BAD_REQUEST,
                "GitHub App setup missing installation_id".into(),
            ));
        };
        board.set_github_app_installation_id(Some(id));
        // Best-effort mint into OpenShell `github` — install page should not fail if
        // the gateway is down; Settings → Mint / sync remains the retry.
        let board_bg = board.clone();
        tokio::spawn(async move {
            if let Err(e) = crate::github_app::ensure_github_provider(&board_bg).await {
                tracing::warn!("post-install GitHub App token sync: {e}");
            }
        });
        return Ok((
            jar,
            Redirect::temporary("/?github_app=installed"),
        ));
    }

    let Some(code) = q.code.filter(|s| !s.is_empty()) else {
        return Err((StatusCode::BAD_REQUEST, "missing code".into()));
    };
    let Some(state) = q.state.filter(|s| !s.is_empty()) else {
        return Err((StatusCode::BAD_REQUEST, "missing state".into()));
    };
    let Some(auth) = board.auth_bundle() else {
        return Err((StatusCode::BAD_REQUEST, "bootstrap required".into()));
    };
    let key = auth
        .session_key_bytes()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let Some(oauth) = decode_oauth_state(&key, &state) else {
        return Err((StatusCode::BAD_REQUEST, "invalid OAuth state".into()));
    };
    let Some(app) = board.github_app_bundle() else {
        return Err((StatusCode::BAD_REQUEST, "GitHub App not configured".into()));
    };

    let token = exchange_code(
        app.client_id.trim(),
        app.client_secret.trim(),
        &code,
        &oauth.ru,
    )
    .await
    .map_err(|e| (StatusCode::BAD_GATEWAY, e))?;

    let login = github_login(&token)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e))?;

    let allowed = github_user_allowed(&board, &token, &login)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e))?;
    if !allowed {
        // Land on the same origin as the authorize redirect_uri when possible.
        let home = oauth
            .ru
            .trim_end_matches("/auth/github/callback")
            .to_string();
        return Ok((
            jar,
            Redirect::temporary(&format!("{home}/?auth_error=not_allowlisted")),
        ));
    }

    let user = SessionUser {
        kind: SessionKind::Github,
        login,
    };
    let session = encode_session(&key, &user, SESSION_TTL_SECS)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    let home = oauth
        .ru
        .trim_end_matches("/auth/github/callback")
        .to_string();
    let dest = match oauth.next.as_deref().and_then(sanitize_login_next) {
        Some(path) => format!("{home}{path}"),
        None => format!("{home}/"),
    };
    Ok((
        jar.add(session_cookie(session)),
        Redirect::temporary(&dest),
    ))
}

fn urlencoding_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

async fn exchange_code(
    client_id: &str,
    client_secret: &str,
    code: &str,
    redirect_uri: &str,
) -> Result<String, String> {
    #[derive(Serialize)]
    struct Body<'a> {
        client_id: &'a str,
        client_secret: &'a str,
        code: &'a str,
        redirect_uri: &'a str,
    }
    #[derive(Deserialize)]
    struct TokenResp {
        access_token: Option<String>,
        error: Option<String>,
        error_description: Option<String>,
    }
    let client = reqwest::Client::new();
    let resp = client
        .post("https://github.com/login/oauth/access_token")
        .header(header::ACCEPT, "application/json")
        .json(&Body {
            client_id,
            client_secret,
            code,
            redirect_uri,
        })
        .send()
        .await
        .map_err(|e| format!("token exchange: {e}"))?;
    let body: TokenResp = resp
        .json()
        .await
        .map_err(|e| format!("token response: {e}"))?;
    if let Some(err) = body.error {
        return Err(format!(
            "{err}: {}",
            body.error_description.unwrap_or_default()
        ));
    }
    body.access_token
        .filter(|t| !t.is_empty())
        .ok_or_else(|| "no access_token in response".into())
}

async fn github_login(token: &str) -> Result<String, String> {
    #[derive(Deserialize)]
    struct User {
        login: String,
    }
    let client = reqwest::Client::new();
    let user: User = client
        .get("https://api.github.com/user")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::USER_AGENT, "sandboard")
        .header(header::ACCEPT, "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| format!("github user: {e}"))?
        .error_for_status()
        .map_err(|e| format!("github user status: {e}"))?
        .json()
        .await
        .map_err(|e| format!("github user json: {e}"))?;
    Ok(user.login)
}

/// Pure allowlist check for usernames (no network).
pub fn user_in_allowlist(login: &str, allowed_users: &[String]) -> bool {
    let login = login.trim();
    if login.is_empty() {
        return false;
    }
    allowed_users
        .iter()
        .any(|u| u.trim().eq_ignore_ascii_case(login))
}

fn parse_team(spec: &str) -> Option<(&str, &str)> {
    let spec = spec.trim();
    let (org, team) = spec.split_once('/')?;
    let org = org.trim();
    let team = team.trim();
    if org.is_empty() || team.is_empty() || team.contains('/') {
        return None;
    }
    Some((org, team))
}

async fn team_member(token: &str, org: &str, team_slug: &str, username: &str) -> Result<bool, String> {
    let url = format!(
        "https://api.github.com/orgs/{org}/teams/{team_slug}/memberships/{username}"
    );
    let client = reqwest::Client::new();
    let resp = client
        .get(&url)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::USER_AGENT, "sandboard")
        .header(header::ACCEPT, "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| format!("team membership: {e}"))?;
    match resp.status().as_u16() {
        200 => {
            #[derive(Deserialize)]
            struct Membership {
                state: String,
            }
            let m: Membership = resp
                .json()
                .await
                .map_err(|e| format!("team membership json: {e}"))?;
            Ok(m.state == "active")
        }
        404 => Ok(false),
        other => Err(format!("team membership HTTP {other}")),
    }
}

async fn github_user_allowed(
    board: &SharedBoard,
    token: &str,
    login: &str,
) -> Result<bool, String> {
    if user_in_allowlist(login, &board.auth_allowed_users()) {
        return Ok(true);
    }
    for spec in board.auth_allowed_teams() {
        let Some((org, team)) = parse_team(&spec) else {
            continue;
        };
        if team_member(token, org, team, login).await? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Any authenticated operator (local admin or GitHub). No RBAC yet.
fn require_operator(
    user: Option<&SessionUser>,
) -> Result<&SessionUser, (StatusCode, Json<serde_json::Value>)> {
    match user {
        Some(u) => Ok(u),
        None => Err((
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({ "error": "authentication required" })),
        )),
    }
}

async fn get_auth_settings(
    State(board): State<SharedBoard>,
    jar: CookieJar,
    headers: HeaderMap,
) -> Result<Json<AuthSettingsView>, (StatusCode, Json<serde_json::Value>)> {
    let user = operator_from_jar_or_basic(&board, &jar, &headers);
    require_operator(user.as_ref())?;
    let auth = board.auth_bundle().ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "bootstrap required", "bootstrap": true })),
        )
    })?;
    let gh = board.github_app_bundle();
    Ok(Json(AuthSettingsView {
        admin_username: auth.admin_username,
        allowed_users: board.auth_allowed_users(),
        allowed_teams: board.auth_allowed_teams(),
        github_login_enabled: github_login_enabled(&board),
        has_client_secret: gh
            .as_ref()
            .map(|b| !b.client_secret.trim().is_empty())
            .unwrap_or(false),
    }))
}

async fn put_auth_settings(
    State(board): State<SharedBoard>,
    jar: CookieJar,
    headers: HeaderMap,
    Json(body): Json<AuthSettingsWrite>,
) -> Result<Json<AuthSettingsView>, (StatusCode, Json<serde_json::Value>)> {
    let user = operator_from_jar_or_basic(&board, &jar, &headers);
    require_operator(user.as_ref())?;
    let mut auth = board.auth_bundle().ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "bootstrap required", "bootstrap": true })),
        )
    })?;

    if let Some(users) = body.allowed_users {
        let teams = body
            .allowed_teams
            .clone()
            .unwrap_or_else(|| board.auth_allowed_teams());
        board.set_auth_allowlists(users, teams);
    } else if let Some(teams) = body.allowed_teams {
        board.set_auth_allowlists(board.auth_allowed_users(), teams);
    }

    if let Some(pw) = body.new_password {
        let pw = pw.trim();
        if !pw.is_empty() {
            if pw.len() < 8 {
                return Err((
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({
                        "error": "password must be at least 8 characters"
                    })),
                ));
            }
            auth.password_hash = hash_password(pw).map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({ "error": e })),
                )
            })?;
            let sealed = seal_auth(&auth).map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({ "error": format!("seal auth: {e}") })),
                )
            })?;
            board.set_auth_sealed(Some(sealed));
        }
    }

    let auth = board.auth_bundle().ok_or_else(|| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": "auth missing after update" })),
        )
    })?;
    let gh = board.github_app_bundle();
    Ok(Json(AuthSettingsView {
        admin_username: auth.admin_username,
        allowed_users: board.auth_allowed_users(),
        allowed_teams: board.auth_allowed_teams(),
        github_login_enabled: github_login_enabled(&board),
        has_client_secret: gh
            .as_ref()
            .map(|b| !b.client_secret.trim().is_empty())
            .unwrap_or(false),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::master_key_env;
    use crate::store::Board;
    use std::sync::Arc;

    #[test]
    fn user_allowlist_case_insensitive() {
        assert!(user_in_allowlist(
            "ShaneMCD",
            &["shanemcd".into(), "other".into()]
        ));
        assert!(!user_in_allowlist("nobody", &["shanemcd".into()]));
    }

    #[test]
    fn parse_team_ok() {
        assert_eq!(parse_team("acme/eng"), Some(("acme", "eng")));
        assert!(parse_team("nope").is_none());
    }

    #[test]
    fn session_round_trip() {
        let key = [9u8; 32];
        let user = SessionUser {
            kind: SessionKind::Admin,
            login: "admin".into(),
        };
        let tok = encode_session(&key, &user, 60).unwrap();
        let got = decode_session(&key, &tok).unwrap();
        assert_eq!(got, user);
        assert!(decode_session(&[0u8; 32], &tok).is_none());
    }

    #[test]
    fn oauth_state_round_trip_embeds_redirect() {
        let key = [3u8; 32];
        let ru = "http://localhost:5173/auth/github/callback";
        let state = encode_oauth_state(&key, ru, None).unwrap();
        let got = decode_oauth_state(&key, &state).unwrap();
        assert_eq!(got.ru, ru);
        assert!(decode_oauth_state(&[0u8; 32], &state).is_none());
    }

    #[test]
    fn public_base_url_prefers_forwarded_host() {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-host", "localhost:5173".parse().unwrap());
        headers.insert("x-forwarded-proto", "http".parse().unwrap());
        headers.insert(header::HOST, "127.0.0.1:8080".parse().unwrap());
        assert_eq!(public_base_url(&headers), "http://localhost:5173");
    }

    #[test]
    fn return_origin_wins_for_redirect_uri() {
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, "127.0.0.1:8080".parse().unwrap());
        assert_eq!(
            redirect_uri_for_start(&headers, Some("http://localhost:5173")),
            "http://localhost:5173/auth/github/callback"
        );
        assert!(sanitize_return_origin("https://evil.example").is_none());
        assert!(sanitize_return_origin("http://localhost:5173/evil").is_none());
    }

    #[tokio::test]
    async fn bootstrap_login_and_reject_second_bootstrap() {
        let hex = "ef".repeat(32);
        let _env = master_key_env::Guard::with_hex_key(&hex);
        let dir = std::env::temp_dir().join(format!(
            "sandboard-auth-boot-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::create_dir_all(&dir);
        let board = Arc::new(Board::new(
            crate::schema::Schema::default(),
            dir.join("board.json"),
        ));

        let jar = CookieJar::new();
        let (jar, Json(st)) = bootstrap(
            State(board.clone()),
            jar,
            Json(BootstrapBody {
                username: "admin".into(),
                password: "password123".into(),
            }),
        )
        .await
        .expect("bootstrap");
        assert!(st.enabled);
        assert!(!st.bootstrap);
        assert_eq!(st.user.as_ref().map(|u| u.login.as_str()), Some("admin"));
        assert!(jar.get(SESSION_COOKIE).is_some());

        let err = bootstrap(
            State(board.clone()),
            CookieJar::new(),
            Json(BootstrapBody {
                username: "other".into(),
                password: "password123".into(),
            }),
        )
        .await
        .expect_err("second bootstrap");
        assert_eq!(err.0, StatusCode::CONFLICT);

        let (jar2, Json(st2)) = login(
            State(board.clone()),
            CookieJar::new(),
            Json(LoginBody {
                username: "admin".into(),
                password: "password123".into(),
            }),
        )
        .await
        .expect("login");
        assert!(st2.user.is_some());
        assert!(jar2.get(SESSION_COOKIE).is_some());

        let bad = login(
            State(board.clone()),
            CookieJar::new(),
            Json(LoginBody {
                username: "admin".into(),
                password: "wrong-password".into(),
            }),
        )
        .await
        .expect_err("bad login");
        assert_eq!(bad.0, StatusCode::UNAUTHORIZED);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn path_exempt_rules() {
        assert!(path_exempt("/auth/status"));
        assert!(path_exempt("/api/webhooks/github"));
        assert!(!path_exempt("/mcp"));
        assert!(path_exempt("/.well-known/oauth-protected-resource"));
        assert!(path_exempt("/.well-known/oauth-authorization-server"));
        assert!(path_exempt("/oauth/register"));
        assert!(path_exempt("/oauth/token"));
        assert!(path_exempt("/oauth/authorize"));
        assert!(path_exempt("/healthz"));
        assert!(path_exempt("/llms.txt"));
        assert!(!path_exempt("/api/board"));
        assert!(!path_exempt("/api/auth/settings"));
    }

    #[tokio::test]
    async fn app_install_callback_saves_installation_without_oauth_state() {
        let hex = "a1".repeat(32);
        let _env = master_key_env::Guard::with_hex_key(&hex);
        let dir = std::env::temp_dir().join(format!(
            "sandboard-auth-install-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::create_dir_all(&dir);
        let board = Arc::new(Board::new(
            crate::schema::Schema::default(),
            dir.join("board.json"),
        ));

        let (_jar, _redirect) = github_callback(
            State(board.clone()),
            CookieJar::new(),
            Query(GithubCallbackQuery {
                code: Some("unused_user_code".into()),
                state: None,
                error: None,
                error_description: None,
                installation_id: Some(151366427),
                setup_action: Some("install".into()),
            }),
        )
        .await
        .expect("install callback");
        assert_eq!(board.github_app_installation_id(), Some(151366427));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn api_board_requires_session_after_bootstrap() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::util::ServiceExt;

        let hex = "f0".repeat(32);
        let _env = master_key_env::Guard::with_hex_key(&hex);
        let dir = std::env::temp_dir().join(format!(
            "sandboard-auth-gate-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::create_dir_all(&dir);
        let board = Arc::new(Board::new(
            crate::schema::Schema::default(),
            dir.join("board.json"),
        ));

        let app = Router::new()
            .nest("/auth", routes())
            .nest("/api", crate::api::routes())
            .layer(axum::middleware::from_fn_with_state(
                board.clone(),
                require_session,
            ))
            .with_state(board.clone());

        let unauth = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/board")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unauth.status(), StatusCode::UNAUTHORIZED);

        let boot = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/auth/bootstrap")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"username":"admin","password":"password123"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(boot.status(), StatusCode::OK);
        let set_cookie = boot
            .headers()
            .get(header::SET_COOKIE)
            .and_then(|v| v.to_str().ok())
            .expect("session cookie")
            .to_string();
        let cookie = set_cookie.split(';').next().unwrap();

        let authed = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/board")
                    .header(header::COOKIE, cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(authed.status(), StatusCode::OK);

        let basic = base64::engine::general_purpose::STANDARD.encode("admin:password123");
        let via_basic = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/board")
                    .header(header::AUTHORIZATION, format!("Basic {basic}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(via_basic.status(), StatusCode::OK);

        let bad_basic = base64::engine::general_purpose::STANDARD.encode("admin:wrong-password");
        let rejected = app
            .oneshot(
                Request::builder()
                    .uri("/api/board")
                    .header(header::AUTHORIZATION, format!("Basic {bad_basic}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(rejected.status(), StatusCode::UNAUTHORIZED);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
