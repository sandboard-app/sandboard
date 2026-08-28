//! GitHub App JWT + installation access tokens for sandbox `GH_TOKEN`.
//!
//! OpenShell has no App-native refresh strategy, so sandboard mints short-lived
//! installation tokens and upserts them into the gateway provider instance
//! [`PROVIDER_NAME`] (`github-app`, shipped type [`PROVIDER_TYPE`]).
//! Only `GH_TOKEN` is pushed to the gateway — never the App private key.
//!
//! Push / `report_pull_request` routing looks up `owner/repo` in the repo-access
//! cache and mints for **that** installation. [`CONFIG_INSTALLATION_ID`] stays
//! the fallback for the singleton [`PROVIDER_NAME`] provider — it is not the
//! routing source.

use crate::model::{
    EscalationOption, ItemId, OpenShellProviderDesired, WorkItem, GITHUB_APP_PROVIDER_TYPE,
};
use crate::secrets::{open_string_map, seal_string_map, GitHubAppBundle};
use crate::store::{Board, SharedBoard};

use chrono::{DateTime, Duration, Utc};
use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::time::Duration as StdDuration;

/// OpenShell provider **instance** name (sandbox attach name).
pub const PROVIDER_NAME: &str = "github-app";
/// Pre-rename instance name; board load rewrites attaches + provider rows.
pub const LEGACY_PROVIDER_NAME: &str = "github";
/// OpenShell builtin type used before the shipped `github-app` profile.
pub const LEGACY_BUILTIN_TYPE: &str = "github";
/// Shipped custom provider type (see `sandbox/openshell/github-app.yaml`).
pub const PROVIDER_TYPE: &str = GITHUB_APP_PROVIDER_TYPE;
/// Env / credential key sandboxes and `gh` expect (`gh` prefers this over `GITHUB_TOKEN`).
pub const CREDENTIAL_KEY: &str = "GH_TOKEN";
/// Board config: GitHub App numeric id (non-secret).
pub const CONFIG_APP_ID: &str = "GITHUB_APP_ID";
/// Board config: installation id that mints tokens (non-secret).
pub const CONFIG_INSTALLATION_ID: &str = "GITHUB_INSTALLATION_ID";
/// Board-only sealed credential: App private key PEM (never pushed to gateway).
pub const CRED_PRIVATE_KEY: &str = "GITHUB_APP_PRIVATE_KEY";
/// Board-only sealed: webhook secret (Access / Forge; not gateway).
pub const CRED_WEBHOOK_SECRET: &str = "GITHUB_APP_WEBHOOK_SECRET";
/// Board-only sealed: OAuth client id (Access; not gateway).
pub const CRED_CLIENT_ID: &str = "GITHUB_APP_CLIENT_ID";
/// Board-only sealed: OAuth client secret (Access; not gateway).
pub const CRED_CLIENT_SECRET: &str = "GITHUB_APP_CLIENT_SECRET";
/// Remint when this close to expiry (installation tokens last ~1h).
pub const REFRESH_SKEW: Duration = Duration::minutes(10);
/// How often the repo-access cache walks installations (visibility only).
pub const REPO_ACCESS_REFRESH_INTERVAL: StdDuration = StdDuration::from_secs(10 * 60);
/// GitHub page for installing the App / adding repositories to an installation.
pub const INSTALLATIONS_MANAGE_URL: &str = "https://github.com/settings/installations";
/// Board Settings surface that lists the cache and the install deep link.
pub const SETTINGS_REPO_ACCESS_PATH: &str = "/settings/repo-access";
/// Gateway provider instance for a cache-resolved installation (not the singleton).
pub const ROUTED_PROVIDER_PREFIX: &str = "github-app-install-";

/// Config keys that stay on the board and must not be sent to OpenShell.
pub fn board_only_config_keys() -> &'static [&'static str] {
    &[CONFIG_APP_ID, CONFIG_INSTALLATION_ID]
}

/// Credential keys that stay on the board and must not be sent to OpenShell.
pub fn board_only_credential_keys() -> &'static [&'static str] {
    &[
        CRED_PRIVATE_KEY,
        CRED_WEBHOOK_SECRET,
        CRED_CLIENT_ID,
        CRED_CLIENT_SECRET,
    ]
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("github app: {0}")]
    Config(String),
    #[error("jwt: {0}")]
    Jwt(String),
    #[error("github api: {0}")]
    Api(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InstallationInfo {
    pub id: u64,
    pub account_login: String,
    #[serde(default)]
    pub account_type: String,
}

/// One `owner/repo` row in the GitHub App access cache (later stages look this up).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GitHubRepoAccessEntry {
    pub installation_id: u64,
    #[serde(default)]
    pub permissions: BTreeMap<String, String>,
    pub last_seen_at: DateTime<Utc>,
}

/// Durable App installation → repo visibility cache. Stage 3 mints from this lookup.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct GitHubRepoAccessCache {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refreshed_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(default)]
    pub installations: Vec<InstallationInfo>,
    /// `owner/repo` → installation + permissions (GitHub `full_name` as returned).
    #[serde(default)]
    pub repos: BTreeMap<String, GitHubRepoAccessEntry>,
}

impl GitHubRepoAccessCache {
    /// Lookup `owner/repo` (case-insensitive). Later stages mint from this id.
    pub fn installation_id_for(&self, owner_repo: &str) -> Option<u64> {
        let key = owner_repo.trim();
        if key.is_empty() {
            return None;
        }
        if let Some(e) = self.repos.get(key) {
            return Some(e.installation_id);
        }
        self.repos
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, e)| e.installation_id)
    }
}

/// Repo listed by `GET /installation/repositories`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallationRepo {
    pub full_name: String,
    pub permissions: BTreeMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct InstallationToken {
    pub token: String,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Default)]
pub struct TokenCache {
    pub expires_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
}

impl TokenCache {
    pub fn needs_mint(&self, now: DateTime<Utc>) -> bool {
        match self.expires_at {
            None => true,
            Some(exp) => now + REFRESH_SKEW >= exp,
        }
    }
}

#[derive(Debug, Serialize)]
struct AppJwtClaims {
    iat: i64,
    exp: i64,
    iss: String,
}

/// RS256 App JWT (≤10 minutes). Used only as Bearer to App APIs.
pub fn make_app_jwt(bundle: &GitHubAppBundle, now: DateTime<Utc>) -> Result<String, Error> {
    let app_id = bundle.app_id.trim();
    if app_id.is_empty() {
        return Err(Error::Config("app_id empty".into()));
    }
    if bundle.private_key_pem.trim().is_empty() {
        return Err(Error::Config("private_key empty".into()));
    }
    let iat = now.timestamp() - 60;
    let exp = now.timestamp() + 9 * 60;
    let claims = AppJwtClaims {
        iat,
        exp,
        iss: app_id.to_string(),
    };
    let key = EncodingKey::from_rsa_pem(bundle.private_key_pem.as_bytes())
        .map_err(|e| Error::Jwt(format!("rsa pem: {e}")))?;
    let mut header = Header::new(Algorithm::RS256);
    header.typ = Some("JWT".into());
    encode(&header, &claims, &key).map_err(|e| Error::Jwt(e.to_string()))
}

fn api_base() -> String {
    std::env::var("SANDBOARD_GITHUB_API")
        .ok()
        .map(|s| s.trim().trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "https://api.github.com".into())
}

fn client() -> Result<reqwest::Client, Error> {
    reqwest::Client::builder()
        .user_agent("sandboard")
        .build()
        .map_err(|e| Error::Api(e.to_string()))
}

/// `GET /app/installations` — accounts where the App is installed.
pub async fn list_installations(jwt: &str) -> Result<Vec<InstallationInfo>, Error> {
    #[derive(Deserialize)]
    struct Account {
        login: String,
        #[serde(rename = "type")]
        account_type: Option<String>,
    }
    #[derive(Deserialize)]
    struct Row {
        id: u64,
        account: Option<Account>,
    }
    let url = format!("{}/app/installations", api_base());
    let resp = client()?
        .get(&url)
        .header(reqwest::header::AUTHORIZATION, format!("Bearer {jwt}"))
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| Error::Api(format!("list installations: {e}")))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(Error::Api(format!(
            "list installations HTTP {status}: {}",
            body.chars().take(200).collect::<String>()
        )));
    }
    let rows: Vec<Row> = resp
        .json()
        .await
        .map_err(|e| Error::Api(format!("list installations json: {e}")))?;
    Ok(rows
        .into_iter()
        .map(|r| InstallationInfo {
            id: r.id,
            account_login: r
                .account
                .as_ref()
                .map(|a| a.login.clone())
                .unwrap_or_default(),
            account_type: r
                .account
                .and_then(|a| a.account_type)
                .unwrap_or_default(),
        })
        .collect())
}

