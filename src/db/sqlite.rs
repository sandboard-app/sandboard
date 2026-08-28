//! SQLite `BoardStore` — row-level board persistence and one-shot JSON import.

use super::codec::{
    item_from_row, item_to_row, parent_first, META_AGENT_RUNTIME, META_AUTH_ALLOWED_TEAMS,
    META_AUTH_ALLOWED_USERS, META_AUTH_SEALED, META_COCKPIT_SANDBOX_PROFILE_ID,
    META_COCKPIT_SESSION, META_DEFAULT_SANDBOX_PROFILE_ID, META_GITHUB_APP_INSTALLATION_ID,
    META_GITHUB_APP_SEALED, META_JSON_IMPORTED, META_NEXT_ID, META_OPENSHELL_AUTH_MODE,
    META_OPENSHELL_GATEWAY_ENDPOINT, META_MCP_SERVERS, META_OPENSHELL_MTLS_SEALED,
    META_OPENSHELL_OIDC_CONFIG, META_OPENSHELL_OIDC_SEALED, META_OPENSHELL_POLICIES,
    META_OPENSHELL_PROVIDERS, META_OPENSHELL_PROVIDER_TYPES,
    META_OPENSHELL_PROVIDER_TYPE_TOMBSTONES, META_SANDBOX_PROFILES,
    META_WEBHOOK_POLL, META_WEBHOOK_POLL_PR_REVIEWS, META_WEBHOOK_POLL_TIPS,
    META_GITHUB_REPO_ACCESS, META_WORKSPACE_BINDING,
};
use super::config::DatabaseBackend;
use super::store::{BoardStore, StoreError};
use super::{connect_sqlite_migrated, parse_database_url};
use crate::model::{
    AgentRuntimeConfig, CockpitSession, ItemId, McpServerDesired, OpenShellAuthMode,
    OpenShellOidcConfig, OpenShellPolicy, OpenShellProviderDesired, SandboxProfile,
    WebhookPollConfig, WorkItem, WorkspaceBinding,
};
use crate::store::{BoardState, StoryLine};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::sqlite::SqlitePool;
use sqlx::{Sqlite, Transaction};
use std::collections::BTreeMap;
use std::path::Path;

pub struct SqliteBoardStore {
    pool: SqlitePool,
}

impl SqliteBoardStore {
    pub async fn connect(url: &str) -> Result<Self, StoreError> {
        let parsed = parse_database_url(url)?;
        if parsed.backend() != DatabaseBackend::Sqlite {
            return Err(StoreError::WrongBackend {
                expected: DatabaseBackend::Sqlite,
                got: parsed.backend(),
            });
        }
        let pool = connect_sqlite_migrated(parsed.as_str()).await?;
        // Schema uses FKs; SQLite leaves them off unless asked.
        sqlx::query("PRAGMA foreign_keys = ON")
            .execute(&pool)
            .await
            .map_err(|e| StoreError::Connect(e.to_string()))?;
        Ok(Self { pool })
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// Load durable board rows into an in-memory `BoardState` (no live agent logs).
    pub async fn load_board_state(&self) -> Result<BoardState, StoreError> {
        let next_id = self.get_next_id().await?;
        let mut items_list = self.load_all_items().await?;
        // Attach blockers from the edge table.
        for item in &mut items_list {
            item.blocked_by = self.load_blockers(item.id).await?;
        }
        let mut items = BTreeMap::new();
        for item in items_list {
            items.insert(item.id, item);
        }
        let stories = self.load_all_stories().await?;
        let sandbox_profiles = self.load_sandbox_profiles().await?;
        let openshell_policies = self.load_openshell_policies().await?;
        let mcp_servers = self.load_mcp_servers().await?;
        let default_sandbox_profile_id = self.load_default_sandbox_profile_id().await?;
        let cockpit_sandbox_profile_id = self.load_cockpit_sandbox_profile_id().await?;
        let workspace = self.load_workspace_binding().await?;
        let openshell_gateway_endpoint = self.load_openshell_gateway_endpoint().await?;
        let openshell_auth_mode = self.load_openshell_auth_mode().await?;
        let openshell_oidc_config = self.load_openshell_oidc_config().await?;
        let openshell_mtls_sealed = self.load_openshell_mtls_sealed().await?;
        let openshell_oidc_sealed = self.load_openshell_oidc_sealed().await?;
        let github_app_sealed = self.load_github_app_sealed().await?;
        let github_app_installation_id = self.load_github_app_installation_id().await?;
        let auth_sealed = self.load_auth_sealed().await?;
        let auth_allowed_users = self.load_auth_allowed_users().await?;
        let auth_allowed_teams = self.load_auth_allowed_teams().await?;
        let agent_runtime = self.load_agent_runtime().await?;
        let openshell_providers = self.load_openshell_providers().await?;
        let openshell_provider_types = self.load_openshell_provider_types().await?;
        let openshell_provider_type_tombstones =
            self.load_openshell_provider_type_tombstones().await?;
        let webhook_poll = self.load_webhook_poll().await?;
        let webhook_poll_tips = self.load_webhook_poll_tips().await?;
        let webhook_poll_pr_reviews = self.load_webhook_poll_pr_reviews().await?;
        let github_repo_access = self.load_github_repo_access().await?;
        let cockpit_session = self.load_cockpit_session().await?;
        let mut state = BoardState {
            next_id,
            items,
            stories,
            sandbox_profiles,
            openshell_policies,
            mcp_servers,
            default_sandbox_profile_id,
            cockpit_sandbox_profile_id,
            workspace,
            openshell_gateway_endpoint,
            openshell_auth_mode,
            openshell_oidc_config,
            openshell_mtls_sealed,
            openshell_oidc_sealed,
            github_app_sealed,
            github_app_installation_id,
            auth_sealed,
            auth_allowed_users,
            auth_allowed_teams,
            agent_runtime,
            openshell_providers,
            openshell_provider_types,
            openshell_provider_type_tombstones,
            webhook_poll,
            webhook_poll_tips,
            webhook_poll_pr_reviews,
            github_repo_access,
            cockpit_session,
            ..Default::default()
        };
        state.rebuild_hot_indexes();
        Ok(state)
    }

    /// Replace durable rows with the in-memory snapshot (live agent logs stay in-process).
    pub async fn save_board_state(&self, state: &BoardState) -> Result<(), StoreError> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| StoreError::Query(e.to_string()))?;

        sqlx::query("DELETE FROM item_blockers")
            .execute(&mut *tx)
            .await
            .map_err(|e| StoreError::Query(e.to_string()))?;
        sqlx::query("DELETE FROM stories")
            .execute(&mut *tx)
            .await
            .map_err(|e| StoreError::Query(e.to_string()))?;
        sqlx::query("DELETE FROM items")
            .execute(&mut *tx)
            .await
            .map_err(|e| StoreError::Query(e.to_string()))?;

        // Items first, blockers second: `blocked_by` can point at a sibling
        // with a higher id (or any non-ancestor), so writing edges in the same
        // pass as rows trips SQLite FOREIGN KEY (787).
        let items: Vec<WorkItem> = state.items.values().cloned().collect();
        for item in parent_first(&items) {
            let nrc = state.non_retired_child_count(item.id) as i64;
            let obc = state.open_blocker_count(item) as i64;
            upsert_item_tx(&mut tx, item, nrc, obc).await?;
        }
        for item in &items {
            replace_blockers_tx(&mut tx, item.id, &item.blocked_by).await?;
        }

