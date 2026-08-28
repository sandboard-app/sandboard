//! MCP OAuth 2.1 — co-located authorization server + resource server for `/mcp`.
//!
//! Cursor (and other HTTP MCP clients) cannot send session cookies, so `/mcp`
//! is gated with Bearer tokens discovered via Protected Resource Metadata
//! rather than the web session middleware.
//!
//! Access and refresh tokens are JWTs HMAC-signed with the admin session key
//! (persisted in the auth bundle). Auth codes and DCR clients stay in process
//! memory — that is fine; refresh must survive `cargo run` restarts so Cursor
//! is not forced through browser login every boot.

use crate::auth;
use crate::store::SharedBoard;

use axum::extract::{Form, Query, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use axum_extra::extract::cookie::CookieJar;
use base64::Engine;
use jsonwebtoken::{decode, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation};
use parking_lot::Mutex;
use rand::RngExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

const SCOPE: &str = "mcp";
const ACCESS_TTL_SECS: u64 = 60 * 60;
const REFRESH_TTL_SECS: u64 = 60 * 60 * 24 * 30;
const CODE_TTL_SECS: u64 = 600;
const CLIENT_NAME_MAX: usize = 128;

/// Stable public client for Cursor CLI / IDE (`auth.CLIENT_ID` in mcp.json).
/// Lives in process memory like other DCR clients, but is re-seeded on use so
/// restarts do not invalidate a checked-in CLIENT_ID.
pub const CURSOR_CLIENT_ID: &str = "sandboard-cursor";

/// Public client for the sandboxed cockpit / Cockpit inject path.
/// Tokens are minted server-side (no browser OAuth inside the box).
pub const COCKPIT_CLIENT_ID: &str = "sandboard-cockpit";

/// Default MCP URL as seen from inside the cockpit sandbox (Docker/OpenShell).
/// Access token lifetime (seconds) — exported for inject metadata.
pub const MCP_ACCESS_TTL_SECS: u64 = ACCESS_TTL_SECS;

fn cursor_redirect_uris() -> Vec<String> {
    vec![
        "http://localhost:8787/callback".into(),
        "http://127.0.0.1:8787/callback".into(),
        "cursor://anysphere.cursor-mcp/oauth/callback".into(),
        "https://www.cursor.com/agents/mcp/oauth/callback".into(),
    ]
}

fn ensure_static_clients(st: &mut OAuthStore) {
    st.clients
        .entry(CURSOR_CLIENT_ID.to_string())
        .or_insert_with(|| ClientRecord {
            redirect_uris: cursor_redirect_uris(),
            client_name: Some("Cursor (sandboard)".into()),
        });
    st.clients
        .entry(COCKPIT_CLIENT_ID.to_string())
        .or_insert_with(|| ClientRecord {
            // No browser redirect — mint path only. Placeholder keeps DCR shape.
            redirect_uris: vec!["http://127.0.0.1/sandboard-cockpit-no-redirect".into()],
            client_name: Some("sandboard cockpit (injected)".into()),
        });
}

/// Tokens minted for the cockpit sandbox MCP client (never return refresh to browsers).
#[derive(Debug, Clone, Serialize)]
pub struct OpsMcpTokens {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_in: u64,
    pub expires_at: u64,
    pub resource: String,
    pub client_id: String,
    pub sub: String,
}

/// Informational `aud` for the (vestigial) cockpit JWT. Cockpit's shipped
/// `sandboard` MCP entry is stdio over a local Unix socket now — nothing sends
/// this Bearer over a wire. See `cockpit_mcp_tunnel`.
pub fn cockpit_mcp_resource() -> String {
    crate::cockpit_mcp_tunnel::MCP_TRANSPORT_LABEL.to_string()
}

/// Mint access + refresh JWTs for the cockpit (`sandboard-cockpit` client).
///
/// `sub` is the board principal (logged-in login, or `"cockpit"` for supervisor
/// fallback). `resource` should be the URL the sandbox uses (see
/// [`cockpit_mcp_resource`]).
pub fn mint_cockpit_seat_tokens(
    board: &SharedBoard,
    sub: &str,
    resource: &str,
) -> Result<OpsMcpTokens, String> {
    let sub = sub.trim();
    if sub.is_empty() {
        return Err("sub required".into());
    }
    let resource = resource.trim().trim_end_matches('/');
    if resource.is_empty() {
        return Err("resource required".into());
    }
    {
        let mut st = store().lock();
        ensure_static_clients(&mut st);
    }
    let access = mint_access_token(board, sub, resource)?;
    let refresh = mint_refresh_token(board, sub, COCKPIT_CLIENT_ID, resource)?;
    let now = now_secs();
    Ok(OpsMcpTokens {
        access_token: access,
        refresh_token: refresh,
        expires_in: MCP_ACCESS_TTL_SECS,
        expires_at: now.saturating_add(MCP_ACCESS_TTL_SECS),
        resource: resource.to_string(),
        client_id: COCKPIT_CLIENT_ID.to_string(),
        sub: sub.to_string(),
    })
}

/// Test / inject helper: verify an access token against the cockpit resource.
pub fn verify_cockpit_access_token(board: &SharedBoard, token: &str, resource: &str) -> Option<String> {
    verify_access_token(board, token, resource)
}

#[derive(Clone)]
struct ClientRecord {
    redirect_uris: Vec<String>,
    client_name: Option<String>,
}

#[derive(Clone)]
struct CodeRecord {
    client_id: String,
    redirect_uri: String,
    code_challenge: String,
    resource: String,
    sub: String,
    exp: u64,
}

#[derive(Default)]
struct OAuthStore {
    clients: HashMap<String, ClientRecord>,
    /// One-time authorization codes (short-lived; OK to lose on restart).
    codes: HashMap<String, CodeRecord>,
}

fn store() -> &'static Mutex<OAuthStore> {
    static STORE: std::sync::OnceLock<Mutex<OAuthStore>> = std::sync::OnceLock::new();
    STORE.get_or_init(|| {
        let mut st = OAuthStore::default();
        ensure_static_clients(&mut st);
        Mutex::new(st)
    })
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn random_token() -> String {
    let mut raw = [0u8; 32];
    rand::rng().fill(&mut raw);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw)
}