/// Deep link to manage one installation (add/remove repos) on GitHub.
pub fn installation_manage_url(account_login: &str, account_type: &str, id: u64) -> String {
    if account_type.eq_ignore_ascii_case("Organization") && !account_login.trim().is_empty() {
        format!(
            "https://github.com/organizations/{}/settings/installations/{id}",
            account_login.trim()
        )
    } else {
        format!("https://github.com/settings/installations/{id}")
    }
}

fn permissions_from_json(value: Option<&serde_json::Value>) -> BTreeMap<String, String> {
    let Some(serde_json::Value::Object(map)) = value else {
        return BTreeMap::new();
    };
    let mut out = BTreeMap::new();
    for (k, v) in map {
        let s = match v {
            serde_json::Value::Bool(true) => "true".to_string(),
            serde_json::Value::Bool(false) => "false".to_string(),
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        out.insert(k.clone(), s);
    }
    out
}

/// Fold one installation's repo list into a cache snapshot (pure mapping).
pub fn apply_installation_repos(
    cache: &mut GitHubRepoAccessCache,
    installation: &InstallationInfo,
    repos: &[InstallationRepo],
    now: DateTime<Utc>,
) {
    if !cache.installations.iter().any(|i| i.id == installation.id) {
        cache.installations.push(installation.clone());
    }
    for repo in repos {
        let full_name = repo.full_name.trim();
        if full_name.is_empty() || !full_name.contains('/') {
            continue;
        }
        cache.repos.insert(
            full_name.to_string(),
            GitHubRepoAccessEntry {
                installation_id: installation.id,
                permissions: repo.permissions.clone(),
                last_seen_at: now,
            },
        );
    }
}

/// `GET /installation/repositories` — repos this installation token can see.
///
/// Uses an installation access token (App JWT mint), never the sandbox `GH_TOKEN`
/// provider path. Paginates until a short page.
pub async fn list_installation_repositories(
    installation_token: &str,
) -> Result<Vec<InstallationRepo>, Error> {
    #[derive(Deserialize)]
    struct Page {
        #[serde(default)]
        repositories: Vec<serde_json::Value>,
    }
    let mut out = Vec::new();
    let mut page: u32 = 1;
    loop {
        let url = format!(
            "{}/installation/repositories?per_page=100&page={page}",
            api_base()
        );
        let resp = client()?
            .get(&url)
            .header(
                reqwest::header::AUTHORIZATION,
                format!("Bearer {installation_token}"),
            )
            .header(reqwest::header::ACCEPT, "application/vnd.github+json")
            .send()
            .await
            .map_err(|e| Error::Api(format!("list installation repos: {e}")))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::Api(format!(
                "list installation repos HTTP {status}: {}",
                body.chars().take(200).collect::<String>()
            )));
        }
        let body: Page = resp
            .json()
            .await
            .map_err(|e| Error::Api(format!("list installation repos json: {e}")))?;
        let n = body.repositories.len();
        for row in body.repositories {
            let full_name = row
                .get("full_name")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            let Some(full_name) = full_name else {
                continue;
            };
            out.push(InstallationRepo {
                full_name,
                permissions: permissions_from_json(row.get("permissions")),
            });
        }
        if n < 100 {
            break;
        }
        page = page.saturating_add(1);
        if page > 50 {
            break;
        }
    }
    Ok(out)
}

/// Walk every App installation and cache `owner/repo` → installation + permissions.
///
/// Mints per-installation tokens only to call `GET /installation/repositories`.
/// Does not call [`ensure_github_provider`] or write sandbox `GH_TOKEN`.
pub async fn refresh_repo_access_cache(board: &SharedBoard) -> Result<GitHubRepoAccessCache, Error> {
    let Some(bundle) = board.github_app_bundle() else {
        return Ok(board.github_repo_access_cache());
    };
    if bundle.app_id.trim().is_empty() || bundle.private_key_pem.trim().is_empty() {
        return Ok(board.github_repo_access_cache());
    }
    let jwt = make_app_jwt(&bundle, Utc::now())?;
    let installations = match list_installations(&jwt).await {
        Ok(list) => list,
        Err(e) => {
            let mut cache = board.github_repo_access_cache();
            cache.last_error = Some(e.to_string());
            board.set_github_repo_access_cache(cache.clone());
            return Err(e);
        }
    };

    let now = Utc::now();
    let mut next = GitHubRepoAccessCache {
        refreshed_at: Some(now),
        last_error: None,
        installations: Vec::new(),
        repos: BTreeMap::new(),
    };

    for inst in &installations {
        let token = match create_installation_token(&jwt, inst.id).await {
            Ok(t) => t,
            Err(e) => {
                let mut cache = board.github_repo_access_cache();
                cache.last_error = Some(format!("installation {}: {e}", inst.id));
                board.set_github_repo_access_cache(cache.clone());
                return Err(e);
            }
        };
        let repos = match list_installation_repositories(&token.token).await {
            Ok(r) => r,
            Err(e) => {
                let mut cache = board.github_repo_access_cache();
                cache.last_error = Some(format!("repos for installation {}: {e}", inst.id));
                board.set_github_repo_access_cache(cache.clone());
                return Err(e);
            }
        };
        apply_installation_repos(&mut next, inst, &repos, now);
    }

    board.set_github_repo_access_cache(next.clone());
    tracing::info!(
        installations = next.installations.len(),
        repos = next.repos.len(),
        "refreshed GitHub App repo-access cache"
    );
    Ok(next)
}

/// Background loop: refresh the repo-access cache on an interval.
pub async fn repo_access_refresh_loop(board: SharedBoard) {
    loop {
        match refresh_repo_access_cache(&board).await {
            Ok(_) => {}
            Err(e) => tracing::warn!("GitHub App repo-access cache refresh: {e}"),
        }
        tokio::time::sleep(REPO_ACCESS_REFRESH_INTERVAL).await;
    }
}

/// `POST /app/installations/{id}/access_tokens`.
pub async fn create_installation_token(
    jwt: &str,
    installation_id: u64,
) -> Result<InstallationToken, Error> {
    #[derive(Deserialize)]
    struct Resp {
        token: String,
        expires_at: String,
    }
    let url = format!(
        "{}/app/installations/{installation_id}/access_tokens",
        api_base()
    );
    let resp = client()?
        .post(&url)
        .header(reqwest::header::AUTHORIZATION, format!("Bearer {jwt}"))
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .json(&serde_json::json!({}))
        .send()
        .await
        .map_err(|e| Error::Api(format!("installation token: {e}")))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(Error::Api(format!(
            "installation token HTTP {status}: {}",
            body.chars().take(200).collect::<String>()
        )));
    }
    let body: Resp = resp
        .json()
        .await
        .map_err(|e| Error::Api(format!("installation token json: {e}")))?;
    let expires_at = DateTime::parse_from_rfc3339(&body.expires_at)
        .map(|d| d.with_timezone(&Utc))
        .map_err(|e| Error::Api(format!("expires_at: {e}")))?;
    if body.token.trim().is_empty() {
        return Err(Error::Api("empty installation token".into()));
    }
    Ok(InstallationToken {
        token: body.token,
        expires_at,
    })
}

/// Mint from sealed bundle + installation id.
pub async fn mint_installation_token(
    bundle: &GitHubAppBundle,
    installation_id: u64,
) -> Result<InstallationToken, Error> {
    let jwt = make_app_jwt(bundle, Utc::now())?;
    create_installation_token(&jwt, installation_id).await
}

/// Credential map pushed to the OpenShell `github-app` provider (`GH_TOKEN` only).
pub fn provider_credentials(token: &str) -> BTreeMap<String, String> {
    let mut m = BTreeMap::new();
    m.insert(CREDENTIAL_KEY.into(), token.to_string());
    m
}