        for (&goal_id, lines) in &state.stories {
            // Drop story lines whose Project was deleted — otherwise INSERT
            // trips FOREIGN KEY (787) and the whole board fails to boot.
            if !state.items.contains_key(&goal_id) {
                tracing::warn!(
                    goal_id,
                    lines = lines.len(),
                    "skipping orphan stories (no matching item)"
                );
                continue;
            }
            replace_stories_tx(&mut tx, goal_id, lines).await?;
        }

        set_meta_tx(&mut tx, META_NEXT_ID, &state.next_id.to_string()).await?;
        let profiles_json = serde_json::to_string(&state.sandbox_profiles)
            .map_err(|e| StoreError::Query(format!("serialize sandbox_profiles: {e}")))?;
        set_meta_tx(&mut tx, META_SANDBOX_PROFILES, &profiles_json).await?;
        let policies_json = serde_json::to_string(&state.openshell_policies)
            .map_err(|e| StoreError::Query(format!("serialize openshell_policies: {e}")))?;
        set_meta_tx(&mut tx, META_OPENSHELL_POLICIES, &policies_json).await?;
        let mcp_servers_json = serde_json::to_string(&state.mcp_servers)
            .map_err(|e| StoreError::Query(format!("serialize mcp_servers: {e}")))?;
        set_meta_tx(&mut tx, META_MCP_SERVERS, &mcp_servers_json).await?;
        set_meta_tx(
            &mut tx,
            META_DEFAULT_SANDBOX_PROFILE_ID,
            state.default_sandbox_profile_id.as_deref().unwrap_or(""),
        )
        .await?;
        set_meta_tx(
            &mut tx,
            META_COCKPIT_SANDBOX_PROFILE_ID,
            state.cockpit_sandbox_profile_id.as_deref().unwrap_or(""),
        )
        .await?;
        let workspace_json = match &state.workspace {
            None => String::new(),
            Some(ws) => serde_json::to_string(ws)
                .map_err(|e| StoreError::Query(format!("serialize workspace_binding: {e}")))?,
        };
        set_meta_tx(&mut tx, META_WORKSPACE_BINDING, &workspace_json).await?;
        set_meta_tx(
            &mut tx,
            META_OPENSHELL_GATEWAY_ENDPOINT,
            state.openshell_gateway_endpoint.as_deref().unwrap_or(""),
        )
        .await?;
        set_meta_tx(
            &mut tx,
            META_OPENSHELL_AUTH_MODE,
            state
                .openshell_auth_mode
                .map(|m| m.as_str().to_string())
                .unwrap_or_default()
                .as_str(),
        )
        .await?;
        let oidc_cfg_json = match &state.openshell_oidc_config {
            None => String::new(),
            Some(cfg) => serde_json::to_string(cfg)
                .map_err(|e| StoreError::Query(format!("serialize openshell_oidc_config: {e}")))?,
        };
        set_meta_tx(&mut tx, META_OPENSHELL_OIDC_CONFIG, &oidc_cfg_json).await?;
        set_meta_tx(
            &mut tx,
            META_OPENSHELL_MTLS_SEALED,
            state.openshell_mtls_sealed.as_deref().unwrap_or(""),
        )
        .await?;
        set_meta_tx(
            &mut tx,
            META_OPENSHELL_OIDC_SEALED,
            state.openshell_oidc_sealed.as_deref().unwrap_or(""),
        )
        .await?;
        set_meta_tx(
            &mut tx,
            META_GITHUB_APP_SEALED,
            state.github_app_sealed.as_deref().unwrap_or(""),
        )
        .await?;
        set_meta_tx(
            &mut tx,
            META_GITHUB_APP_INSTALLATION_ID,
            &state
                .github_app_installation_id
                .map(|id| id.to_string())
                .unwrap_or_default(),
        )
        .await?;
        set_meta_tx(
            &mut tx,
            META_AUTH_SEALED,
            state.auth_sealed.as_deref().unwrap_or(""),
        )
        .await?;
        let users_json = serde_json::to_string(&state.auth_allowed_users)
            .map_err(|e| StoreError::Query(format!("serialize auth_allowed_users: {e}")))?;
        set_meta_tx(&mut tx, META_AUTH_ALLOWED_USERS, &users_json).await?;
        let teams_json = serde_json::to_string(&state.auth_allowed_teams)
            .map_err(|e| StoreError::Query(format!("serialize auth_allowed_teams: {e}")))?;
        set_meta_tx(&mut tx, META_AUTH_ALLOWED_TEAMS, &teams_json).await?;
        let agent_runtime_json = match &state.agent_runtime {
            None => String::new(),
            Some(rt) => serde_json::to_string(rt)
                .map_err(|e| StoreError::Query(format!("serialize agent_runtime: {e}")))?,
        };
        set_meta_tx(&mut tx, META_AGENT_RUNTIME, &agent_runtime_json).await?;
        let providers_json = serde_json::to_string(&state.openshell_providers)
            .map_err(|e| StoreError::Query(format!("serialize openshell_providers: {e}")))?;
        set_meta_tx(&mut tx, META_OPENSHELL_PROVIDERS, &providers_json).await?;
        let provider_types_json = serde_json::to_string(&state.openshell_provider_types)
            .map_err(|e| StoreError::Query(format!("serialize openshell_provider_types: {e}")))?;
        set_meta_tx(&mut tx, META_OPENSHELL_PROVIDER_TYPES, &provider_types_json).await?;
        let tombstones_json = serde_json::to_string(&state.openshell_provider_type_tombstones)
            .map_err(|e| {
                StoreError::Query(format!("serialize openshell_provider_type_tombstones: {e}"))
            })?;
        set_meta_tx(
            &mut tx,
            META_OPENSHELL_PROVIDER_TYPE_TOMBSTONES,
            &tombstones_json,
        )
        .await?;
        let webhook_poll_json = match &state.webhook_poll {
            None => String::new(),
            Some(cfg) => serde_json::to_string(cfg)
                .map_err(|e| StoreError::Query(format!("serialize webhook_poll: {e}")))?,
        };
        set_meta_tx(&mut tx, META_WEBHOOK_POLL, &webhook_poll_json).await?;
        let tips_json = serde_json::to_string(&state.webhook_poll_tips)
            .map_err(|e| StoreError::Query(format!("serialize webhook_poll_tips: {e}")))?;
        set_meta_tx(&mut tx, META_WEBHOOK_POLL_TIPS, &tips_json).await?;
        let pr_reviews_json = serde_json::to_string(&state.webhook_poll_pr_reviews)
            .map_err(|e| StoreError::Query(format!("serialize webhook_poll_pr_reviews: {e}")))?;
        set_meta_tx(&mut tx, META_WEBHOOK_POLL_PR_REVIEWS, &pr_reviews_json).await?;
        let repo_access_json = serde_json::to_string(&state.github_repo_access)
            .map_err(|e| StoreError::Query(format!("serialize github_repo_access: {e}")))?;
        set_meta_tx(&mut tx, META_GITHUB_REPO_ACCESS, &repo_access_json).await?;
        let cockpit_session_json = match &state.cockpit_session {
            None => String::new(),
            Some(session) => serde_json::to_string(session)
                .map_err(|e| StoreError::Query(format!("serialize cockpit_session: {e}")))?,
        };
        set_meta_tx(&mut tx, META_COCKPIT_SESSION, &cockpit_session_json).await?;