/// Public origin for OAuth redirect_uri / metadata issuer.
///
/// Prefer the origin the browser actually used (Vite/Tailscale/proxy), not the
/// backend bind address and not a process-wide `SANDBOARD_PUBLIC_URL` override.
/// MCP client OAuth (Atlassian, etc.) must bounce back to the tab that started
/// login — env public URL is only a fallback when the request has no Host /
/// Origin / X-Forwarded-* (or when those are empty).
/// Never invents `127.0.0.1:8080`.
pub fn public_origin(headers: &HeaderMap) -> String {
    let proto = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("http")
        .split(',')
        .next()
        .unwrap_or("http")
        .trim();
    if let Some(host) = headers
        .get("x-forwarded-host")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.split(',').next().unwrap_or(s).trim())
        .filter(|s| !s.is_empty())
    {
        return format!("{proto}://{host}");
    }
    if let Some(origin) = headers
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != "null")
    {
        return origin.to_string();
    }
    if let Some(referer) = headers.get(header::REFERER).and_then(|v| v.to_str().ok()) {
        if let Ok(u) = referer.parse::<axum::http::Uri>() {
            if let Some(auth) = u.authority() {
                let scheme = u.scheme_str().unwrap_or(proto);
                return format!("{scheme}://{auth}");
            }
        }
    }
    if let Some(host) = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return format!("{proto}://{host}");
    }
    std::env::var("SANDBOARD_PUBLIC_URL")
        .ok()
        .map(|s| s.trim().trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_default()
}

fn canonical_resource(origin: &str) -> String {
    format!("{}/mcp", origin.trim_end_matches('/'))
}

/// `localhost` and `127.0.0.1` (same port/path) are the same MCP resource.
fn resources_equivalent(a: &str, b: &str) -> bool {
    normalize_resource(a) == normalize_resource(b)
}

fn normalize_resource(raw: &str) -> String {
    let Ok(uri) = raw.trim().parse::<axum::http::Uri>() else {
        return raw.trim().to_string();
    };
    let host = uri.host().unwrap_or("");
    let host = if host == "localhost" || host == "127.0.0.1" || host == "[::1]" || host == "::1" {
        "loopback"
    } else {
        host
    };
    let port = uri.port_u16();
    let path = uri.path().trim_end_matches('/');
    match port {
        Some(p) => format!("{host}:{p}{path}"),
        None => format!("{host}{path}"),
    }
}

fn auth_configured(board: &SharedBoard) -> bool {
    board.auth_bundle().is_some()
}

fn resource_metadata_url(origin: &str) -> String {
    format!(
        "{}/.well-known/oauth-protected-resource",
        origin.trim_end_matches('/')
    )
}

fn www_authenticate(origin: &str) -> HeaderValue {
    let meta = resource_metadata_url(origin);
    HeaderValue::from_str(&format!(
        "Bearer resource_metadata=\"{meta}\", scope=\"{SCOPE}\""
    ))
    .unwrap_or_else(|_| HeaderValue::from_static("Bearer"))
}

fn unauthorized_mcp(origin: &str) -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, www_authenticate(origin))],
        Json(serde_json::json!({ "error": "invalid_token" })),
    )
        .into_response()
}

#[derive(Debug, Serialize, Deserialize)]
struct AccessClaims {
    sub: String,
    aud: String,
    scope: String,
    exp: u64,
    iat: u64,
    #[serde(rename = "typ")]
    typ: String,
}

/// Refresh JWT — same signing key as access / web sessions so a sandboard restart
/// does not wipe Cursor's stored refresh token.
#[derive(Debug, Serialize, Deserialize)]
struct RefreshClaims {
    sub: String,
    aud: String,
    client_id: String,
    scope: String,
    exp: u64,
    iat: u64,
    #[serde(rename = "typ")]
    typ: String,
}

fn session_hmac_key(board: &SharedBoard) -> Result<Vec<u8>, String> {
    let auth = board
        .auth_bundle()
        .ok_or_else(|| "auth not configured".to_string())?;
    auth.session_key_bytes().map_err(|e| e.to_string())
}

fn mint_access_token(
    board: &SharedBoard,
    sub: &str,
    resource: &str,
) -> Result<String, String> {
    let key = session_hmac_key(board)?;
    let now = now_secs();
    let claims = AccessClaims {
        sub: sub.to_string(),
        aud: resource.to_string(),
        scope: SCOPE.to_string(),
        exp: now.saturating_add(ACCESS_TTL_SECS),
        iat: now,
        typ: "mcp_access".into(),
    };
    let mut header = Header::new(Algorithm::HS256);
    header.typ = Some("JWT".into());
    encode(&header, &claims, &EncodingKey::from_secret(&key)).map_err(|e| e.to_string())
}

fn mint_refresh_token(
    board: &SharedBoard,
    sub: &str,
    client_id: &str,
    resource: &str,
) -> Result<String, String> {
    let key = session_hmac_key(board)?;
    let now = now_secs();
    let claims = RefreshClaims {
        sub: sub.to_string(),
        aud: resource.to_string(),
        client_id: client_id.to_string(),
        scope: SCOPE.to_string(),
        exp: now.saturating_add(REFRESH_TTL_SECS),
        iat: now,
        typ: "mcp_refresh".into(),
    };
    let mut header = Header::new(Algorithm::HS256);
    header.typ = Some("JWT".into());
    encode(&header, &claims, &EncodingKey::from_secret(&key)).map_err(|e| e.to_string())
}

fn verify_access_token(board: &SharedBoard, token: &str, resource: &str) -> Option<String> {
    let key = session_hmac_key(board).ok()?;
    let mut validation = Validation::new(Algorithm::HS256);
    validation.validate_aud = false;
    validation.set_required_spec_claims(&["exp", "sub"]);
    let data = decode::<AccessClaims>(token, &DecodingKey::from_secret(&key), &validation).ok()?;
    if data.claims.typ != "mcp_access" {
        return None;
    }
    if data.claims.scope != SCOPE {
        return None;
    }
    if !resources_equivalent(&data.claims.aud, resource) {
        return None;
    }
    Some(data.claims.sub)
}