/// Gateway config for `github-app`: strip board-only App mint fields.
pub fn gateway_config(config: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    config
        .iter()
        .filter(|(k, _)| !board_only_config_keys().contains(&k.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

/// Filter board sealed credentials down to what the gateway may see.
pub fn gateway_credentials(creds: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    creds
        .iter()
        .filter(|(k, v)| {
            !board_only_credential_keys().contains(&k.as_str()) && !v.trim().is_empty()
        })
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

/// Read App bundle from the `github-app` provider row (config + sealed map).
pub fn bundle_from_provider(board: &Board) -> Option<GitHubAppBundle> {
    let p = board
        .openshell_providers()
        .into_iter()
        .find(|p| p.name == PROVIDER_NAME)?;
    let map = p
        .credentials_sealed
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .and_then(|s| open_string_map(s).ok())
        .unwrap_or_default();
    let app_id = p
        .config
        .get(CONFIG_APP_ID)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_default();
    let private_key_pem = map
        .get(CRED_PRIVATE_KEY)
        .map(|s| s.to_string())
        .unwrap_or_default();
    if app_id.is_empty() && private_key_pem.is_empty() {
        return None;
    }
    Some(GitHubAppBundle {
        app_id,
        private_key_pem,
        webhook_secret: map
            .get(CRED_WEBHOOK_SECRET)
            .cloned()
            .unwrap_or_default(),
        client_id: map.get(CRED_CLIENT_ID).cloned().unwrap_or_default(),
        client_secret: map
            .get(CRED_CLIENT_SECRET)
            .cloned()
            .unwrap_or_default(),
    })
}

/// Installation id from provider config (preferred) or legacy board field.
pub fn installation_id_from_provider(board: &Board) -> Option<u64> {
    let p = board
        .openshell_providers()
        .into_iter()
        .find(|p| p.name == PROVIDER_NAME)?;
    p.config
        .get(CONFIG_INSTALLATION_ID)
        .and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|&n| n > 0)
}

/// Presence flags derived from the provider row (or legacy sealed blob).
pub fn status_from_board(board: &Board) -> crate::secrets::GitHubAppStatus {
    if let Some(b) = bundle_from_provider(board) {
        return crate::secrets::GitHubAppStatus::from(&b);
    }
    crate::secrets::github_app_status_from_sealed(board.github_app_sealed().as_deref())
}

/// Mint (if needed) and upsert the OpenShell `github-app` provider with a live token.
///
/// No-op when App credentials or installation_id are unset (returns Ok(false)).
/// Returns Ok(true) when the gateway provider was refreshed (or confirmed fresh).
pub async fn ensure_github_provider(board: &SharedBoard) -> Result<bool, Error> {
    let Some(bundle) = board.github_app_bundle() else {
        return Ok(false);
    };
    if !bundle.app_id.trim().is_empty() && bundle.private_key_pem.trim().is_empty() {
        return Err(Error::Config("GitHub App private key missing".into()));
    }
    if bundle.app_id.trim().is_empty() || bundle.private_key_pem.trim().is_empty() {
        return Ok(false);
    }
    let Some(installation_id) = board.github_app_installation_id() else {
        return Ok(false);
    };

    let cache = board.github_app_token_cache();
    let now = Utc::now();
    ensure_desired_row(board, None)?;

    // Sweeper calls this every tick — stay quiet when the cache is fresh and
    // the gateway already has GH_TOKEN without a leftover GITHUB_TOKEN.
    if !cache.needs_mint(now) {
        if !gateway_github_provider_needs_push(board).await? {
            return Ok(true);
        }
        if let Some(token) = sealed_github_token(board)? {
            ensure_desired_row(board, Some(&token))?;
            push_github_provider_on_gateway(board, &token).await?;
            tracing::info!(
                installation_id,
                "repaired OpenShell `{PROVIDER_NAME}` provider"
            );
            return Ok(true);
        }
        // Cache claimed fresh but nothing sealed — fall through to remint.
    }

    let minted = match mint_installation_token(&bundle, installation_id).await {
        Ok(t) => t,
        Err(e) => {
            board.set_github_app_token_cache(TokenCache {
                expires_at: cache.expires_at,
                last_error: Some(e.to_string()),
            });
            return Err(e);
        }
    };
    ensure_desired_row(board, Some(&minted.token))?;
    push_github_provider_on_gateway(board, &minted.token).await?;

    board.set_github_app_token_cache(TokenCache {
        expires_at: Some(minted.expires_at),
        last_error: None,
    });
    tracing::info!(
        installation_id,
        expires_at = %minted.expires_at,
        "synced GitHub App installation token to OpenShell provider `{PROVIDER_NAME}`"
    );
    Ok(true)
}

/// True when the gateway is missing `github-app`, lacks `GH_TOKEN`, or still has
/// a leftover `GITHUB_TOKEN` credential key.
async fn gateway_github_provider_needs_push(board: &SharedBoard) -> Result<bool, Error> {
    let os = board.openshell_client();
    let list = os
        .list_providers()
        .await
        .map_err(|e| Error::Api(format!("openshell list providers: {e}")))?;
    let Some(p) = list.iter().find(|p| p.name == PROVIDER_NAME) else {
        return Ok(true);
    };
    let has_gh = p.credential_keys.iter().any(|k| k == CREDENTIAL_KEY);
    let has_legacy = p.credential_keys.iter().any(|k| k == "GITHUB_TOKEN");
    Ok(!has_gh || has_legacy)
}

/// Create or update the gateway provider. Never create when it already exists
/// (that races the sweeper and logs "provider already exists").
async fn push_github_provider_on_gateway(board: &SharedBoard, token: &str) -> Result<(), Error> {
    let desired = board
        .openshell_providers()
        .into_iter()
        .find(|p| p.name == PROVIDER_NAME)
        .ok_or_else(|| Error::Config("github-app provider missing after upsert".into()))?;
    let os = board.openshell_client();
    let exists = os
        .list_providers()
        .await
        .map_err(|e| Error::Api(format!("openshell list providers: {e}")))?
        .iter()
        .any(|p| p.name == PROVIDER_NAME);
    let config = gateway_config(&desired.config);

    if exists {
        let mut credentials = provider_credentials(token);
        // Empty value clears a merged leftover from older PAT / App syncs.
        credentials.insert("GITHUB_TOKEN".into(), String::new());
        os.update_provider(PROVIDER_NAME, PROVIDER_TYPE, credentials, config)
            .await
            .map_err(|e| Error::Api(format!("openshell update {PROVIDER_NAME} provider: {e}")))?;
    } else {
        os.create_provider(
            PROVIDER_NAME,
            PROVIDER_TYPE,
            provider_credentials(token),
            config,
        )
        .await
        .map_err(|e| Error::Api(format!("openshell create {PROVIDER_NAME} provider: {e}")))?;
    }
    Ok(())
}

fn sealed_github_token(board: &SharedBoard) -> Result<Option<String>, Error> {
    let Some(p) = board
        .openshell_providers()
        .into_iter()
        .find(|p| p.name == PROVIDER_NAME)
    else {
        return Ok(None);
    };
    let Some(sealed) = p.credentials_sealed.as_deref() else {
        return Ok(None);
    };
    let map = open_string_map(sealed).map_err(|e| Error::Config(format!("open GH_TOKEN: {e}")))?;
    if let Some(t) = map.get(CREDENTIAL_KEY).filter(|t| !t.is_empty()) {
        return Ok(Some(t.clone()));
    }
    // Migrate one-shot from older App syncs that sealed GITHUB_TOKEN.
    if let Some(t) = map.get("GITHUB_TOKEN").filter(|t| !t.is_empty()) {
        return Ok(Some(t.clone()));
    }
    Ok(None)
}

/// Merge `fresh_token` into the existing sealed map (preserve App private key).
fn ensure_desired_row(board: &SharedBoard, fresh_token: Option<&str>) -> Result<(), Error> {
    let existing = board
        .openshell_providers()
        .into_iter()
        .find(|p| p.name == PROVIDER_NAME);

    let mut map = existing
        .as_ref()
        .and_then(|e| e.credentials_sealed.as_deref())
        .filter(|s| !s.trim().is_empty())
        .map(|s| open_string_map(s).map_err(|e| Error::Config(format!("open credentials: {e}"))))
        .transpose()?
        .unwrap_or_default();

    if let Some(token) = fresh_token {
        map.insert(CREDENTIAL_KEY.into(), token.to_string());
        map.remove("GITHUB_TOKEN");
    }

    let credential_keys: Vec<String> = map.keys().cloned().collect();
    let credentials_sealed = if map.is_empty() {
        None
    } else {
        Some(seal_string_map(&map).map_err(|e| Error::Config(format!("seal credentials: {e}")))?)
    };

    let config = existing
        .as_ref()
        .map(|e| e.config.clone())
        .unwrap_or_default();
    let refresh = existing.as_ref().and_then(|e| e.refresh.clone());

    board.upsert_openshell_provider(
        OpenShellProviderDesired {
            name: PROVIDER_NAME.into(),
            provider_type: PROVIDER_TYPE.into(),
            config,
            credentials_sealed,
            credential_keys,
            refresh,
        }
        .normalized(),
    );
    Ok(())
}

/// Whether minting is possible (App material + installation id on the provider).
pub fn configured_for_tokens(board: &SharedBoard) -> bool {
    status_from_board(board).complete && board.github_app_installation_id().is_some()
}

/// Whether a desired provider can supply a host GitHub REST token.
pub fn provider_can_host_poll(p: &OpenShellProviderDesired) -> bool {
    if p.provider_type == PROVIDER_TYPE || p.name == PROVIDER_NAME {
        return true;
    }
    if p.provider_type == LEGACY_BUILTIN_TYPE {
        return true;
    }
    p.credential_keys
        .iter()
        .any(|k| k == CREDENTIAL_KEY || k == "GITHUB_TOKEN")
}

/// Host GitHub REST token for Forge poll from an **explicit** provider name.
///
/// No auto-selection — `provider_name` must be set under Forge. Returns
/// `Ok(None)` when unset, missing, or not yet credentialed.
///
/// - `github-app`: mint/reuse installation token (App JWT path).
/// - other rows: read sealed `GH_TOKEN` (or legacy `GITHUB_TOKEN`).
pub async fn host_poll_token(
    board: &SharedBoard,
    provider_name: Option<&str>,
) -> Result<Option<(String, String)>, Error> {
    let Some(name) = provider_name.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    let Some(p) = board
        .openshell_providers()
        .into_iter()
        .find(|p| p.name == name)
    else {
        return Ok(None);
    };
    if !provider_can_host_poll(&p) {
        return Ok(None);
    }

    if p.provider_type == PROVIDER_TYPE || p.name == PROVIDER_NAME {
        return match host_installation_token(board).await? {
            Some(t) => Ok(Some((name.to_string(), t))),
            None => Ok(None),
        };
    }

    let Some(sealed) = p.credentials_sealed.as_deref() else {
        return Ok(None);
    };
    let map = open_string_map(sealed).map_err(|e| Error::Config(format!("open poll token: {e}")))?;
    if let Some(t) = map
        .get(CREDENTIAL_KEY)
        .or_else(|| map.get("GITHUB_TOKEN"))
        .filter(|t| !t.is_empty())
    {
        return Ok(Some((name.to_string(), t.clone())));
    }
    Ok(None)
}

/// Host-side installation token for REST (webhook poll). Reuses the sealed
/// cache when fresh; mints without requiring an OpenShell gateway push.
///
/// Returns `Ok(None)` when App/installation are not configured.
pub async fn host_installation_token(board: &SharedBoard) -> Result<Option<String>, Error> {
    if !configured_for_tokens(board) {
        return Ok(None);
    }
    let Some(bundle) = board.github_app_bundle() else {
        return Ok(None);
    };
    if bundle.app_id.trim().is_empty() || bundle.private_key_pem.trim().is_empty() {
        return Ok(None);
    }
    let Some(installation_id) = board.github_app_installation_id() else {
        return Ok(None);
    };

    let cache = board.github_app_token_cache();
    let now = Utc::now();
    if !cache.needs_mint(now) {
        if let Some(token) = sealed_github_token(board)? {
            return Ok(Some(token));
        }
    }

    let minted = match mint_installation_token(&bundle, installation_id).await {
        Ok(t) => t,
        Err(e) => {
            board.set_github_app_token_cache(TokenCache {
                expires_at: cache.expires_at,
                last_error: Some(e.to_string()),
            });
            return Err(e);
        }
    };
    // Seal for reuse by poll + provider sync; do not push OpenShell here.
    ensure_desired_row(board, Some(&minted.token))?;
    board.set_github_app_token_cache(TokenCache {
        expires_at: Some(minted.expires_at),
        last_error: None,
    });
    Ok(Some(minted.token))
}

/// GitHub API base (override with `SANDBOARD_GITHUB_API` in tests).
pub fn github_api_base() -> String {
    api_base()
}

/// GitHub PR `mergeable` as returned by the pulls API (`true` / `false` / `null`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrMergeableState {
    Mergeable,
    Conflicting,
    /// `null` / missing — GitHub has not finished computing; retry later.
    Unknown,
}

/// Result of a host-side PR conflict check (no sandbox).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrConflictCheck {
    pub mergeable: PrMergeableState,
    /// Base branch name (`main`, etc.).
    pub base_ref: Option<String>,
}

/// `GET /repos/{owner}/{repo}/pulls/{n}` using the App installation token.
///
/// Returns `Ok(None)` when App/installation are not configured. Used by Review
/// catch-up after main advances — observe `mergeable` first; MERGEABLE is a
/// no-op, CONFLICTING bounces, UNKNOWN retries. No sandbox rebase.
pub async fn fetch_pr_conflict_check(
    board: &SharedBoard,
    pr_url: &str,
) -> Result<Option<PrConflictCheck>, Error> {
    let Some(token) = host_installation_token(board).await? else {
        return Ok(None);
    };
    let Some((owner_repo, number)) = crate::store::parse_github_pr_url(pr_url) else {
        return Err(Error::Config(format!(
            "not a github.com pull URL: {pr_url}"
        )));
    };
    fetch_pr_conflict_check_with_token(&token, &owner_repo, number).await
}

pub(crate) async fn fetch_pr_conflict_check_with_token(
    token: &str,
    owner_repo: &str,
    number: u64,
) -> Result<Option<PrConflictCheck>, Error> {
    #[derive(Deserialize)]
    struct Base {
        #[serde(rename = "ref")]
        base_ref: Option<String>,
    }
    #[derive(Deserialize)]
    struct Resp {
        /// `true` / `false` / omitted or null while GitHub computes.
        mergeable: Option<bool>,
        base: Option<Base>,
    }
    let url = format!(
        "{}/repos/{owner_repo}/pulls/{number}",
        api_base()
    );
    let resp = client()?
        .get(&url)
        .header(reqwest::header::AUTHORIZATION, format!("Bearer {token}"))
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| Error::Api(format!("GET pull: {e}")))?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(Error::Api(format!(
            "GET pull HTTP {status}: {}",
            body.chars().take(200).collect::<String>()
        )));
    }
    let body: Resp = resp
        .json()
        .await
        .map_err(|e| Error::Api(format!("GET pull json: {e}")))?;
    let mergeable = match body.mergeable {
        Some(true) => PrMergeableState::Mergeable,
        Some(false) => PrMergeableState::Conflicting,
        None => PrMergeableState::Unknown,
    };
    Ok(Some(PrConflictCheck {
        mergeable,
        base_ref: body.base.and_then(|b| b.base_ref),
    }))
}