        tx.commit()
            .await
            .map_err(|e| StoreError::Query(e.to_string()))?;
        Ok(())
    }

    /// When the DB has never been populated, import `sandboard.json` once and stamp meta.
    /// Returns `true` if an import ran. Leaves the JSON file untouched.
    pub async fn import_json_if_empty(&self, json_path: &Path) -> Result<bool, StoreError> {
        if !self.is_empty().await? {
            return Ok(false);
        }
        let raw = match std::fs::read_to_string(json_path) {
            Ok(r) => r,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(e) => {
                return Err(StoreError::Query(format!(
                    "read {}: {e}",
                    json_path.display()
                )))
            }
        };
        let state: BoardState = serde_json::from_str(&raw)
            .map_err(|e| StoreError::Query(format!("parse {}: {e}", json_path.display())))?;
        self.save_board_state(&state).await?;
        self.meta_set(META_JSON_IMPORTED, &Utc::now().to_rfc3339())
            .await?;
        Ok(true)
    }

    async fn load_sandbox_profiles(&self) -> Result<BTreeMap<String, SandboxProfile>, StoreError> {
        match self.meta_get(META_SANDBOX_PROFILES).await? {
            None => Ok(BTreeMap::new()),
            Some(raw) if raw.is_empty() || raw == "{}" => Ok(BTreeMap::new()),
            Some(raw) => serde_json::from_str(&raw)
                .map_err(|e| StoreError::Query(format!("decode sandbox_profiles: {e}"))),
        }
    }

    async fn load_openshell_policies(
        &self,
    ) -> Result<BTreeMap<String, OpenShellPolicy>, StoreError> {
        match self.meta_get(META_OPENSHELL_POLICIES).await? {
            None => Ok(BTreeMap::new()),
            Some(raw) if raw.is_empty() || raw == "{}" => Ok(BTreeMap::new()),
            Some(raw) => serde_json::from_str(&raw)
                .map_err(|e| StoreError::Query(format!("decode openshell_policies: {e}"))),
        }
    }

    async fn load_mcp_servers(&self) -> Result<BTreeMap<String, McpServerDesired>, StoreError> {
        match self.meta_get(META_MCP_SERVERS).await? {
            None => Ok(BTreeMap::new()),
            Some(raw) if raw.is_empty() || raw == "{}" => Ok(BTreeMap::new()),
            Some(raw) => serde_json::from_str(&raw)
                .map_err(|e| StoreError::Query(format!("decode mcp_servers: {e}"))),
        }
    }

    async fn load_default_sandbox_profile_id(&self) -> Result<Option<String>, StoreError> {
        Ok(self
            .meta_get(META_DEFAULT_SANDBOX_PROFILE_ID)
            .await?
            .filter(|s| !s.is_empty()))
    }

    async fn load_cockpit_sandbox_profile_id(&self) -> Result<Option<String>, StoreError> {
        Ok(self
            .meta_get(META_COCKPIT_SANDBOX_PROFILE_ID)
            .await?
            .filter(|s| !s.is_empty()))
    }

    async fn load_workspace_binding(&self) -> Result<Option<WorkspaceBinding>, StoreError> {
        match self.meta_get(META_WORKSPACE_BINDING).await? {
            None => Ok(None),
            Some(raw) if raw.trim().is_empty() || raw == "null" => Ok(None),
            Some(raw) => serde_json::from_str(&raw)
                .map_err(|e| StoreError::Query(format!("decode workspace_binding: {e}"))),
        }
    }

    async fn load_openshell_gateway_endpoint(&self) -> Result<Option<String>, StoreError> {
        Ok(self
            .meta_get(META_OPENSHELL_GATEWAY_ENDPOINT)
            .await?
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()))
    }

    async fn load_openshell_auth_mode(&self) -> Result<Option<OpenShellAuthMode>, StoreError> {
        Ok(self
            .meta_get(META_OPENSHELL_AUTH_MODE)
            .await?
            .as_deref()
            .and_then(OpenShellAuthMode::parse))
    }

    async fn load_openshell_oidc_config(&self) -> Result<Option<OpenShellOidcConfig>, StoreError> {
        match self.meta_get(META_OPENSHELL_OIDC_CONFIG).await? {
            None => Ok(None),
            Some(raw) if raw.trim().is_empty() || raw == "null" => Ok(None),
            Some(raw) => serde_json::from_str(&raw)
                .map_err(|e| StoreError::Query(format!("decode openshell_oidc_config: {e}"))),
        }
    }

    async fn load_openshell_mtls_sealed(&self) -> Result<Option<String>, StoreError> {
        Ok(self
            .meta_get(META_OPENSHELL_MTLS_SEALED)
            .await?
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()))
    }

    async fn load_openshell_oidc_sealed(&self) -> Result<Option<String>, StoreError> {
        Ok(self
            .meta_get(META_OPENSHELL_OIDC_SEALED)
            .await?
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()))
    }

    async fn load_github_app_sealed(&self) -> Result<Option<String>, StoreError> {
        Ok(self
            .meta_get(META_GITHUB_APP_SEALED)
            .await?
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()))
    }

    async fn load_github_app_installation_id(&self) -> Result<Option<u64>, StoreError> {
        match self.meta_get(META_GITHUB_APP_INSTALLATION_ID).await? {
            None => Ok(None),
            Some(raw) if raw.trim().is_empty() => Ok(None),
            Some(raw) => {
                raw.trim().parse().map(Some).map_err(|e| {
                    StoreError::Query(format!("decode github_app_installation_id: {e}"))
                })
            }
        }
    }

    async fn load_auth_sealed(&self) -> Result<Option<String>, StoreError> {
        Ok(self
            .meta_get(META_AUTH_SEALED)
            .await?
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()))
    }

    async fn load_auth_allowed_users(&self) -> Result<Vec<String>, StoreError> {
        match self.meta_get(META_AUTH_ALLOWED_USERS).await? {
            None => Ok(Vec::new()),
            Some(raw) if raw.trim().is_empty() || raw == "null" => Ok(Vec::new()),
            Some(raw) => serde_json::from_str(&raw)
                .map_err(|e| StoreError::Query(format!("decode auth_allowed_users: {e}"))),
        }
    }

    async fn load_auth_allowed_teams(&self) -> Result<Vec<String>, StoreError> {
        match self.meta_get(META_AUTH_ALLOWED_TEAMS).await? {
            None => Ok(Vec::new()),
            Some(raw) if raw.trim().is_empty() || raw == "null" => Ok(Vec::new()),
            Some(raw) => serde_json::from_str(&raw)
                .map_err(|e| StoreError::Query(format!("decode auth_allowed_teams: {e}"))),
        }
    }

    async fn load_agent_runtime(&self) -> Result<Option<AgentRuntimeConfig>, StoreError> {
        match self.meta_get(META_AGENT_RUNTIME).await? {
            None => Ok(None),
            Some(raw) if raw.trim().is_empty() || raw == "null" => Ok(None),
            Some(raw) => serde_json::from_str(&raw)
                .map_err(|e| StoreError::Query(format!("decode agent_runtime: {e}"))),
        }
    }

    async fn load_openshell_providers(&self) -> Result<Vec<OpenShellProviderDesired>, StoreError> {
        match self.meta_get(META_OPENSHELL_PROVIDERS).await? {
            None => Ok(Vec::new()),
            Some(raw) if raw.trim().is_empty() || raw == "null" => Ok(Vec::new()),
            Some(raw) => serde_json::from_str(&raw)
                .map_err(|e| StoreError::Query(format!("decode openshell_providers: {e}"))),
        }
    }

    async fn load_openshell_provider_types(
        &self,
    ) -> Result<BTreeMap<String, crate::model::OpenShellProviderTypeDesired>, StoreError> {
        match self.meta_get(META_OPENSHELL_PROVIDER_TYPES).await? {
            None => Ok(BTreeMap::new()),
            Some(raw) if raw.trim().is_empty() || raw == "null" => Ok(BTreeMap::new()),
            Some(raw) => serde_json::from_str(&raw)
                .map_err(|e| StoreError::Query(format!("decode openshell_provider_types: {e}"))),
        }
    }

    async fn load_openshell_provider_type_tombstones(
        &self,
    ) -> Result<std::collections::BTreeSet<String>, StoreError> {
        match self.meta_get(META_OPENSHELL_PROVIDER_TYPE_TOMBSTONES).await? {
            None => Ok(std::collections::BTreeSet::new()),
            Some(raw) if raw.trim().is_empty() || raw == "null" => {
                Ok(std::collections::BTreeSet::new())
            }
            Some(raw) => serde_json::from_str(&raw).map_err(|e| {
                StoreError::Query(format!("decode openshell_provider_type_tombstones: {e}"))
            }),
        }
    }

    async fn load_webhook_poll(&self) -> Result<Option<WebhookPollConfig>, StoreError> {
        match self.meta_get(META_WEBHOOK_POLL).await? {
            None => Ok(None),
            Some(raw) if raw.trim().is_empty() || raw == "null" => Ok(None),
            Some(raw) => serde_json::from_str(&raw)
                .map_err(|e| StoreError::Query(format!("decode webhook_poll: {e}"))),
        }
    }

    async fn load_webhook_poll_tips(&self) -> Result<BTreeMap<String, String>, StoreError> {
        match self.meta_get(META_WEBHOOK_POLL_TIPS).await? {
            None => Ok(BTreeMap::new()),
            Some(raw) if raw.trim().is_empty() || raw == "null" => Ok(BTreeMap::new()),
            Some(raw) => serde_json::from_str(&raw)
                .map_err(|e| StoreError::Query(format!("decode webhook_poll_tips: {e}"))),
        }
    }

    async fn load_webhook_poll_pr_reviews(&self) -> Result<BTreeMap<String, u64>, StoreError> {
        match self.meta_get(META_WEBHOOK_POLL_PR_REVIEWS).await? {
            None => Ok(BTreeMap::new()),
            Some(raw) if raw.trim().is_empty() || raw == "null" => Ok(BTreeMap::new()),
            Some(raw) => serde_json::from_str(&raw)
                .map_err(|e| StoreError::Query(format!("decode webhook_poll_pr_reviews: {e}"))),
        }
    }

    async fn load_github_repo_access(
        &self,
    ) -> Result<crate::github_app::GitHubRepoAccessCache, StoreError> {
        match self.meta_get(META_GITHUB_REPO_ACCESS).await? {
            None => Ok(crate::github_app::GitHubRepoAccessCache::default()),
            Some(raw) if raw.trim().is_empty() || raw == "null" => {
                Ok(crate::github_app::GitHubRepoAccessCache::default())
            }
            Some(raw) => serde_json::from_str(&raw)
                .map_err(|e| StoreError::Query(format!("decode github_repo_access: {e}"))),
        }
    }

    async fn load_cockpit_session(&self) -> Result<Option<CockpitSession>, StoreError> {
        match self.meta_get(META_COCKPIT_SESSION).await? {
            None => Ok(None),
            Some(raw) if raw.trim().is_empty() || raw == "null" => Ok(None),
            Some(raw) => serde_json::from_str(&raw)
                .map_err(|e| StoreError::Query(format!("decode cockpit_session: {e}"))),
        }
    }
}