fn verify_refresh_token(
    board: &SharedBoard,
    token: &str,
    client_id: &str,
) -> Option<RefreshClaims> {
    let key = session_hmac_key(board).ok()?;
    let mut validation = Validation::new(Algorithm::HS256);
    validation.validate_aud = false;
    validation.set_required_spec_claims(&["exp", "sub"]);
    let data = decode::<RefreshClaims>(token, &DecodingKey::from_secret(&key), &validation).ok()?;
    if data.claims.typ != "mcp_refresh" {
        return None;
    }
    if data.claims.scope != SCOPE {
        return None;
    }
    if data.claims.client_id != client_id {
        return None;
    }
    Some(data.claims)
}

/// Axum middleware: require Bearer when admin auth is configured.
pub async fn require_mcp_bearer(
    State(board): State<SharedBoard>,
    headers: HeaderMap,
    req: Request,
    next: Next,
) -> Response {
    // CORS preflight must reach the MCP stack unauthenticated.
    if req.method() == Method::OPTIONS {
        return next.run(req).await;
    }
    if !auth_configured(&board) {
        return next.run(req).await;
    }
    let origin = public_origin(&headers);
    let resource = canonical_resource(&origin);
    // Also accept the alternate loopback spelling in Host.
    let alt_origin = if origin.contains("localhost") {
        origin.replacen("localhost", "127.0.0.1", 1)
    } else if origin.contains("127.0.0.1") {
        origin.replacen("127.0.0.1", "localhost", 1)
    } else {
        origin.clone()
    };
    let alt_resource = canonical_resource(&alt_origin);

    let Some(authz) = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()) else {
        return unauthorized_mcp(&origin);
    };
    let Some(token) = authz
        .strip_prefix("Bearer ")
        .or_else(|| authz.strip_prefix("bearer "))
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return unauthorized_mcp(&origin);
    };
    if verify_access_token(&board, token, &resource).is_none()
        && verify_access_token(&board, token, &alt_resource).is_none()
    {
        return unauthorized_mcp(&origin);
    }
    next.run(req).await
}

pub fn well_known_routes() -> Router<SharedBoard> {
    Router::new()
        .route(
            "/oauth-protected-resource",
            get(protected_resource_metadata),
        )
        .route(
            "/oauth-protected-resource/mcp",
            get(protected_resource_metadata),
        )
        .route(
            "/oauth-authorization-server",
            get(authorization_server_metadata),
        )
}

pub fn oauth_routes() -> Router<SharedBoard> {
    Router::new()
        .route("/register", post(register_client))
        .route("/authorize", get(authorize_get).post(authorize_post))
        .route("/token", post(token))
}

async fn protected_resource_metadata(
    headers: HeaderMap,
) -> Json<serde_json::Value> {
    let origin = public_origin(&headers);
    let resource = canonical_resource(&origin);
    Json(serde_json::json!({
        "resource": resource,
        "authorization_servers": [origin],
        "scopes_supported": [SCOPE],
        "bearer_methods_supported": ["header"],
    }))
}

async fn authorization_server_metadata(
    headers: HeaderMap,
) -> Json<serde_json::Value> {
    let origin = public_origin(&headers);
    Json(serde_json::json!({
        "issuer": origin,
        "authorization_endpoint": format!("{origin}/oauth/authorize"),
        "token_endpoint": format!("{origin}/oauth/token"),
        "registration_endpoint": format!("{origin}/oauth/register"),
        "response_types_supported": ["code"],
        "grant_types_supported": ["authorization_code", "refresh_token"],
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["none"],
        "scopes_supported": [SCOPE],
        "client_id_metadata_document_supported": true,
    }))
}

#[derive(Debug, Deserialize)]
struct RegisterBody {
    #[serde(default)]
    redirect_uris: Vec<String>,
    #[serde(default)]
    client_name: Option<String>,
    #[serde(default)]
    token_endpoint_auth_method: Option<String>,
    #[serde(default)]
    grant_types: Option<Vec<String>>,
    #[serde(default)]
    response_types: Option<Vec<String>>,
}

fn redirect_uri_allowed(uri: &str) -> bool {
    let uri = uri.trim();
    if uri.is_empty() {
        return false;
    }
    if uri == "https://www.cursor.com/agents/mcp/oauth/callback" {
        return true;
    }
    if uri.starts_with("cursor://") {
        return true;
    }
    let Ok(parsed) = uri.parse::<axum::http::Uri>() else {
        return false;
    };
    let scheme = parsed.scheme_str().unwrap_or("");
    if scheme != "http" && scheme != "https" {
        return false;
    }
    let host = parsed.host().unwrap_or("");
    host == "localhost" || host == "127.0.0.1" || host == "[::1]" || host == "::1"
}

async fn register_client(
    Json(body): Json<RegisterBody>,
) -> Result<(StatusCode, Json<serde_json::Value>), (StatusCode, Json<serde_json::Value>)> {
    if body.redirect_uris.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "invalid_client_metadata",
                "error_description": "redirect_uris required",
            })),
        ));
    }
    for uri in &body.redirect_uris {
        if !redirect_uri_allowed(uri) {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": "invalid_redirect_uri",
                    "error_description": format!("redirect_uri not allowed: {uri}"),
                })),
            ));
        }
    }
    if let Some(m) = body.token_endpoint_auth_method.as_deref() {
        if m != "none" {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": "invalid_client_metadata",
                    "error_description": "only token_endpoint_auth_method=none is supported",
                })),
            ));
        }
    }
    let client_id = format!("sandboard-{}", &random_token()[..16]);
    let name = body
        .client_name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.chars().take(CLIENT_NAME_MAX).collect::<String>());
    {
        let mut st = store().lock();
        st.clients.insert(
            client_id.clone(),
            ClientRecord {
                redirect_uris: body.redirect_uris.clone(),
                client_name: name.clone(),
            },
        );
    }
    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({
            "client_id": client_id,
            "client_id_issued_at": now_secs(),
            "redirect_uris": body.redirect_uris,
            "client_name": name,
            "token_endpoint_auth_method": "none",
            "grant_types": body.grant_types.unwrap_or_else(|| {
                vec!["authorization_code".into(), "refresh_token".into()]
            }),
            "response_types": body.response_types.unwrap_or_else(|| vec!["code".into()]),
        })),
    ))
}

#[derive(Debug, Deserialize, Clone)]
pub struct AuthorizeQuery {
    pub response_type: Option<String>,
    pub client_id: Option<String>,
    pub redirect_uri: Option<String>,
    pub state: Option<String>,
    pub code_challenge: Option<String>,
    pub code_challenge_method: Option<String>,
    pub resource: Option<String>,
    pub scope: Option<String>,
}