/// Gateway provider name for a cache-resolved installation token.
pub fn routed_provider_name(installation_id: u64) -> String {
    format!("{ROUTED_PROVIDER_PREFIX}{installation_id}")
}

/// Cache lookup for push routing. Does **not** read [`CONFIG_INSTALLATION_ID`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepoInstallRoute {
    Covered {
        owner_repo: String,
        installation_id: u64,
    },
    Uncovered {
        owner_repo: String,
    },
}

pub fn route_repo_installation(cache: &GitHubRepoAccessCache, owner_repo: &str) -> RepoInstallRoute {
    let owner_repo = owner_repo.trim().to_string();
    match cache.installation_id_for(&owner_repo) {
        Some(installation_id) => RepoInstallRoute::Covered {
            owner_repo,
            installation_id,
        },
        None => RepoInstallRoute::Uncovered { owner_repo },
    }
}

/// Outcome of minting a token for a cached (or uncovered) `owner/repo`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepoTokenOutcome {
    Uncovered {
        owner_repo: String,
    },
    Ready {
        owner_repo: String,
        installation_id: u64,
        provider_name: String,
        /// Distinct from the singleton [`PROVIDER_NAME`] / `GITHUB_INSTALLATION_ID`.
        routed: bool,
    },
}

/// Replace singleton `github-app` on a create-spec with the routed provider.
pub fn overlay_routed_provider(providers: &mut Vec<String>, outcome: &RepoTokenOutcome) {
    let RepoTokenOutcome::Ready {
        provider_name,
        routed: true,
        ..
    } = outcome
    else {
        return;
    };
    providers.retain(|n| n != PROVIDER_NAME);
    if !providers.iter().any(|n| n == provider_name) {
        providers.push(provider_name.clone());
    }
}

/// Clone/push `owner/name` already known on the card (PR list, then intent/DoD/notes).
pub fn owner_repo_from_card(item: &WorkItem) -> Option<String> {
    if let Some(pr) = item
        .unmerged_prs()
        .next()
        .or_else(|| item.pull_requests.first())
    {
        if let Some(r) = pr.push_owner_repo() {
            return Some(r);
        }
    }
    if let Some(r) = crate::schema::clone_repo_from_prose(&item.intent) {
        return Some(r);
    }
    if let Some(dod) = item.definition_of_done.as_deref() {
        if let Some(r) = crate::schema::clone_repo_from_prose(dod) {
            return Some(r);
        }
    }
    for n in item.notes.iter().rev() {
        if let Some(r) = crate::schema::clone_repo_from_prose(&n.text) {
            return Some(r);
        }
        if let Some(r) = owner_repo_from_decision_note(&n.text) {
            return Some(r);
        }
    }
    None
}