async fn set_meta_tx(
    tx: &mut Transaction<'_, Sqlite>,
    key: &str,
    value: &str,
) -> Result<(), StoreError> {
    sqlx::query(
        r#"
        INSERT INTO meta (key, value) VALUES (?, ?)
        ON CONFLICT(key) DO UPDATE SET value = excluded.value
        "#,
    )
    .bind(key)
    .bind(value)
    .execute(&mut **tx)
    .await
    .map_err(|e| StoreError::Query(e.to_string()))?;
    Ok(())
}

async fn upsert_item_tx(
    tx: &mut Transaction<'_, Sqlite>,
    item: &WorkItem,
    non_retired_child_count: i64,
    open_blocker_count: i64,
) -> Result<(), StoreError> {
    let row = item_to_row(item, non_retired_child_count, open_blocker_count)?;
    sqlx::query(
        r#"
        INSERT INTO items (
            id, parent_id, level, title, intent, definition_of_done, state,
            above_line, capability, run_deadline_at, parked, awaiting_dispatch,
            rebase_requested, entered_state_at, created_at,
            origin_json, lease_json, escalation_json, gates_json, notes_json,
            history_json, plan_json, proposal_json, extras_json,
            non_retired_child_count, open_blocker_count
        ) VALUES (
            ?, ?, ?, ?, ?, ?, ?,
            ?, ?, ?, ?, ?,
            ?, ?, ?,
            ?, ?, ?, ?, ?,
            ?, ?, ?, ?,
            ?, ?
        )
        ON CONFLICT(id) DO UPDATE SET
            parent_id = excluded.parent_id,
            level = excluded.level,
            title = excluded.title,
            intent = excluded.intent,
            definition_of_done = excluded.definition_of_done,
            state = excluded.state,
            above_line = excluded.above_line,
            capability = excluded.capability,
            run_deadline_at = excluded.run_deadline_at,
            parked = excluded.parked,
            awaiting_dispatch = excluded.awaiting_dispatch,
            rebase_requested = excluded.rebase_requested,
            entered_state_at = excluded.entered_state_at,
            created_at = excluded.created_at,
            origin_json = excluded.origin_json,
            lease_json = excluded.lease_json,
            escalation_json = excluded.escalation_json,
            gates_json = excluded.gates_json,
            notes_json = excluded.notes_json,
            history_json = excluded.history_json,
            plan_json = excluded.plan_json,
            proposal_json = excluded.proposal_json,
            extras_json = excluded.extras_json,
            non_retired_child_count = excluded.non_retired_child_count,
            open_blocker_count = excluded.open_blocker_count
        "#,
    )
    .bind(row.id as i64)
    .bind(row.parent_id.map(|p| p as i64))
    .bind(row.level)
    .bind(row.title)
    .bind(row.intent)
    .bind(row.definition_of_done)
    .bind(&row.state)
    .bind(row.above_line as i64)
    .bind(row.capability)
    .bind(row.run_deadline_at.as_deref())
    .bind(row.parked as i64)
    .bind(row.awaiting_dispatch as i64)
    .bind(row.rebase_requested as i64)
    .bind(&row.entered_state_at)
    .bind(&row.created_at)
    .bind(&row.origin_json)
    .bind(row.lease_json.as_deref())
    .bind(row.escalation_json.as_deref())
    .bind(&row.gates_json)
    .bind(&row.notes_json)
    .bind(&row.history_json)
    .bind(row.plan_json.as_deref())
    .bind(row.proposal_json.as_deref())
    .bind(&row.extras_json)
    .bind(row.non_retired_child_count)
    .bind(row.open_blocker_count)
    .execute(&mut **tx)
    .await
    .map_err(|e| StoreError::Query(e.to_string()))?;
    Ok(())
}