async fn resolve_client(client_id: &str) -> Result<ClientRecord, String> {
    {
        let mut st = store().lock();
        ensure_static_clients(&mut st);
        if let Some(c) = st.clients.get(client_id) {
            return Ok(c.clone());
        }
    }
    // Client ID Metadata Documents (HTTPS URL as client_id).
    if client_id.starts_with("https://") {
        return fetch_client_metadata(client_id).await;
    }
    Err("unknown client_id".into())
}

async fn fetch_client_metadata(url: &str) -> Result<ClientRecord, String> {
    #[derive(Deserialize)]
    struct Doc {
        client_id: String,
        #[serde(default)]
        client_name: Option<String>,
        redirect_uris: Vec<String>,
    }
    let client = reqwest::Client::new();
    let resp = client
        .get(url)
        .header(header::ACCEPT, "application/json")
        .send()
        .await
        .map_err(|e| format!("fetch client metadata: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("client metadata HTTP {}", resp.status()));
    }
    let doc: Doc = resp
        .json()
        .await
        .map_err(|e| format!("client metadata json: {e}"))?;
    if doc.client_id != url {
        return Err("client_id in metadata must match URL".into());
    }
    for uri in &doc.redirect_uris {
        if !redirect_uri_allowed(uri) {
            return Err(format!("redirect_uri not allowed: {uri}"));
        }
    }
    let record = ClientRecord {
        redirect_uris: doc.redirect_uris,
        client_name: doc.client_name,
    };
    store().lock().clients.insert(url.to_string(), record.clone());
    Ok(record)
}

fn validate_authorize_request(
    q: &AuthorizeQuery,
    client: &ClientRecord,
    headers: &HeaderMap,
) -> Result<(String, String, String), String> {
    if q.response_type.as_deref() != Some("code") {
        return Err("response_type must be code".into());
    }
    let redirect_uri = q
        .redirect_uri
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "redirect_uri required".to_string())?
        .to_string();
    if !client.redirect_uris.iter().any(|u| u == &redirect_uri) {
        return Err("redirect_uri not registered for client".into());
    }
    let method = q.code_challenge_method.as_deref().unwrap_or("");
    if method != "S256" {
        return Err("code_challenge_method must be S256".into());
    }
    let challenge = q
        .code_challenge
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "code_challenge required".to_string())?
        .to_string();
    let origin = public_origin(headers);
    let default_resource = canonical_resource(&origin);
    let resource = q
        .resource
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(default_resource.as_str())
        .to_string();
    if !resources_equivalent(&resource, &default_resource)
        && !resources_equivalent(
            &resource,
            &canonical_resource(&if origin.contains("localhost") {
                origin.replacen("localhost", "127.0.0.1", 1)
            } else {
                origin.replacen("127.0.0.1", "localhost", 1)
            }),
        )
    {
        return Err("resource does not match this MCP server".into());
    }
    if let Some(scope) = q.scope.as_deref() {
        for part in scope.split_whitespace() {
            if part != SCOPE {
                return Err(format!("unsupported scope: {part}"));
            }
        }
    }
    Ok((redirect_uri, challenge, resource))
}

fn html_escape(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '&' => "&amp;".into(),
            '<' => "&lt;".into(),
            '>' => "&gt;".into(),
            '"' => "&quot;".into(),
            '\'' => "&#39;".into(),
            _ => c.to_string(),
        })
        .collect()
}