fn owner_repo_from_decision_note(text: &str) -> Option<String> {
    let raw = text.trim();
    let after_decision = raw
        .strip_prefix("Decision:")
        .or_else(|| raw.strip_prefix("decision:"))
        .map(str::trim)
        .unwrap_or(raw);
    let after_clone = after_decision
        .strip_prefix("Clone ")
        .or_else(|| after_decision.strip_prefix("clone "))
        .or_else(|| {
            after_decision
                .strip_prefix("Clone target:")
                .or_else(|| after_decision.strip_prefix("clone target:"))
        })
        .map(str::trim)?;
    let token = after_clone.split_whitespace().next()?;
    let token = token.trim_matches(|c: char| matches!(c, ')' | '(' | ',' | ';' | '.'));
    crate::schema::parse_owner_name(token).ok()
}

pub fn uncovered_escalation(owner_repo: &str) -> (String, Vec<EscalationOption>, usize) {
    let question = format!(
        "Agent wants to push to {owner_repo} but the GitHub App is not installed there. \
Open Settings → Repo access ({SETTINGS_REPO_ACCESS_PATH}), install the App / add this \
repo, Refresh the cache, then Unpark."
    );
    let options = vec![
        EscalationOption {
            label: "Install the App and refresh".into(),
            detail: format!(
                "Open {SETTINGS_REPO_ACCESS_PATH}. Use the GitHub install page \
({INSTALLATIONS_MANAGE_URL}) to add {owner_repo}, Refresh repo access, then Unpark."
            ),
        },
        EscalationOption {
            label: "Push a covered repo instead".into(),
            detail: "Change the clone/PR target to an owner/name already listed under \
Settings → Repo access, then Unpark."
                .into(),
        },
    ];
    (question, options, 0)
}

pub fn escalate_uncovered_repo(
    board: &Board,
    id: ItemId,
    agent_id: &str,
    owner_repo: &str,
) -> Result<WorkItem, String> {
    let (question, options, recommended) = uncovered_escalation(owner_repo);
    board.escalate(id, agent_id, question, options, recommended)
}

async fn route_with_optional_refresh(
    board: &SharedBoard,
    owner_repo: &str,
) -> RepoInstallRoute {
    let first = route_repo_installation(&board.github_repo_access_cache(), owner_repo);
    if matches!(first, RepoInstallRoute::Covered { .. }) {
        return first;
    }
    if refresh_repo_access_cache(board).await.is_ok() {
        return route_repo_installation(&board.github_repo_access_cache(), owner_repo);
    }
    first
}

/// Mint a token for `installation_id` onto a dedicated gateway provider.
/// Does not change [`CONFIG_INSTALLATION_ID`] or the singleton GH_TOKEN cache.
pub async fn apply_routed_installation_token(
    board: &SharedBoard,
    installation_id: u64,
) -> Result<String, Error> {
    let Some(bundle) = board.github_app_bundle() else {
        return Err(Error::Config(
            "GitHub App credentials missing; cannot mint a routed GH_TOKEN".into(),
        ));
    };
    if bundle.app_id.trim().is_empty() || bundle.private_key_pem.trim().is_empty() {
        return Err(Error::Config(
            "GitHub App credentials incomplete; cannot mint a routed GH_TOKEN".into(),
        ));
    }
    let minted = mint_installation_token(&bundle, installation_id).await?;
    let name = routed_provider_name(installation_id);
    let os = board.openshell_client();
    os.apply_provider(
        &name,
        PROVIDER_TYPE,
        provider_credentials(&minted.token),
        BTreeMap::new(),
        None,
    )
    .await
    .map_err(|e| Error::Api(format!("openshell apply {name}: {e}")))?;
    tracing::info!(
        installation_id,
        provider = %name,
        "minted GitHub App installation token for repo-access routing"
    );
    Ok(name)
}

pub async fn attach_routed_provider_to_sandbox(
    board: &SharedBoard,
    sandbox: &str,
    provider_name: &str,
) -> Result<(), Error> {
    let sandbox = sandbox.trim();
    if sandbox.is_empty() {
        return Ok(());
    }
    let os = board.openshell_client();
    os.attach_sandbox_provider(sandbox, provider_name)
        .await
        .map_err(|e| Error::Api(format!("openshell attach {provider_name} to {sandbox}: {e}")))?;
    if provider_name != PROVIDER_NAME {
        if let Err(e) = os.detach_sandbox_provider(sandbox, PROVIDER_NAME).await {
            tracing::debug!(
                sandbox,
                "detach singleton `{PROVIDER_NAME}` after routed attach: {e}"
            );
        }
    }
    Ok(())
}

/// Look up the access cache (not `GITHUB_INSTALLATION_ID`), mint, and attach
/// to a live sandbox when `sandbox` is set.
pub async fn sync_sandbox_token_for_repo(
    board: &SharedBoard,
    sandbox: Option<&str>,
    owner_repo: &str,
) -> Result<RepoTokenOutcome, Error> {
    let owner_repo = owner_repo.trim();
    if owner_repo.is_empty() {
        return Err(Error::Config("empty owner/repo for installation routing".into()));
    }
    let route = route_with_optional_refresh(board, owner_repo).await;
    match route {
        RepoInstallRoute::Uncovered { owner_repo } => {
            Ok(RepoTokenOutcome::Uncovered { owner_repo })
        }
        RepoInstallRoute::Covered {
            owner_repo,
            installation_id,
        } => {
            let singleton = board.github_app_installation_id();
            let (provider_name, routed) = if singleton == Some(installation_id) {
                (PROVIDER_NAME.to_string(), false)
            } else {
                let name = apply_routed_installation_token(board, installation_id).await?;
                (name, true)
            };
            if routed {
                if let Some(box_name) = sandbox.map(str::trim).filter(|s| !s.is_empty()) {
                    attach_routed_provider_to_sandbox(board, box_name, &provider_name).await?;
                }
            }
            Ok(RepoTokenOutcome::Ready {
                owner_repo,
                installation_id,
                provider_name,
                routed,
            })
        }
    }
}

/// Result of ensuring a card/sandbox can push to a known repo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnsurePushToken {
    /// No `owner/name` on the card yet — agent may still escalate for remotes.
    Skipped,
    /// App does not cover the repo; card is in Needs You.
    Parked,
    Ready(RepoTokenOutcome),
}