/// Recompute denorm columns for one item + its parent after a single-row upsert.
async fn refresh_denorm_tx(
    tx: &mut Transaction<'_, Sqlite>,
    item: &WorkItem,
) -> Result<(), StoreError> {
    // open_blocker_count from edge table + blocker state.
    sqlx::query(
        r#"
        UPDATE items SET open_blocker_count = (
            SELECT COUNT(*) FROM item_blockers ib
            JOIN items b ON b.id = ib.blocker_id
            WHERE ib.item_id = items.id
              AND b.state NOT IN ('done', 'retired')
        )
        WHERE id = ?
        "#,
    )
    .bind(item.id as i64)
    .execute(&mut **tx)
    .await
    .map_err(|e| StoreError::Query(e.to_string()))?;

    if let Some(parent) = item.parent {
        sqlx::query(
            r#"
            UPDATE items SET non_retired_child_count = (
                SELECT COUNT(*) FROM items c
                WHERE c.parent_id = items.id AND c.state != 'retired'
            )
            WHERE id = ?
            "#,
        )
        .bind(parent as i64)
        .execute(&mut **tx)
        .await
        .map_err(|e| StoreError::Query(e.to_string()))?;
    }

    sqlx::query(
        r#"
        UPDATE items SET non_retired_child_count = (
            SELECT COUNT(*) FROM items c
            WHERE c.parent_id = items.id AND c.state != 'retired'
        )
        WHERE id = ?
        "#,
    )
    .bind(item.id as i64)
    .execute(&mut **tx)
    .await
    .map_err(|e| StoreError::Query(e.to_string()))?;
    Ok(())
}

async fn replace_blockers_tx(
    tx: &mut Transaction<'_, Sqlite>,
    item_id: ItemId,
    blocker_ids: &[ItemId],
) -> Result<(), StoreError> {
    sqlx::query("DELETE FROM item_blockers WHERE item_id = ?")
        .bind(item_id as i64)
        .execute(&mut **tx)
        .await
        .map_err(|e| StoreError::Query(e.to_string()))?;
    for &bid in blocker_ids {
        sqlx::query("INSERT INTO item_blockers (item_id, blocker_id) VALUES (?, ?)")
            .bind(item_id as i64)
            .bind(bid as i64)
            .execute(&mut **tx)
            .await
            .map_err(|e| StoreError::Query(e.to_string()))?;
    }
    Ok(())
}

async fn replace_stories_tx(
    tx: &mut Transaction<'_, Sqlite>,
    goal_id: ItemId,
    lines: &[StoryLine],
) -> Result<(), StoreError> {
    sqlx::query("DELETE FROM stories WHERE goal_id = ?")
        .bind(goal_id as i64)
        .execute(&mut **tx)
        .await
        .map_err(|e| StoreError::Query(e.to_string()))?;
    for (pos, line) in lines.iter().enumerate() {
        sqlx::query("INSERT INTO stories (goal_id, position, at, text) VALUES (?, ?, ?, ?)")
            .bind(goal_id as i64)
            .bind(pos as i64)
            .bind(line.at.to_rfc3339())
            .bind(&line.text)
            .execute(&mut **tx)
            .await
            .map_err(|e| StoreError::Query(e.to_string()))?;
    }
    Ok(())
}