fn authorize_query_string(q: &AuthorizeQuery) -> String {
    let mut parts = Vec::new();
    let push = |parts: &mut Vec<String>, k: &str, v: Option<&String>| {
        if let Some(v) = v.map(|s| s.as_str()).filter(|s| !s.is_empty()) {
            parts.push(format!("{k}={}", urlencoding_encode(v)));
        }
    };
    push(&mut parts, "response_type", q.response_type.as_ref());
    push(&mut parts, "client_id", q.client_id.as_ref());
    push(&mut parts, "redirect_uri", q.redirect_uri.as_ref());
    push(&mut parts, "state", q.state.as_ref());
    push(&mut parts, "code_challenge", q.code_challenge.as_ref());
    push(
        &mut parts,
        "code_challenge_method",
        q.code_challenge_method.as_ref(),
    );
    push(&mut parts, "resource", q.resource.as_ref());
    push(&mut parts, "scope", q.scope.as_ref());
    parts.join("&")
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

async fn authorize_get(
    State(board): State<SharedBoard>,
    jar: CookieJar,
    headers: HeaderMap,
    Query(q): Query<AuthorizeQuery>,
) -> Response {
    let Some(client_id) = q.client_id.as_deref().map(str::trim).filter(|s| !s.is_empty()) else {
        return (StatusCode::BAD_REQUEST, "client_id required").into_response();
    };
    let client = match resolve_client(client_id).await {
        Ok(c) => c,
        Err(e) => return (StatusCode::BAD_REQUEST, e).into_response(),
    };
    if let Err(e) = validate_authorize_request(&q, &client, &headers) {
        return (StatusCode::BAD_REQUEST, e).into_response();
    }

    let Some(user) = auth::session_user_from_jar(&board, &jar) else {
        let next = format!("/oauth/authorize?{}", authorize_query_string(&q));
        return Redirect::temporary(&format!("/?next={}", urlencoding_encode(&next)))
            .into_response();
    };

    let name = client
        .client_name
        .as_deref()
        .unwrap_or(client_id);
    // Base64 so `&` in the query string survives HTML attribute + form POST.
    let payload_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(authorize_query_string(&q).as_bytes());
    let body = format!(
        r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><title>Authorize MCP — sandboard</title>
<style>
  body {{ font-family: system-ui, sans-serif; max-width: 28rem; margin: 3rem auto; padding: 0 1rem;
         color: #1a2428; background: #f5faf6; }}
  h1 {{ font-size: 1.35rem; }}
  .card {{ background: #fff; border: 1px solid #d5dde0; border-radius: 12px; padding: 1.25rem; }}
  .dim {{ color: #5a6a72; font-size: 0.9rem; }}
  .btns {{ display: flex; gap: 0.75rem; margin-top: 1.25rem; }}
  button {{ flex: 1; padding: 0.65rem 1rem; border-radius: 8px; border: 1px solid #c5ced3;
           background: #fff; font-weight: 600; cursor: pointer; }}
  button.primary {{ background: #2377d2; border-color: #2377d2; color: #fff; }}
</style></head><body>
  <div class="card">
    <h1>Authorize MCP access</h1>
    <p class="dim">Signed in as <strong>{login}</strong>.</p>
    <p><strong>{name}</strong> wants to use the sandboard board via MCP.</p>
    <form method="post" action="/oauth/authorize">
      <input type="hidden" name="decision" value="approve" />
      <input type="hidden" name="payload" value="{payload}" />
      <div class="btns">
        <button type="submit" class="primary">Approve</button>
      </div>
    </form>
    <form method="post" action="/oauth/authorize" style="margin-top:0.5rem">
      <input type="hidden" name="decision" value="deny" />
      <input type="hidden" name="payload" value="{payload}" />
      <div class="btns">
        <button type="submit">Deny</button>
      </div>
    </form>
  </div>
</body></html>"#,
        login = html_escape(&user.login),
        name = html_escape(name),
        payload = payload_b64,
    );
    Html(body).into_response()
}

#[derive(Debug, Deserialize)]
struct AuthorizeForm {
    decision: String,
    payload: String,
}

fn parse_authorize_payload(payload: &str) -> AuthorizeQuery {
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim())
        .ok()
        .and_then(|b| String::from_utf8(b).ok())
        .unwrap_or_else(|| payload.to_string());
    let mut q = AuthorizeQuery {
        response_type: None,
        client_id: None,
        redirect_uri: None,
        state: None,
        code_challenge: None,
        code_challenge_method: None,
        resource: None,
        scope: None,
    };
    for pair in decoded.split('&') {
        let Some((k, v)) = pair.split_once('=') else {
            continue;
        };
        let v = urlencoding_decode(v);
        match k {
            "response_type" => q.response_type = Some(v),
            "client_id" => q.client_id = Some(v),
            "redirect_uri" => q.redirect_uri = Some(v),
            "state" => q.state = Some(v),
            "code_challenge" => q.code_challenge = Some(v),
            "code_challenge_method" => q.code_challenge_method = Some(v),
            "resource" => q.resource = Some(v),
            "scope" => q.scope = Some(v),
            _ => {}
        }
    }
    q
}

fn urlencoding_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let h = || {
                    let hi = (bytes[i + 1] as char).to_digit(16)?;
                    let lo = (bytes[i + 2] as char).to_digit(16)?;
                    Some(((hi << 4) | lo) as u8)
                };
                if let Some(b) = h() {
                    out.push(b);
                    i += 3;
                } else {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

async fn authorize_post(
    State(board): State<SharedBoard>,
    jar: CookieJar,
    headers: HeaderMap,
    Form(form): Form<AuthorizeForm>,
) -> Response {
    let Some(user) = auth::session_user_from_jar(&board, &jar) else {
        return (StatusCode::UNAUTHORIZED, "login required").into_response();
    };
    let q = parse_authorize_payload(&form.payload);
    let Some(client_id) = q.client_id.clone() else {
        return (StatusCode::BAD_REQUEST, "client_id required").into_response();
    };
    let client = match resolve_client(&client_id).await {
        Ok(c) => c,
        Err(e) => return (StatusCode::BAD_REQUEST, e).into_response(),
    };
    let (redirect_uri, challenge, resource) =
        match validate_authorize_request(&q, &client, &headers) {
            Ok(v) => v,
            Err(e) => return (StatusCode::BAD_REQUEST, e).into_response(),
        };

    if form.decision != "approve" {
        let mut url = format!("{redirect_uri}?error=access_denied");
        if let Some(state) = q.state.as_deref() {
            url.push_str("&state=");
            url.push_str(&urlencoding_encode(state));
        }
        // 303 after POST so the browser GETs the loopback callback (Cursor
        // only accepts GET; 307 would replay POST → Method Not Allowed).
        return Redirect::to(&url).into_response();
    }

    let code = random_token();
    {
        let mut st = store().lock();
        st.codes.insert(
            code.clone(),
            CodeRecord {
                client_id,
                redirect_uri: redirect_uri.clone(),
                code_challenge: challenge,
                resource,
                sub: user.login.clone(),
                exp: now_secs().saturating_add(CODE_TTL_SECS),
            },
        );
    }
    let mut url = format!("{redirect_uri}?code={}", urlencoding_encode(&code));
    if let Some(state) = q.state.as_deref() {
        url.push_str("&state=");
        url.push_str(&urlencoding_encode(state));
    }
    Redirect::to(&url).into_response()
}

#[derive(Debug, Deserialize)]
struct TokenForm {
    grant_type: Option<String>,
    code: Option<String>,
    redirect_uri: Option<String>,
    client_id: Option<String>,
    code_verifier: Option<String>,
    refresh_token: Option<String>,
    resource: Option<String>,
}

fn pkce_s256_challenge(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
}

fn oauth_error(status: StatusCode, err: &str, desc: &str) -> Response {
    (
        status,
        Json(serde_json::json!({
            "error": err,
            "error_description": desc,
        })),
    )
        .into_response()
}

async fn token(
    State(board): State<SharedBoard>,
    headers: HeaderMap,
    req: Request,
) -> Response {
    if !auth_configured(&board) {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "bootstrap local admin before MCP OAuth",
        );
    }

    let form = match parse_token_body(req).await {
        Ok(f) => f,
        Err(e) => return oauth_error(StatusCode::BAD_REQUEST, "invalid_request", &e),
    };

    match form.grant_type.as_deref() {
        Some("authorization_code") => token_authorization_code(&board, &headers, form).await,
        Some("refresh_token") => token_refresh(&board, &headers, form).await,
        _ => oauth_error(
            StatusCode::BAD_REQUEST,
            "unsupported_grant_type",
            "use authorization_code or refresh_token",
        ),
    }
}

async fn parse_token_body(req: Request) -> Result<TokenForm, String> {
    let ct = req
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let bytes = axum::body::to_bytes(req.into_body(), 1024 * 1024)
        .await
        .map_err(|e| e.to_string())?;
    if ct.contains("application/json") {
        serde_json::from_slice(&bytes).map_err(|e| e.to_string())
    } else {
        serde_urlencoded::from_bytes(&bytes).map_err(|e| e.to_string())
    }
}

async fn token_authorization_code(
    board: &SharedBoard,
    headers: &HeaderMap,
    form: TokenForm,
) -> Response {
    let code = form.code.as_deref().unwrap_or("").trim();
    let client_id = form.client_id.as_deref().unwrap_or("").trim();
    let redirect_uri = form.redirect_uri.as_deref().unwrap_or("").trim();
    let verifier = form.code_verifier.as_deref().unwrap_or("").trim();
    if code.is_empty() || client_id.is_empty() || redirect_uri.is_empty() || verifier.is_empty() {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "code, client_id, redirect_uri, and code_verifier required",
        );
    }

    let record = {
        let mut st = store().lock();
        st.codes.remove(code)
    };
    let Some(record) = record else {
        return oauth_error(StatusCode::BAD_REQUEST, "invalid_grant", "unknown code");
    };
    if record.exp < now_secs() {
        return oauth_error(StatusCode::BAD_REQUEST, "invalid_grant", "code expired");
    }
    if record.client_id != client_id || record.redirect_uri != redirect_uri {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_grant",
            "code does not match client_id/redirect_uri",
        );
    }
    if pkce_s256_challenge(verifier) != record.code_challenge {
        return oauth_error(StatusCode::BAD_REQUEST, "invalid_grant", "pkce failed");
    }

    let origin = public_origin(headers);
    let default_resource = canonical_resource(&origin);
    let resource = form
        .resource
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(record.resource.as_str());
    if !resources_equivalent(resource, &record.resource)
        && !resources_equivalent(resource, &default_resource)
    {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_target",
            "resource mismatch",
        );
    }
    let resource = resource.to_string();

    issue_tokens(board, client_id, &record.sub, &resource)
}

async fn token_refresh(board: &SharedBoard, headers: &HeaderMap, form: TokenForm) -> Response {
    let refresh = form.refresh_token.as_deref().unwrap_or("").trim();
    let client_id = form.client_id.as_deref().unwrap_or("").trim();
    if refresh.is_empty() || client_id.is_empty() {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "refresh_token and client_id required",
        );
    }
    let Some(claims) = verify_refresh_token(board, refresh, client_id) else {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_grant",
            "invalid or expired refresh_token",
        );
    };
    let origin = public_origin(headers);
    let default_resource = canonical_resource(&origin);
    let resource = form
        .resource
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(claims.aud.as_str());
    if !resources_equivalent(resource, &claims.aud)
        && !resources_equivalent(resource, &default_resource)
    {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_target",
            "resource mismatch",
        );
    }
    issue_tokens(board, client_id, &claims.sub, resource)
}

fn issue_tokens(board: &SharedBoard, client_id: &str, sub: &str, resource: &str) -> Response {
    let access = match mint_access_token(board, sub, resource) {
        Ok(t) => t,
        Err(e) => {
            return oauth_error(StatusCode::INTERNAL_SERVER_ERROR, "server_error", &e);
        }
    };
    let refresh = match mint_refresh_token(board, sub, client_id, resource) {
        Ok(t) => t,
        Err(e) => {
            return oauth_error(StatusCode::INTERNAL_SERVER_ERROR, "server_error", &e);
        }
    };
    Json(serde_json::json!({
        "access_token": access,
        "token_type": "Bearer",
        "expires_in": ACCESS_TTL_SECS,
        "refresh_token": refresh,
        "scope": SCOPE,
    }))
    .into_response()
}

/// Test helper: clear ephemeral OAuth state between cases.
#[cfg(test)]
pub fn reset_store_for_tests() {
    let mut st = store().lock();
    *st = OAuthStore::default();
}

/// Test helper: mint a Bearer for integration tests.
#[cfg(test)]
pub fn mint_test_access_token(board: &SharedBoard, sub: &str, resource: &str) -> String {
    mint_access_token(board, sub, resource).expect("mint")
}

/// Test helper: mint a refresh JWT (survives process-memory store clear).
#[cfg(test)]
fn mint_test_refresh_token(
    board: &SharedBoard,
    sub: &str,
    client_id: &str,
    resource: &str,
) -> String {
    mint_refresh_token(board, sub, client_id, resource).expect("mint refresh")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{SessionKind, SessionUser};
    use crate::secrets::{seal_auth, AuthBundle};
    use crate::store::Board;
    use axum::body::Body;
    use axum::http::Request;
    use std::sync::Arc;
    use tower::ServiceExt;

    fn board_with_admin() -> (SharedBoard, crate::secrets::master_key_env::Guard) {
        let hex = "ab".repeat(32);
        let env = crate::secrets::master_key_env::Guard::with_hex_key(&hex);
        let path = std::env::temp_dir().join(format!(
            "sandboard-mcp-oauth-{}-{}.json",
            std::process::id(),
            now_secs()
        ));
        let board = Arc::new(Board::new(crate::schema::Schema::default(), path));
        let bundle = AuthBundle {
            admin_username: "admin".into(),
            password_hash: "unused-for-jwt-tests".into(),
            session_key_b64: base64::engine::general_purpose::STANDARD.encode([7u8; 32]),
        };
        let sealed = seal_auth(&bundle).expect("seal");
        board.set_auth_sealed(Some(sealed));
        (board, env)
    }

    #[test]
    fn loopback_resources_match() {
        assert!(resources_equivalent(
            "http://127.0.0.1:8080/mcp",
            "http://localhost:8080/mcp"
        ));
        assert!(!resources_equivalent(
            "http://127.0.0.1:8080/mcp",
            "http://127.0.0.1:8081/mcp"
        ));
    }

    #[test]
    fn public_origin_uses_browser_headers_not_invented_loopback() {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-host", "localhost:5173".parse().unwrap());
        headers.insert("x-forwarded-proto", "http".parse().unwrap());
        headers.insert(header::HOST, "127.0.0.1:9999".parse().unwrap());
        assert_eq!(public_origin(&headers), "http://localhost:5173");

        let mut headers = HeaderMap::new();
        headers.insert(header::ORIGIN, "https://tot.example:5173".parse().unwrap());
        headers.insert(header::HOST, "127.0.0.1:9999".parse().unwrap());
        assert_eq!(public_origin(&headers), "https://tot.example:5173");

        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, "localhost:9090".parse().unwrap());
        assert_eq!(public_origin(&headers), "http://localhost:9090");
    }

    /// Guards MCP client OAuth (Atlassian, etc.): a process-wide public URL
    /// must not steal redirect_uri away from the tab that started login.
    #[test]
    fn public_origin_prefers_browser_over_sandboard_public_url_env() {
        let prev = std::env::var("SANDBOARD_PUBLIC_URL").ok();
        // SAFETY: test-only; serial enough for this crate's unit tests.
        unsafe {
            std::env::set_var("SANDBOARD_PUBLIC_URL", "https://sandboard.example.ts.net");
        }

        let mut headers = HeaderMap::new();
        headers.insert(header::ORIGIN, "https://tot.example:5173".parse().unwrap());
        headers.insert(header::HOST, "127.0.0.1:8080".parse().unwrap());
        assert_eq!(public_origin(&headers), "https://tot.example:5173");

        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-host", "tot.example:5173".parse().unwrap());
        headers.insert("x-forwarded-proto", "https".parse().unwrap());
        assert_eq!(public_origin(&headers), "https://tot.example:5173");

        assert_eq!(
            public_origin(&HeaderMap::new()),
            "https://sandboard.example.ts.net"
        );

        unsafe {
            match prev {
                Some(v) => std::env::set_var("SANDBOARD_PUBLIC_URL", v),
                None => std::env::remove_var("SANDBOARD_PUBLIC_URL"),
            }
        }
    }

    #[test]
    fn mint_cockpit_seat_tokens_round_trips_access_verify() {
        let (board, _env) = board_with_admin();
        let resource = crate::cockpit_mcp_tunnel::MCP_TRANSPORT_LABEL;
        let tokens = mint_cockpit_seat_tokens(&board, "admin", resource).expect("mint");
        assert_eq!(tokens.client_id, COCKPIT_CLIENT_ID);
        assert_eq!(tokens.sub, "admin");
        assert_eq!(tokens.resource, resource);
        assert_eq!(
            verify_cockpit_access_token(&board, &tokens.access_token, resource).as_deref(),
            Some("admin")
        );
        // Refresh must not verify as access.
        assert!(verify_cockpit_access_token(&board, &tokens.refresh_token, resource).is_none());
    }

    #[test]
    fn pkce_challenge_matches_rfc() {
        // RFC 7636 appendix B
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(
            pkce_s256_challenge(verifier),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[tokio::test]
    async fn bearer_gate_401_when_auth_configured() {
        reset_store_for_tests();
        let (board, _env) = board_with_admin();
        let app = Router::new()
            .route("/mcp", get(|| async { "ok" }))
            .layer(axum::middleware::from_fn_with_state(
                board.clone(),
                require_mcp_bearer,
            ))
            .with_state(board);

        let res = app
            .oneshot(
                Request::builder()
                    .uri("/mcp")
                    .header(header::HOST, "127.0.0.1:8080")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        let www = res
            .headers()
            .get(header::WWW_AUTHENTICATE)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(www.contains("resource_metadata="));
        assert!(www.contains("scope=\"mcp\""));
    }

    #[tokio::test]
    async fn bearer_gate_allows_valid_token() {
        reset_store_for_tests();
        let (board, _env) = board_with_admin();
        let token = mint_test_access_token(&board, "admin", "http://127.0.0.1:8080/mcp");
        let app = Router::new()
            .route("/mcp", get(|| async { "ok" }))
            .layer(axum::middleware::from_fn_with_state(
                board.clone(),
                require_mcp_bearer,
            ))
            .with_state(board);

        let res = app
            .oneshot(
                Request::builder()
                    .uri("/mcp")
                    .header(header::HOST, "127.0.0.1:8080")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn bootstrap_board_skips_bearer_gate() {
        reset_store_for_tests();
        let path = std::env::temp_dir().join(format!(
            "sandboard-mcp-oauth-boot-{}-{}.json",
            std::process::id(),
            now_secs()
        ));
        let board = Arc::new(Board::new(crate::schema::Schema::default(), path));
        let app = Router::new()
            .route("/mcp", get(|| async { "ok" }))
            .layer(axum::middleware::from_fn_with_state(
                board.clone(),
                require_mcp_bearer,
            ))
            .with_state(board);

        let res = app
            .oneshot(
                Request::builder()
                    .uri("/mcp")
                    .header(header::HOST, "127.0.0.1:8080")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn pkce_code_flow_issues_bearer() {
        reset_store_for_tests();
        let (board, _env) = board_with_admin();
        let app = Router::new()
            .nest("/.well-known", well_known_routes())
            .nest("/oauth", oauth_routes())
            .nest(
                "/mcp",
                Router::new()
                    .route("/", get(|| async { "ok" }))
                    .layer(axum::middleware::from_fn_with_state(
                        board.clone(),
                        require_mcp_bearer,
                    )),
            )
            .with_state(board.clone());

        let prm = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/.well-known/oauth-protected-resource")
                    .header(header::HOST, "127.0.0.1:8080")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(prm.status(), StatusCode::OK);
        let prm_body: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(prm.into_body(), 1024 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(prm_body["resource"], "http://127.0.0.1:8080/mcp");
        assert!(prm_body["authorization_servers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "http://127.0.0.1:8080"));

        let reg = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/oauth/register")
                    .header(header::HOST, "127.0.0.1:8080")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"redirect_uris":["http://localhost:8787/callback"],"client_name":"test","token_endpoint_auth_method":"none"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(reg.status(), StatusCode::CREATED);
        let reg_body: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(reg.into_body(), 1024 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        let client_id = reg_body["client_id"].as_str().unwrap().to_string();

        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let challenge = pkce_s256_challenge(verifier);
        let session = crate::auth::mint_session_cookie_value(
            &board,
            &SessionUser {
                kind: SessionKind::Admin,
                login: "admin".into(),
            },
        )
        .unwrap();

        let authz = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/oauth/authorize")
                    .header(header::HOST, "127.0.0.1:8080")
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .header(header::COOKIE, format!("sandboard_session={session}"))
                    .body(Body::from({
                        let q = AuthorizeQuery {
                            response_type: Some("code".into()),
                            client_id: Some(client_id.clone()),
                            redirect_uri: Some("http://localhost:8787/callback".into()),
                            state: Some("xyz".into()),
                            code_challenge: Some(challenge),
                            code_challenge_method: Some("S256".into()),
                            resource: Some("http://127.0.0.1:8080/mcp".into()),
                            scope: Some("mcp".into()),
                        };
                        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
                            .encode(authorize_query_string(&q).as_bytes());
                        format!("decision=approve&payload={payload}")
                    }))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(authz.status(), StatusCode::SEE_OTHER);
        let loc = authz
            .headers()
            .get(header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(loc.starts_with("http://localhost:8787/callback?code="));
        let code = loc
            .split("code=")
            .nth(1)
            .unwrap()
            .split('&')
            .next()
            .unwrap()
            .to_string();

        let token_res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/oauth/token")
                    .header(header::HOST, "127.0.0.1:8080")
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(format!(
                        "grant_type=authorization_code&code={code}&redirect_uri=http%3A%2F%2Flocalhost%3A8787%2Fcallback&client_id={client_id}&code_verifier={verifier}&resource=http%3A%2F%2F127.0.0.1%3A8080%2Fmcp"
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(token_res.status(), StatusCode::OK);
        let token_body: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(token_res.into_body(), 1024 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        let access = token_body["access_token"].as_str().unwrap();

        let bad_resource = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/oauth/token")
                    .header(header::HOST, "127.0.0.1:8080")
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(
                        "grant_type=refresh_token&refresh_token=nope&client_id=x&resource=http://evil.example/mcp",
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(bad_resource.status(), StatusCode::BAD_REQUEST);

        let mcp = app
            .oneshot(
                Request::builder()
                    .uri("/mcp")
                    .header(header::HOST, "127.0.0.1:8080")
                    .header(header::AUTHORIZATION, format!("Bearer {access}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(mcp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn token_rejects_resource_mismatch_on_code_exchange() {
        reset_store_for_tests();
        let (board, _env) = board_with_admin();
        let app = Router::new()
            .nest("/oauth", oauth_routes())
            .with_state(board.clone());

        let reg = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/oauth/register")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"redirect_uris":["http://127.0.0.1:8787/callback"]}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        let reg_body: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(reg.into_body(), 1024 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        let client_id = reg_body["client_id"].as_str().unwrap().to_string();
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let challenge = pkce_s256_challenge(verifier);
        let session = crate::auth::mint_session_cookie_value(
            &board,
            &SessionUser {
                kind: SessionKind::Admin,
                login: "admin".into(),
            },
        )
        .unwrap();
        let q = AuthorizeQuery {
            response_type: Some("code".into()),
            client_id: Some(client_id.clone()),
            redirect_uri: Some("http://127.0.0.1:8787/callback".into()),
            state: None,
            code_challenge: Some(challenge),
            code_challenge_method: Some("S256".into()),
            resource: Some("http://127.0.0.1:8080/mcp".into()),
            scope: None,
        };
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(authorize_query_string(&q).as_bytes());
        let authz = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/oauth/authorize")
                    .header(header::HOST, "127.0.0.1:8080")
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .header(header::COOKIE, format!("sandboard_session={session}"))
                    .body(Body::from(format!("decision=approve&payload={payload}")))
                    .unwrap(),
            )
            .await
            .unwrap();
        let loc = authz
            .headers()
            .get(header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        let code = loc
            .split("code=")
            .nth(1)
            .unwrap()
            .split('&')
            .next()
            .unwrap();

        let token_res = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/oauth/token")
                    .header(header::HOST, "127.0.0.1:8080")
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(format!(
                        "grant_type=authorization_code&code={code}&redirect_uri=http%3A%2F%2F127.0.0.1%3A8787%2Fcallback&client_id={client_id}&code_verifier={verifier}&resource=http%3A%2F%2Fevil.example%2Fmcp"
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(token_res.status(), StatusCode::BAD_REQUEST);
        let body: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(token_res.into_body(), 1024 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["error"], "invalid_target");
    }

    /// Refresh JWTs are signed with the persisted session key — clearing the
    /// in-memory OAuth store (codes / DCR) must not force a new browser login.
    #[tokio::test]
    async fn refresh_token_survives_oauth_store_reset() {
        reset_store_for_tests();
        let (board, _env) = board_with_admin();
        let client_id = CURSOR_CLIENT_ID;
        let resource = "http://127.0.0.1:8080/mcp";
        let refresh = mint_test_refresh_token(&board, "admin", client_id, resource);

        // Simulate sandboard restart: ephemeral store wiped; auth bundle (session key) kept.
        reset_store_for_tests();

        let app = Router::new()
            .nest("/oauth", oauth_routes())
            .with_state(board.clone());

        let token_res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/oauth/token")
                    .header(header::HOST, "127.0.0.1:8080")
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(format!(
                        "grant_type=refresh_token&refresh_token={refresh}&client_id={client_id}&resource={}",
                        urlencoding_encode(resource)
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            token_res.status(),
            StatusCode::OK,
            "refresh must work after store reset"
        );
        let token_body: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(token_res.into_body(), 1024 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        let access = token_body["access_token"].as_str().expect("access_token");
        assert!(
            token_body["refresh_token"].as_str().is_some(),
            "rotate refresh JWT on use"
        );

        let mcp = Router::new()
            .route("/mcp", get(|| async { "ok" }))
            .layer(axum::middleware::from_fn_with_state(
                board.clone(),
                require_mcp_bearer,
            ))
            .with_state(board)
            .oneshot(
                Request::builder()
                    .uri("/mcp")
                    .header(header::HOST, "127.0.0.1:8080")
                    .header(header::AUTHORIZATION, format!("Bearer {access}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(mcp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn access_token_cannot_be_used_as_refresh() {
        reset_store_for_tests();
        let (board, _env) = board_with_admin();
        let access = mint_test_access_token(&board, "admin", "http://127.0.0.1:8080/mcp");
        let app = Router::new()
            .nest("/oauth", oauth_routes())
            .with_state(board);

        let token_res = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/oauth/token")
                    .header(header::HOST, "127.0.0.1:8080")
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(format!(
                        "grant_type=refresh_token&refresh_token={access}&client_id={CURSOR_CLIENT_ID}&resource=http%3A%2F%2F127.0.0.1%3A8080%2Fmcp"
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(token_res.status(), StatusCode::BAD_REQUEST);
    }
}