/// Resolve clone/PR repo, mint/attach, or park Needs You on cache miss.
pub async fn ensure_push_token(
    board: &SharedBoard,
    item_id: ItemId,
    agent_id: &str,
    sandbox: Option<&str>,
    owner_repo: Option<&str>,
) -> Result<EnsurePushToken, Error> {
    let repo = match owner_repo
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
    {
        Some(r) => r,
        None => match board.get(item_id).as_ref().and_then(owner_repo_from_card) {
            Some(r) => r,
            None => return Ok(EnsurePushToken::Skipped),
        },
    };
    let outcome = sync_sandbox_token_for_repo(board, sandbox, &repo).await?;
    if let RepoTokenOutcome::Uncovered { owner_repo } = &outcome {
        escalate_uncovered_repo(board, item_id, agent_id, owner_repo)
            .map_err(Error::Config)?;
        return Ok(EnsurePushToken::Parked);
    }
    Ok(EnsurePushToken::Ready(outcome))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::openshell::{OpenShell, Output};
    use crate::secrets::open_string_map;
    use crate::store::Board;
    use axum::routing::{get, post};
    use axum::{Json, Router};
    use std::sync::{Mutex, MutexGuard};
    use std::time::Duration as StdDuration;

    /// Minimal RSA key for JWT unit tests (not a real GitHub App key).
    fn test_rsa_pem() -> String {
        // Generated once for tests; never used against GitHub.
        include_str!("testdata/github_app_test_rsa.pem").to_string()
    }

    /// Serialize `SANDBOARD_GITHUB_API` mutations across tests.
    mod github_api_env {
        use super::*;
        static LOCK: Mutex<()> = Mutex::new(());

        pub(crate) struct Guard {
            _lock: MutexGuard<'static, ()>,
            prev: Option<String>,
        }

        impl Guard {
            pub(crate) fn set(base: &str) -> Self {
                let _lock = LOCK.lock().unwrap_or_else(|p| p.into_inner());
                let prev = std::env::var("SANDBOARD_GITHUB_API").ok();
                std::env::set_var("SANDBOARD_GITHUB_API", base);
                Self { _lock, prev }
            }
        }

        impl Drop for Guard {
            fn drop(&mut self) {
                match &self.prev {
                    Some(v) => std::env::set_var("SANDBOARD_GITHUB_API", v),
                    None => std::env::remove_var("SANDBOARD_GITHUB_API"),
                }
            }
        }
    }

    fn test_board(label: &str) -> (std::path::PathBuf, SharedBoard, crate::secrets::master_key_env::Guard) {
        let dir = std::env::temp_dir().join(format!(
            "sandboard-test-ghapp-{label}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::create_dir_all(&dir);
        let key_path = dir.join("master.key");
        let env = crate::secrets::master_key_env::Guard::with_key_path(&key_path);
        let mut board_inner = Board::new(crate::schema::Schema::default(), dir.join("board.json"));
        // ensure_github_provider lists then create/updates the gateway provider.
        board_inner.openshell = Some(OpenShell::mock(
            |argv| {
                if argv.first().map(String::as_str) == Some("provider") {
                    Output {
                        code: 0,
                        stdout: "[]".into(),
                        stderr: String::new(),
                    }
                } else {
                    Output {
                        code: 1,
                        stdout: String::new(),
                        stderr: format!("unexpected mock argv: {argv:?}"),
                    }
                }
            },
            StdDuration::from_secs(5),
        ));
        let board = std::sync::Arc::new(board_inner);
        (dir, board, env)
    }

    fn seal_test_app(board: &SharedBoard) {
        board
            .set_github_app_bundle(&GitHubAppBundle {
                app_id: "123456".into(),
                private_key_pem: test_rsa_pem(),
                ..Default::default()
            })
            .expect("seal onto provider");
    }

    async fn spawn_github_mock() -> (String, tokio::task::JoinHandle<()>) {
        let app = Router::new()
            .route(
                "/app/installations",
                get(|| async {
                    Json(serde_json::json!([{
                        "id": 99,
                        "account": { "login": "clankrshq", "type": "Organization" }
                    }]))
                }),
            )
            .route(
                "/app/installations/{id}/access_tokens",
                post(|| async {
                    let expires = (Utc::now() + Duration::hours(1)).to_rfc3339();
                    Json(serde_json::json!({
                        "token": "ghs_mock_installation_token",
                        "expires_at": expires,
                    }))
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock");
        let addr = listener.local_addr().expect("addr");
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve mock");
        });
        (format!("http://{addr}"), handle)
    }

    #[test]
    fn token_cache_needs_mint_when_empty_or_near_expiry() {
        let now = Utc::now();
        assert!(TokenCache::default().needs_mint(now));
        let fresh = TokenCache {
            expires_at: Some(now + Duration::hours(1)),
            last_error: None,
        };
        assert!(!fresh.needs_mint(now));
        let soon = TokenCache {
            expires_at: Some(now + Duration::minutes(5)),
            last_error: None,
        };
        assert!(soon.needs_mint(now));
    }

    #[test]
    fn make_app_jwt_round_trips_header() {
        let pem = test_rsa_pem();
        if pem.trim().is_empty() || !pem.contains("BEGIN") {
            // File missing in sparse checkouts — skip rather than fail CI shape.
            eprintln!("skip jwt test: no testdata pem");
            return;
        }
        let bundle = GitHubAppBundle {
            app_id: "123456".into(),
            private_key_pem: pem,
            ..Default::default()
        };
        let jwt = make_app_jwt(&bundle, Utc::now()).expect("jwt");
        let parts: Vec<_> = jwt.split('.').collect();
        assert_eq!(parts.len(), 3);
        assert!(!jwt.contains("BEGIN"));
    }

    #[test]
    fn provider_credentials_sets_gh_token_only() {
        let m = provider_credentials("ghs_test");
        assert_eq!(m.get(CREDENTIAL_KEY).map(String::as_str), Some("ghs_test"));
        assert!(!m.contains_key("GITHUB_TOKEN"));
        assert_eq!(m.len(), 1);
    }

    #[tokio::test]
    async fn host_poll_token_requires_explicit_provider_name() {
        let (dir, board, _env) = test_board("poll-explicit");
        seal_test_app(&board);
        board.set_github_app_installation_id(Some(99));
        // Even with App ready, no auto-pick without Forge provider_name.
        assert!(host_poll_token(&board, None).await.expect("ok").is_none());
        assert!(host_poll_token(&board, Some("nope"))
            .await
            .expect("ok")
            .is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn gateway_credentials_strips_app_private_material() {
        let mut board = BTreeMap::new();
        board.insert(CREDENTIAL_KEY.into(), "ghs_live".into());
        board.insert(CRED_PRIVATE_KEY.into(), "-----BEGIN RSA PRIVATE KEY-----\nX\n-----END RSA PRIVATE KEY-----\n".into());
        board.insert(CRED_WEBHOOK_SECRET.into(), "whsec".into());
        let gw = gateway_credentials(&board);
        assert_eq!(gw.get(CREDENTIAL_KEY).map(String::as_str), Some("ghs_live"));
        assert!(!gw.contains_key(CRED_PRIVATE_KEY));
        assert!(!gw.contains_key(CRED_WEBHOOK_SECRET));
        let mut cfg = BTreeMap::new();
        cfg.insert(CONFIG_APP_ID.into(), "1".into());
        cfg.insert("OTHER".into(), "x".into());
        let gcfg = gateway_config(&cfg);
        assert!(!gcfg.contains_key(CONFIG_APP_ID));
        assert_eq!(gcfg.get("OTHER").map(String::as_str), Some("x"));
    }

    #[test]
    fn ensure_desired_row_seals_token_without_plaintext_on_board() {
        let (dir, board, _env) = test_board("desired");
        ensure_desired_row(&board, Some("ghs_secret_value")).expect("upsert");
        let providers = board.openshell_providers();
        assert_eq!(providers.len(), 1);
        let p = &providers[0];
        assert_eq!(p.name, PROVIDER_NAME);
        assert_eq!(p.provider_type, PROVIDER_TYPE);
        assert!(p.credential_keys.iter().any(|k| k == CREDENTIAL_KEY));
        let sealed = p.credentials_sealed.as_deref().expect("sealed");
        assert!(!sealed.contains("ghs_secret_value"));
        let opened = open_string_map(sealed).expect("open");
        assert_eq!(
            opened.get(CREDENTIAL_KEY).map(String::as_str),
            Some("ghs_secret_value")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn ensure_skips_mint_when_token_cache_still_fresh() {
        let (dir, board, _env) = test_board("fresh");
        seal_test_app(&board);
        board.set_github_app_installation_id(Some(99));
        ensure_desired_row(&board, Some("ghs_cached_only")).expect("seed sealed");
        board.set_github_app_token_cache(TokenCache {
            expires_at: Some(Utc::now() + Duration::hours(1)),
            last_error: None,
        });
        // Point at a dead base — mint must not be attempted (sealed token reused).
        let _api = github_api_env::Guard::set("http://127.0.0.1:1");
        let minted = ensure_github_provider(&board).await.expect("ensure");
        assert!(minted);
        let p = board
            .openshell_providers()
            .into_iter()
            .find(|p| p.name == PROVIDER_NAME)
            .expect("desired github row");
        assert_eq!(p.name, PROVIDER_NAME);
        let opened = open_string_map(p.credentials_sealed.as_deref().unwrap()).expect("open");
        assert_eq!(
            opened.get(CREDENTIAL_KEY).map(String::as_str),
            Some("ghs_cached_only")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn ensure_mints_and_upserts_via_mock_github_and_openshell() {
        let dir = std::env::temp_dir().join(format!(
            "sandboard-test-ghapp-mint-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::create_dir_all(&dir);
        let key_path = dir.join("master.key");
        let _env = crate::secrets::master_key_env::Guard::with_key_path(&key_path);
        let mut board_inner = Board::new(crate::schema::Schema::default(), dir.join("board.json"));
        board_inner.openshell = Some(OpenShell::mock(
            |argv| {
                if argv.first().map(String::as_str) == Some("provider") {
                    Output {
                        code: 0,
                        stdout: "[]".into(),
                        stderr: String::new(),
                    }
                } else {
                    Output {
                        code: 1,
                        stdout: String::new(),
                        stderr: format!("unexpected mock argv: {argv:?}"),
                    }
                }
            },
            StdDuration::from_secs(5),
        ));
        let board: SharedBoard = std::sync::Arc::new(board_inner);
        seal_test_app(&board);
        board.set_github_app_installation_id(Some(99));

        let (base, handle) = spawn_github_mock().await;
        let _api = github_api_env::Guard::set(&base);

        let ok = ensure_github_provider(&board).await.expect("ensure");
        assert!(ok);
        let cache = board.github_app_token_cache();
        assert!(cache.expires_at.is_some());
        assert!(cache.last_error.is_none());
        let p = board
            .openshell_providers()
            .into_iter()
            .find(|p| p.name == PROVIDER_NAME)
            .expect("provider");
        assert_eq!(p.name, PROVIDER_NAME);
        let sealed = p.credentials_sealed.as_deref().expect("sealed");
        assert!(!sealed.contains("ghs_mock_installation_token"));
        let opened = open_string_map(sealed).expect("open");
        assert_eq!(
            opened.get(CREDENTIAL_KEY).map(String::as_str),
            Some("ghs_mock_installation_token")
        );

        // Second call with fresh cache must not remint (dead API would fail).
        drop(_api);
        let _dead = github_api_env::Guard::set("http://127.0.0.1:1");
        assert!(ensure_github_provider(&board).await.expect("fresh ensure"));

        handle.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn list_installations_parses_accounts() {
        let (base, handle) = spawn_github_mock().await;
        let _api = github_api_env::Guard::set(&base);
        let bundle = GitHubAppBundle {
            app_id: "123456".into(),
            private_key_pem: test_rsa_pem(),
            ..Default::default()
        };
        let jwt = make_app_jwt(&bundle, Utc::now()).expect("jwt");
        let list = list_installations(&jwt).await.expect("list");
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, 99);
        assert_eq!(list[0].account_login, "clankrshq");
        handle.abort();
    }

    fn perms(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn apply_installation_repos_maps_owner_repo_to_installation() {
        let now = Utc::now();
        let mut cache = GitHubRepoAccessCache::default();
        let inst = InstallationInfo {
            id: 42,
            account_login: "acme".into(),
            account_type: "Organization".into(),
        };
        apply_installation_repos(
            &mut cache,
            &inst,
            &[
                InstallationRepo {
                    full_name: "acme/widgets".into(),
                    permissions: perms(&[("push", "true"), ("pull", "true")]),
                },
                InstallationRepo {
                    full_name: "acme/core".into(),
                    permissions: perms(&[("admin", "true")]),
                },
                InstallationRepo {
                    full_name: "not-a-repo".into(),
                    permissions: BTreeMap::new(),
                },
            ],
            now,
        );
        assert_eq!(cache.installations.len(), 1);
        assert_eq!(cache.repos.len(), 2);
        let widgets = cache.repos.get("acme/widgets").expect("widgets");
        assert_eq!(widgets.installation_id, 42);
        assert_eq!(widgets.permissions.get("push").map(String::as_str), Some("true"));
        assert_eq!(widgets.last_seen_at, now);
        assert_eq!(cache.installation_id_for("acme/core"), Some(42));
        assert_eq!(cache.installation_id_for("ACME/widgets"), Some(42));
        assert_eq!(cache.installation_id_for("missing/repo"), None);
        assert_eq!(
            installation_manage_url("acme", "Organization", 42),
            "https://github.com/organizations/acme/settings/installations/42"
        );
        assert_eq!(
            installation_manage_url("alice", "User", 7),
            "https://github.com/settings/installations/7"
        );
    }

    async fn spawn_repo_access_mock() -> (String, tokio::task::JoinHandle<()>) {
        use axum::http::HeaderMap;
        let app = Router::new()
            .route(
                "/app/installations",
                get(|| async {
                    Json(serde_json::json!([
                        {
                            "id": 99,
                            "account": { "login": "clankrshq", "type": "Organization" }
                        },
                        {
                            "id": 100,
                            "account": { "login": "shanemcd", "type": "User" }
                        }
                    ]))
                }),
            )
            .route(
                "/app/installations/{id}/access_tokens",
                post(
                    |axum::extract::Path(id): axum::extract::Path<u64>| async move {
                        let expires = (Utc::now() + Duration::hours(1)).to_rfc3339();
                        Json(serde_json::json!({
                            "token": format!("ghs_inst_{id}"),
                            "expires_at": expires,
                        }))
                    },
                ),
            )
            .route(
                "/installation/repositories",
                get(|headers: HeaderMap| async move {
                    let auth = headers
                        .get(axum::http::header::AUTHORIZATION)
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("");
                    let repositories = if auth.contains("ghs_inst_99") {
                        serde_json::json!([{
                            "full_name": "clankrshq/sandboard",
                            "permissions": { "admin": true, "push": true, "pull": true }
                        }])
                    } else if auth.contains("ghs_inst_100") {
                        serde_json::json!([{
                            "full_name": "shanemcd/notes",
                            "permissions": { "admin": false, "push": true, "pull": true }
                        }])
                    } else {
                        serde_json::json!([])
                    };
                    Json(serde_json::json!({ "repositories": repositories }))
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock");
        let addr = listener.local_addr().expect("addr");
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve mock");
        });
        (format!("http://{addr}"), handle)
    }

    #[tokio::test]
    async fn refresh_repo_access_cache_maps_installations_without_sandbox_token() {
        let (dir, board, _env) = test_board("repo-access");
        seal_test_app(&board);
        board.set_github_app_installation_id(Some(99));

        let (base, handle) = spawn_repo_access_mock().await;
        let _api = github_api_env::Guard::set(&base);

        let cache = refresh_repo_access_cache(&board).await.expect("refresh");
        assert_eq!(cache.installations.len(), 2);
        assert_eq!(cache.repos.len(), 2);
        assert_eq!(cache.installation_id_for("clankrshq/sandboard"), Some(99));
        assert_eq!(cache.installation_id_for("shanemcd/notes"), Some(100));
        let sandboard = cache.repos.get("clankrshq/sandboard").expect("sandboard");
        assert_eq!(sandboard.permissions.get("admin").map(String::as_str), Some("true"));
        assert!(cache.last_error.is_none());
        assert!(cache.refreshed_at.is_some());

        // Visibility walk must not mint the sandbox GH_TOKEN provider credential.
        let providers = board.openshell_providers();
        let github = providers.iter().find(|p| p.name == PROVIDER_NAME);
        if let Some(p) = github {
            let opened = p
                .credentials_sealed
                .as_deref()
                .and_then(|s| open_string_map(s).ok())
                .unwrap_or_default();
            assert!(
                !opened.contains_key(CREDENTIAL_KEY),
                "repo-access refresh must not write GH_TOKEN"
            );
        }

        // Singleton minting still uses GITHUB_INSTALLATION_ID (99), not routing.
        let ok = ensure_github_provider(&board).await.expect("ensure");
        assert!(ok);
        let p = board
            .openshell_providers()
            .into_iter()
            .find(|p| p.name == PROVIDER_NAME)
            .expect("provider");
        let opened = open_string_map(p.credentials_sealed.as_deref().unwrap()).expect("open");
        assert_eq!(
            opened.get(CREDENTIAL_KEY).map(String::as_str),
            Some("ghs_inst_99")
        );
        assert_eq!(board.github_app_installation_id(), Some(99));

        handle.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn fetch_pr_conflict_check_maps_mergeable_states() {
        let app = Router::new()
            .route(
                "/repos/{owner}/{repo}/pulls/{number}",
                get(|axum::extract::Path((_, _, number)): axum::extract::Path<(
                    String,
                    String,
                    u64,
                )>| async move {
                    if number == 404 {
                        return (
                            axum::http::StatusCode::NOT_FOUND,
                            Json(serde_json::json!({ "message": "Not Found" })),
                        );
                    }
                    let body = match number {
                        1 => serde_json::json!({
                            "mergeable": true,
                            "base": { "ref": "main" }
                        }),
                        2 => serde_json::json!({
                            "mergeable": false,
                            "base": { "ref": "main" }
                        }),
                        3 => serde_json::json!({
                            "mergeable": null,
                            "base": { "ref": "main" }
                        }),
                        _ => serde_json::json!({ "message": "Not Found" }),
                    };
                    (axum::http::StatusCode::OK, Json(body))
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve");
        });
        let _api = github_api_env::Guard::set(&format!("http://{addr}"));

        let m = fetch_pr_conflict_check_with_token("tok", "o/r", 1)
            .await
            .expect("ok")
            .expect("some");
        assert_eq!(m.mergeable, PrMergeableState::Mergeable);
        assert_eq!(m.base_ref.as_deref(), Some("main"));

        let c = fetch_pr_conflict_check_with_token("tok", "o/r", 2)
            .await
            .expect("ok")
            .expect("some");
        assert_eq!(c.mergeable, PrMergeableState::Conflicting);

        let u = fetch_pr_conflict_check_with_token("tok", "o/r", 3)
            .await
            .expect("ok")
            .expect("some");
        assert_eq!(u.mergeable, PrMergeableState::Unknown);

        let missing = fetch_pr_conflict_check_with_token("tok", "o/r", 404)
            .await
            .expect("ok");
        assert!(missing.is_none());

        handle.abort();
    }

    fn cache_repo(full_name: &str, installation_id: u64) -> GitHubRepoAccessCache {
        let mut repos = BTreeMap::new();
        repos.insert(
            full_name.to_string(),
            GitHubRepoAccessEntry {
                installation_id,
                permissions: BTreeMap::new(),
                last_seen_at: Utc::now(),
            },
        );
        GitHubRepoAccessCache {
            installations: vec![InstallationInfo {
                id: installation_id,
                account_login: full_name.split('/').next().unwrap_or("org").into(),
                account_type: "Organization".into(),
            }],
            repos,
            ..Default::default()
        }
    }

    fn running_card(board: &SharedBoard, intent: &str) -> crate::model::WorkItem {
        use crate::model::{Origin, State};
        let p = board
            .create_project("Route proj", "p", "sandboard-app/sandboard", true, None)
            .expect("project");
        let t = board
            .create(
                Some(p.id),
                "Route task",
                intent,
                Some("DoD".into()),
                Origin::Human,
                false,
                None,
            )
            .expect("task");
        let _ = board.transition(t.id, State::Shaping, "human", None);
        let _ = board.transition(t.id, State::Backlog, "human", None);
        let _ = board.transition(t.id, State::Claimed, "agent-1", None);
        let _ = board.transition(t.id, State::Running, "agent-1", None);
        board.get(t.id).expect("item")
    }

    #[test]
    fn route_repo_installation_ignores_board_singleton() {
        let cache = cache_repo("acme/other", 100);
        assert_eq!(
            route_repo_installation(&cache, "acme/other"),
            RepoInstallRoute::Covered {
                owner_repo: "acme/other".into(),
                installation_id: 100,
            }
        );
        assert_eq!(
            route_repo_installation(&cache, "missing/repo"),
            RepoInstallRoute::Uncovered {
                owner_repo: "missing/repo".into(),
            }
        );
    }

    #[test]
    fn uncovered_escalation_points_at_settings_repo_access() {
        let (q, opts, rec) = uncovered_escalation("acme/secret");
        assert!(q.contains("acme/secret"), "{q}");
        assert!(q.contains(SETTINGS_REPO_ACCESS_PATH), "{q}");
        assert_eq!(opts.len(), 2);
        assert_eq!(rec, 0);
        assert!(opts[0].detail.contains(INSTALLATIONS_MANAGE_URL));
        assert!(opts[0].detail.contains("Refresh"));
        assert!(opts[0].detail.contains("Unpark"));
    }

    #[test]
    fn overlay_routed_provider_replaces_singleton() {
        let mut providers = vec!["vertex".into(), PROVIDER_NAME.into()];
        overlay_routed_provider(
            &mut providers,
            &RepoTokenOutcome::Ready {
                owner_repo: "acme/other".into(),
                installation_id: 100,
                provider_name: routed_provider_name(100),
                routed: true,
            },
        );
        assert_eq!(
            providers,
            vec!["vertex".to_string(), "github-app-install-100".to_string()]
        );
        let mut keep = vec![PROVIDER_NAME.into()];
        overlay_routed_provider(
            &mut keep,
            &RepoTokenOutcome::Ready {
                owner_repo: "acme/core".into(),
                installation_id: 99,
                provider_name: PROVIDER_NAME.into(),
                routed: false,
            },
        );
        assert_eq!(keep, vec![PROVIDER_NAME.to_string()]);
    }

    #[test]
    fn owner_repo_from_card_reads_clone_prose() {
        use crate::model::Origin;
        let (dir, board, _env) = test_board("prose-repo");
        let item = board
            .create(
                None,
                "proj",
                "Clone repository: widgets-org/core into /sandbox/repo",
                None,
                Origin::Human,
                true,
                None,
            )
            .expect("create");
        assert_eq!(
            owner_repo_from_card(&item).as_deref(),
            Some("widgets-org/core")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn uncovered_repo_parks_needs_you_without_minting() {
        let (dir, board, _env) = test_board("uncovered");
        seal_test_app(&board);
        board.set_github_app_installation_id(Some(99));
        let item = running_card(
            &board,
            "Clone repository: missing-org/missing into /sandbox/repo",
        );
        board.set_environment(item.id, Some("sandboard-card-uncovered".into()));
        let _api = github_api_env::Guard::set("http://127.0.0.1:1");
        let outcome = ensure_push_token(&board, item.id, "agent-1", Some("sandboard-card-uncovered"), None)
            .await
            .expect("ensure");
        assert_eq!(outcome, EnsurePushToken::Parked);
        let parked = board.get(item.id).expect("card");
        assert_eq!(parked.state, crate::model::State::NeedsHuman);
        let q = parked.escalation.as_ref().expect("esc").question.clone();
        assert!(q.contains("missing-org/missing"), "{q}");
        assert!(q.contains(SETTINGS_REPO_ACCESS_PATH), "{q}");
        assert_eq!(board.github_app_installation_id(), Some(99));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn covered_repo_mints_routed_token_not_singleton_installation() {
        use std::sync::{Arc, Mutex};
        let dir = std::env::temp_dir().join(format!(
            "sandboard-test-ghapp-route-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::create_dir_all(&dir);
        let key_path = dir.join("master.key");
        let _env = crate::secrets::master_key_env::Guard::with_key_path(&key_path);
        let seen: Arc<Mutex<Vec<Vec<String>>>> = Arc::new(Mutex::new(Vec::new()));
        let seen_c = Arc::clone(&seen);
        let mut board_inner = Board::new(crate::schema::Schema::default(), dir.join("board.json"));
        board_inner.openshell = Some(OpenShell::mock(
            move |argv| {
                seen_c.lock().unwrap().push(argv.to_vec());
                if argv.first().map(String::as_str) == Some("provider")
                    && argv.get(1).map(String::as_str) == Some("list")
                {
                    return Output {
                        code: 0,
                        stdout: "[]".into(),
                        stderr: String::new(),
                    };
                }
                Output {
                    code: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                }
            },
            StdDuration::from_secs(5),
        ));
        let board: SharedBoard = std::sync::Arc::new(board_inner);
        seal_test_app(&board);
        board.set_github_app_installation_id(Some(99));
        board.set_github_repo_access_cache(cache_repo("acme/other", 100));

        let (base, handle) = spawn_github_mock().await;
        let _api = github_api_env::Guard::set(&base);

        let item = running_card(
            &board,
            "Clone repository: acme/other into /sandbox/repo",
        );
        board.set_environment(item.id, Some("sandboard-card-route".into()));

        let outcome = ensure_push_token(
            &board,
            item.id,
            "agent-1",
            Some("sandboard-card-route"),
            Some("acme/other"),
        )
        .await
        .expect("ensure");
        match outcome {
            EnsurePushToken::Ready(RepoTokenOutcome::Ready {
                installation_id,
                provider_name,
                routed,
                ..
            }) => {
                assert_eq!(installation_id, 100);
                assert_eq!(provider_name, "github-app-install-100");
                assert!(routed);
            }
            other => panic!("expected routed ready, got {other:?}"),
        }
        assert_eq!(board.github_app_installation_id(), Some(99));
        let calls = seen.lock().unwrap().clone();
        assert!(
            calls.iter().any(|a| {
                a.windows(2).any(|w| {
                    w[0] == "--name" && w[1] == "github-app-install-100"
                })
            }),
            "must create/update routed provider: {calls:?}"
        );
        assert!(
            calls.iter().any(|a| {
                a.first().map(String::as_str) == Some("sandbox")
                    && a.get(1).map(String::as_str) == Some("provider")
                    && a.get(2).map(String::as_str) == Some("attach")
                    && a.iter().any(|s| s == "github-app-install-100")
                    && a.iter().any(|s| s == "sandboard-card-route")
            }),
            "must attach routed GH_TOKEN to live sandbox: {calls:?}"
        );

        handle.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn covered_same_as_singleton_does_not_open_routed_provider() {
        let (dir, board, _env) = test_board("same-install");
        seal_test_app(&board);
        board.set_github_app_installation_id(Some(99));
        board.set_github_repo_access_cache(cache_repo("sandboard-app/sandboard", 99));
        let _api = github_api_env::Guard::set("http://127.0.0.1:1");
        let outcome = sync_sandbox_token_for_repo(&board, Some("box"), "sandboard-app/sandboard")
            .await
            .expect("sync");
        assert_eq!(
            outcome,
            RepoTokenOutcome::Ready {
                owner_repo: "sandboard-app/sandboard".into(),
                installation_id: 99,
                provider_name: PROVIDER_NAME.into(),
                routed: false,
            }
        );
        assert_eq!(board.github_app_installation_id(), Some(99));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