#[async_trait]
impl BoardStore for SqliteBoardStore {
    async fn meta_get(&self, key: &str) -> Result<Option<String>, StoreError> {
        let row: Option<(String,)> = sqlx::query_as("SELECT value FROM meta WHERE key = ?")
            .bind(key)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| StoreError::Query(e.to_string()))?;
        Ok(row.map(|(v,)| v))
    }

    async fn meta_set(&self, key: &str, value: &str) -> Result<(), StoreError> {
        sqlx::query(
            r#"
            INSERT INTO meta (key, value) VALUES (?, ?)
            ON CONFLICT(key) DO UPDATE SET value = excluded.value
            "#,
        )
        .bind(key)
        .bind(value)
        .execute(&self.pool)
        .await
        .map_err(|e| StoreError::Query(e.to_string()))?;
        Ok(())
    }

    async fn get_next_id(&self) -> Result<ItemId, StoreError> {
        match self.meta_get(META_NEXT_ID).await? {
            Some(v) => v
                .parse::<ItemId>()
                .map_err(|e| StoreError::Query(format!("next_id parse: {e}"))),
            None => Ok(1),
        }
    }

    async fn set_next_id(&self, next_id: ItemId) -> Result<(), StoreError> {
        self.meta_set(META_NEXT_ID, &next_id.to_string()).await
    }

    async fn is_empty(&self) -> Result<bool, StoreError> {
        if self.meta_get(META_JSON_IMPORTED).await?.is_some() {
            return Ok(false);
        }
        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM items")
            .fetch_one(&self.pool)
            .await
            .map_err(|e| StoreError::Query(e.to_string()))?;
        Ok(count == 0)
    }

    async fn upsert_item(&self, item: &WorkItem) -> Result<(), StoreError> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| StoreError::Query(e.to_string()))?;
        // Denorm refreshed after blockers land.
        upsert_item_tx(&mut tx, item, 0, 0).await?;
        replace_blockers_tx(&mut tx, item.id, &item.blocked_by).await?;
        refresh_denorm_tx(&mut tx, item).await?;
        tx.commit()
            .await
            .map_err(|e| StoreError::Query(e.to_string()))?;
        Ok(())
    }

    async fn delete_item(&self, id: ItemId) -> Result<(), StoreError> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| StoreError::Query(e.to_string()))?;
        sqlx::query("DELETE FROM item_blockers WHERE item_id = ? OR blocker_id = ?")
            .bind(id as i64)
            .bind(id as i64)
            .execute(&mut *tx)
            .await
            .map_err(|e| StoreError::Query(e.to_string()))?;
        sqlx::query("DELETE FROM stories WHERE goal_id = ?")
            .bind(id as i64)
            .execute(&mut *tx)
            .await
            .map_err(|e| StoreError::Query(e.to_string()))?;
        sqlx::query("DELETE FROM items WHERE id = ?")
            .bind(id as i64)
            .execute(&mut *tx)
            .await
            .map_err(|e| StoreError::Query(e.to_string()))?;
        tx.commit()
            .await
            .map_err(|e| StoreError::Query(e.to_string()))?;
        Ok(())
    }

    async fn get_item(&self, id: ItemId) -> Result<Option<WorkItem>, StoreError> {
        let row = sqlx::query("SELECT * FROM items WHERE id = ?")
            .bind(id as i64)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| StoreError::Query(e.to_string()))?;
        let Some(row) = row else {
            return Ok(None);
        };
        let mut item = item_from_row(&row)?;
        item.blocked_by = self.load_blockers(id).await?;
        Ok(Some(item))
    }

    async fn load_all_items(&self) -> Result<Vec<WorkItem>, StoreError> {
        let rows = sqlx::query("SELECT * FROM items ORDER BY id")
            .fetch_all(&self.pool)
            .await
            .map_err(|e| StoreError::Query(e.to_string()))?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            out.push(item_from_row(&row)?);
        }
        Ok(out)
    }

    async fn replace_blockers(
        &self,
        item_id: ItemId,
        blocker_ids: &[ItemId],
    ) -> Result<(), StoreError> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| StoreError::Query(e.to_string()))?;
        replace_blockers_tx(&mut tx, item_id, blocker_ids).await?;
        tx.commit()
            .await
            .map_err(|e| StoreError::Query(e.to_string()))?;
        Ok(())
    }

    async fn load_blockers(&self, item_id: ItemId) -> Result<Vec<ItemId>, StoreError> {
        let rows: Vec<(i64,)> = sqlx::query_as(
            "SELECT blocker_id FROM item_blockers WHERE item_id = ? ORDER BY blocker_id",
        )
        .bind(item_id as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| StoreError::Query(e.to_string()))?;
        Ok(rows.into_iter().map(|(id,)| id as ItemId).collect())
    }

    async fn replace_stories(
        &self,
        goal_id: ItemId,
        lines: &[StoryLine],
    ) -> Result<(), StoreError> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| StoreError::Query(e.to_string()))?;
        replace_stories_tx(&mut tx, goal_id, lines).await?;
        tx.commit()
            .await
            .map_err(|e| StoreError::Query(e.to_string()))?;
        Ok(())
    }

    async fn load_stories(&self, goal_id: ItemId) -> Result<Vec<StoryLine>, StoreError> {
        let rows: Vec<(String, String)> =
            sqlx::query_as("SELECT at, text FROM stories WHERE goal_id = ? ORDER BY position")
                .bind(goal_id as i64)
                .fetch_all(&self.pool)
                .await
                .map_err(|e| StoreError::Query(e.to_string()))?;
        let mut out = Vec::with_capacity(rows.len());
        for (at, text) in rows {
            let at = chrono::DateTime::parse_from_rfc3339(&at)
                .map(|d| d.with_timezone(&Utc))
                .map_err(|e| StoreError::Query(format!("story at: {e}")))?;
            out.push(StoryLine { at, text });
        }
        Ok(out)
    }

    async fn load_all_stories(&self) -> Result<BTreeMap<ItemId, Vec<StoryLine>>, StoreError> {
        let rows: Vec<(i64, i64, String, String)> = sqlx::query_as(
            "SELECT goal_id, position, at, text FROM stories ORDER BY goal_id, position",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| StoreError::Query(e.to_string()))?;
        let mut map: BTreeMap<ItemId, Vec<StoryLine>> = BTreeMap::new();
        for (goal_id, _pos, at, text) in rows {
            let at = chrono::DateTime::parse_from_rfc3339(&at)
                .map(|d| d.with_timezone(&Utc))
                .map_err(|e| StoreError::Query(format!("story at: {e}")))?;
            map.entry(goal_id as ItemId)
                .or_default()
                .push(StoryLine { at, text });
        }
        Ok(map)
    }

    async fn query_backlog(&self, capabilities: &[String]) -> Result<Vec<WorkItem>, StoreError> {
        // Uses idx_items_backlog_ready + denorm columns (not a full BTreeMap load).
        let rows = sqlx::query(
            r#"
            SELECT * FROM items
            WHERE state = 'backlog'
              AND (level IS NULL OR level != 'Project')
              AND non_retired_child_count = 0
              AND open_blocker_count = 0
            ORDER BY id
            "#,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| StoreError::Query(e.to_string()))?;

        let mut out = Vec::new();
        for row in rows {
            let mut item = item_from_row(&row)?;
            let cap_ok = match &item.capability {
                None => true,
                Some(c) if c == "any" => true,
                Some(c) => capabilities.iter().any(|have| have == c),
            };
            if !cap_ok {
                continue;
            }
            item.blocked_by = self.load_blockers(item.id).await?;
            out.push(item);
        }
        Ok(out)
    }

    async fn query_awaiting_dispatch(&self) -> Result<Vec<WorkItem>, StoreError> {
        // Uses idx_items_dispatch_queue + denorm leaf/blocker columns.
        let rows = sqlx::query(
            r#"
            SELECT * FROM items
            WHERE state = 'backlog'
              AND awaiting_dispatch = 1
              AND parked = 0
              AND (level IS NULL OR level != 'Project')
              AND non_retired_child_count = 0
              AND open_blocker_count = 0
            ORDER BY entered_state_at
            "#,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| StoreError::Query(e.to_string()))?;

        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let mut item = item_from_row(&row)?;
            item.blocked_by = self.load_blockers(item.id).await?;
            out.push(item);
        }
        Ok(out)
    }

    async fn query_expired_leases(&self, now: DateTime<Utc>) -> Result<Vec<ItemId>, StoreError> {
        // Primary filter uses idx_items_lease_sweep (state + run_deadline_at).
        // Legacy rows without run_deadline_at fall back to lease_json expiry.
        let now_s = now.to_rfc3339();
        let rows: Vec<(i64, Option<String>, Option<String>)> = sqlx::query_as(
            r#"
            SELECT id, run_deadline_at, lease_json FROM items
            WHERE state IN ('claimed', 'running')
              AND (
                (run_deadline_at IS NOT NULL AND run_deadline_at != '' AND run_deadline_at < ?)
                OR (
                  (run_deadline_at IS NULL OR run_deadline_at = '')
                  AND lease_json IS NOT NULL AND lease_json != ''
                )
              )
            "#,
        )
        .bind(&now_s)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| StoreError::Query(e.to_string()))?;

        let mut out = Vec::new();
        for (id, deadline, lease_json) in rows {
            let expired = if let Some(d) = deadline.as_deref().filter(|s| !s.is_empty()) {
                match DateTime::parse_from_rfc3339(d) {
                    Ok(dt) => now > dt.with_timezone(&Utc),
                    Err(_) => false,
                }
            } else if let Some(raw) = lease_json.as_deref().filter(|s| !s.is_empty()) {
                match serde_json::from_str::<crate::model::Lease>(raw) {
                    Ok(lease) => lease.is_expired(now),
                    Err(_) => false,
                }
            } else {
                false
            };
            if expired {
                out.push(id as ItemId);
            }
        }
        Ok(out)
    }

    async fn query_children_of(&self, id: ItemId) -> Result<Vec<ItemId>, StoreError> {
        let rows: Vec<(i64,)> =
            sqlx::query_as("SELECT id FROM items WHERE parent_id = ? ORDER BY id")
                .bind(id as i64)
                .fetch_all(&self.pool)
                .await
                .map_err(|e| StoreError::Query(e.to_string()))?;
        Ok(rows.into_iter().map(|(i,)| i as ItemId).collect())
    }

    async fn query_has_non_retired_children(&self, id: ItemId) -> Result<bool, StoreError> {
        let row: Option<(i64,)> =
            sqlx::query_as("SELECT non_retired_child_count FROM items WHERE id = ?")
                .bind(id as i64)
                .fetch_optional(&self.pool)
                .await
                .map_err(|e| StoreError::Query(e.to_string()))?;
        Ok(row.map(|(c,)| c > 0).unwrap_or(false))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::BoardStore;
    use crate::model::{Origin, State};
    use chrono::Utc;
    use std::sync::Arc;

    async fn mem_store() -> SqliteBoardStore {
        SqliteBoardStore::connect("sqlite::memory:")
            .await
            .expect("connect")
    }

    #[tokio::test]
    async fn round_trip_item_blockers_and_stories() {
        let store = mem_store().await;
        let mut parent = WorkItem::new(1, "Project", "why");
        parent.level = Some("Project".into());
        parent.state = State::Backlog;
        parent.project_prompt = Some("standing".into());
        parent.sandbox_profile_id = Some("default".into());

        let mut child = WorkItem::new(2, "Task", "do it");
        child.parent = Some(1);
        child.level = Some("Task".into());
        child.state = State::Backlog;
        child.blocked_by = vec![1];
        child.awaiting_dispatch = true;
        child.definition_of_done = Some("shipped".into());
        child.repo = Some(crate::schema::RepoConfig {
            upstream: "acme/sqlite".into(),
            fork: "bot/sqlite".into(),
            base: "main".into(),
        });

        store.upsert_item(&parent).await.expect("parent");
        store.upsert_item(&child).await.expect("child");
        store.set_next_id(3).await.expect("next_id");

        let line = StoryLine {
            at: Utc::now(),
            text: "kicked off".into(),
        };
        store
            .replace_stories(1, std::slice::from_ref(&line))
            .await
            .expect("stories");

        let loaded = store.get_item(2).await.expect("get").expect("exists");
        assert_eq!(loaded.title, "Task");
        assert_eq!(loaded.parent, Some(1));
        assert_eq!(loaded.blocked_by, vec![1]);
        assert!(loaded.awaiting_dispatch);
        assert_eq!(loaded.definition_of_done.as_deref(), Some("shipped"));
        let repo = loaded.repo.expect("task repo in extras");
        assert_eq!(repo.upstream, "acme/sqlite");
        assert_eq!(repo.fork, "bot/sqlite");

        let p = store.get_item(1).await.expect("get p").expect("p");
        assert_eq!(p.project_prompt.as_deref(), Some("standing"));
        assert!(p.repo.is_none(), "Project must not persist a product-repo");
        assert_eq!(p.sandbox_profile_id.as_deref(), Some("default"));

        let stories = store.load_stories(1).await.expect("stories");
        assert_eq!(stories.len(), 1);
        assert_eq!(stories[0].text, "kicked off");
        assert_eq!(store.get_next_id().await.unwrap(), 3);

        // Full snapshot round-trip including sandbox profile + policies catalog.
        let mut state = store.load_board_state().await.expect("load state");
        assert_eq!(state.items.len(), 2);
        assert_eq!(state.next_id, 3);
        state.openshell_policies.insert(
            "minimal".into(),
            crate::model::OpenShellPolicy {
                id: "minimal".into(),
                name: "Minimal".into(),
                yaml: "version: 1\n# sqlite-roundtrip\n".into(),
            },
        );
        state.sandbox_profiles.insert(
            "default".into(),
            SandboxProfile {
                id: "default".into(),
                name: "Default".into(),
                image: "img:1".into(),
                policy_id: "minimal".into(),
                policy_inline_legacy: None,
                cpu: Some("2".into()),
                memory: None,
                engine: None,
                model: Some("gpt-5".into()),
                provider_names: Vec::new(),
                mcp_server_ids: Vec::new(),
                env: Default::default(),
                prompt: None,
                shipped: false,
            },
        );
        state.default_sandbox_profile_id = Some("default".into());
        state.cockpit_sandbox_profile_id = Some("cockpit".into());
        state.workspace = Some(crate::model::WorkspaceBinding {
            forge: "github".into(),
        });
        store.save_board_state(&state).await.expect("save");
        let again = store.load_board_state().await.expect("reload");
        assert_eq!(again.items.get(&2).unwrap().blocked_by, vec![1]);
        assert_eq!(again.stories.get(&1).unwrap()[0].text, "kicked off");
        assert_eq!(
            again.items.get(&1).unwrap().sandbox_profile_id.as_deref(),
            Some("default")
        );
        assert_eq!(again.default_sandbox_profile_id.as_deref(), Some("default"));
        assert_eq!(again.cockpit_sandbox_profile_id.as_deref(), Some("cockpit"));
        assert_eq!(
            again.sandbox_profiles.get("default").unwrap().image,
            "img:1"
        );
        assert_eq!(
            again.sandbox_profiles.get("default").unwrap().policy_id,
            "minimal"
        );
        assert_eq!(
            again.sandbox_profiles.get("default").unwrap().model.as_deref(),
            Some("gpt-5")
        );
        assert!(
            again
                .openshell_policies
                .get("minimal")
                .unwrap()
                .yaml
                .contains("sqlite-roundtrip"),
            "policy YAML must round-trip via Policies catalog"
        );
        let ws = again.workspace.as_ref().expect("workspace round-trip");
        assert_eq!(ws.forge, "github");
        assert_eq!(ws.forge, "github");

        // Round-trip Agent runtime meta.
        let mut with_rt = again;
        with_rt.agent_runtime = Some(crate::model::AgentRuntimeConfig {
            engine: "agy".into(),
            max_concurrent: 1,
            agent_timeout_secs: 900,
            max_attempts: 3,
            ..Default::default()
        });
        store
            .save_board_state(&with_rt)
            .await
            .expect("save runtime");
        let rt_again = store.load_board_state().await.expect("reload runtime");
        let rt = rt_again
            .agent_runtime
            .as_ref()
            .expect("agent_runtime round-trip");
        assert_eq!(rt.engine, "agy");
        assert_eq!(rt.engine, "agy");
        assert_eq!(rt.max_concurrent, 1);

        // Cockpit session meta round-trip.
        let mut with_cockpit = rt_again;
        with_cockpit.cockpit_session = Some(crate::model::CockpitSession::new(
            Some("sandboard-cockpit".into()),
            Some("conv-db".into()),
        ));
        with_cockpit.cockpit_session.as_mut().unwrap().status =
            crate::model::CockpitSessionStatus::Parked;
        store
            .save_board_state(&with_cockpit)
            .await
            .expect("save cockpit session");
        let cockpit_again = store.load_board_state().await.expect("reload cockpit");
        let session = cockpit_again
            .cockpit_session
            .expect("cockpit_session round-trip");
        assert_eq!(session.environment.as_deref(), Some("sandboard-cockpit"));
        assert_eq!(session.conversation_id.as_deref(), Some("conv-db"));
        assert_eq!(session.status, crate::model::CockpitSessionStatus::Parked);
    }

    #[tokio::test]
    async fn save_board_state_allows_sibling_blocker_with_higher_id() {
        let store = mem_store().await;
        let mut parent = WorkItem::new(1, "Project", "why");
        parent.level = Some("Project".into());
        parent.state = State::Backlog;

        let mut early = WorkItem::new(2, "Early", "waits on later sibling");
        early.parent = Some(1);
        early.level = Some("Task".into());
        early.state = State::Backlog;
        early.blocked_by = vec![3];

        let mut later = WorkItem::new(3, "Later", "the blocker");
        later.parent = Some(1);
        later.level = Some("Task".into());
        later.state = State::Backlog;

        let mut items = BTreeMap::new();
        items.insert(1, parent);
        items.insert(2, early);
        items.insert(3, later);
        let state = BoardState {
            next_id: 4,
            items,
            ..Default::default()
        };
        store.save_board_state(&state).await.expect("save");
        let loaded = store.load_board_state().await.expect("load");
        assert_eq!(loaded.items.get(&2).unwrap().blocked_by, vec![3]);
    }

    #[tokio::test]
    async fn one_shot_json_import_and_no_repeat() {
        let dir = std::env::temp_dir().join(format!(
            "sandboard-import-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let json_path = dir.join("sandboard.json");

        let mut state = BoardState {
            next_id: 5,
            ..Default::default()
        };
        let mut item = WorkItem::new(4, "Imported", "from json");
        item.origin = Origin::Human;
        item.state = State::Backlog;
        state.items.insert(4, item);
        state.stories.insert(
            4,
            vec![StoryLine {
                at: Utc::now(),
                text: "hello".into(),
            }],
        );
        std::fs::write(&json_path, serde_json::to_string_pretty(&state).unwrap()).unwrap();

        let store = mem_store().await;
        assert!(store.is_empty().await.unwrap());
        assert!(store
            .import_json_if_empty(&json_path)
            .await
            .expect("import"));
        assert!(!store.is_empty().await.unwrap());

        let loaded = store.load_board_state().await.expect("load");
        assert_eq!(loaded.next_id, 5);
        assert_eq!(loaded.items.get(&4).unwrap().title, "Imported");
        assert_eq!(loaded.stories.get(&4).unwrap()[0].text, "hello");

        // Second boot: stamp present — no re-import even if we wipe items in JSON.
        std::fs::write(&json_path, "{}").unwrap();
        assert!(!store.import_json_if_empty(&json_path).await.expect("skip"));
        assert_eq!(store.get_item(4).await.unwrap().unwrap().title, "Imported");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn board_survives_restart_via_db() {
        let dir = std::env::temp_dir().join(format!(
            "sandboard-board-db-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("sandboard.db");
        let json_path = dir.join("sandboard.json");
        let url = format!("sqlite:{}", db_path.display());

        let store = Arc::new(
            crate::db::DurableBoardStore::connect(&url)
                .await
                .expect("connect file db"),
        );
        let schema = crate::schema::Schema::default();
        let board =
            crate::store::Board::load_with_store(schema.clone(), json_path.clone(), store.clone())
                .await
                .expect("open empty");

        let project = board
            .create(
                None,
                "DB Project",
                "persist me",
                None,
                Origin::Human,
                true,
                None,
            )
            .expect("create project");
        board
            .transition(project.id, State::Backlog, "test", None)
            .ok();
        board.story(project.id, "noted".into());
        board.flush();

        // Drop in-memory board; reopen from the same DB file.
        drop(board);
        let store2 = Arc::new(
            crate::db::DurableBoardStore::connect(&url)
                .await
                .expect("reconnect"),
        );
        let board2 = crate::store::Board::load_with_store(schema, json_path.clone(), store2)
            .await
            .expect("reopen");
        let restored = board2.get(project.id).expect("item survives");
        assert_eq!(restored.title, "DB Project");
        let stories = board2.stories_for(project.id);
        assert!(stories.iter().any(|s| s.text == "noted"));

        // Flush with a store attached must not create/rewrite sandboard.json.
        assert!(!json_path.exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn indexed_queries_match_board_filter_semantics() {
        let store = mem_store().await;
        let mut project = WorkItem::new(1, "Proj", "why");
        project.level = Some("Project".into());
        project.state = State::Backlog;

        let mut leaf = WorkItem::new(2, "leaf", "do");
        leaf.parent = Some(1);
        leaf.level = Some("Task".into());
        leaf.state = State::Backlog;
        leaf.capability = Some("rust".into());
        leaf.definition_of_done = Some("ship".into());

        let mut blocked = WorkItem::new(3, "blocked", "wait");
        blocked.parent = Some(1);
        blocked.level = Some("Task".into());
        blocked.state = State::Backlog;
        blocked.blocked_by = vec![2];
        blocked.definition_of_done = Some("ship".into());

        let mut claimed = WorkItem::new(4, "running-card", "go");
        claimed.parent = Some(1);
        claimed.level = Some("Task".into());
        claimed.state = State::Running;
        claimed.run_deadline_at = Some(Utc::now() - chrono::Duration::seconds(30));
        claimed.definition_of_done = Some("ship".into());

        let mut dispatch = WorkItem::new(5, "dispatch-me", "start");
        dispatch.parent = Some(1);
        dispatch.level = Some("Task".into());
        dispatch.state = State::Backlog;
        dispatch.awaiting_dispatch = true;
        dispatch.definition_of_done = Some("ship".into());

        let mut items = BTreeMap::new();
        items.insert(1, project);
        items.insert(2, leaf);
        items.insert(3, blocked);
        items.insert(4, claimed);
        items.insert(5, dispatch);
        let mut state = BoardState {
            next_id: 6,
            items,
            ..Default::default()
        };
        state.rebuild_hot_indexes();
        store.save_board_state(&state).await.expect("save");

        // Denorm columns persisted.
        let nrc: (i64,) = sqlx::query_as("SELECT non_retired_child_count FROM items WHERE id = 1")
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert_eq!(nrc.0, 4);
        let obc: (i64,) = sqlx::query_as("SELECT open_blocker_count FROM items WHERE id = 3")
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert_eq!(obc.0, 1);

        let backlog = store
            .query_backlog(&["rust".into()])
            .await
            .expect("backlog");
        let ids: Vec<_> = backlog.iter().map(|i| i.id).collect();
        assert!(ids.contains(&2), "rust leaf: {ids:?}");
        assert!(!ids.contains(&3), "blocked excluded");
        assert!(!ids.contains(&1), "project excluded");
        assert!(ids.contains(&5), "dispatch leaf still backlog");

        let q = store.query_awaiting_dispatch().await.expect("dispatch");
        assert_eq!(q.iter().map(|i| i.id).collect::<Vec<_>>(), vec![5]);

        let kids = store.query_children_of(1).await.expect("children");
        assert_eq!(kids.len(), 4);
        assert!(store.query_has_non_retired_children(1).await.unwrap());
        assert!(!store.query_has_non_retired_children(2).await.unwrap());

        let expired = store
            .query_expired_leases(Utc::now())
            .await
            .expect("leases");
        assert_eq!(expired, vec![4]);
    }
}
