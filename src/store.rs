//! The board. Written to at machine speed, read by agents as their source of
//! truth, and moving a card *is* an action.
//!
//! Both faces — REST/SSE for humans, MCP for the operator and for agents — call
//! into here. Neither owns any state-machine logic, which is what keeps the two
//! renderings from drifting.

use crate::db::DurableBoardStore;
use crate::events::BoardEvent;
use crate::machine::{self, TransitionError};
use crate::model::*;
use crate::schema::{AgentConfig, Level, RepoConfig, Schema};

use chrono::{DateTime, Duration, Utc};
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::broadcast;

// ---------------------------------------------------------------- persistence

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct BoardState {
    pub next_id: ItemId,
    pub items: BTreeMap<ItemId, WorkItem>,
    /// Per-goal running narrative. A few lines appended at meaningful moments,
    /// not an event log — most people will read this instead of the board, and
    /// they will be right to.
    #[serde(default)]
    pub stories: BTreeMap<ItemId, Vec<StoryLine>>,
    /// Named sandbox create profiles. Empty catalogs seed `default`+`cockpit`
    /// from compiled [`AgentConfig::default`] / embedded policy constants.
    #[serde(default)]
    pub sandbox_profiles: BTreeMap<String, SandboxProfile>,
    /// Board-owned OpenShell Policies catalog (id → named YAML). Specs
    /// reference these via `SandboxProfile.policy_id`.
    #[serde(default)]
    pub openshell_policies: BTreeMap<String, OpenShellPolicy>,
    /// Board-owned MCP server catalog (Settings → OpenShell → MCP servers).
    #[serde(default)]
    pub mcp_servers: BTreeMap<String, McpServerDesired>,
    /// Global default profile id. Projects may override via `sandbox_profile_id`.
    #[serde(default)]
    pub default_sandbox_profile_id: Option<String>,
    /// Profile used when Cockpit Start creates the cockpit sandbox.
    #[serde(default)]
    pub cockpit_sandbox_profile_id: Option<String>,
    /// Per-install forge binding. Seeded to `github` when unset; Board is SoT after.
    #[serde(default)]
    pub workspace: Option<WorkspaceBinding>,
    /// Gateway URL for OpenShell clients (Settings → OpenShell). Not secret. Must be https.
    #[serde(default)]
    pub openshell_gateway_endpoint: Option<String>,
    /// Explicit auth mode (`mtls` | `oidc`). Never inferred.
    #[serde(default)]
    pub openshell_auth_mode: Option<crate::model::OpenShellAuthMode>,
    /// Non-secret OIDC issuer/client/audience when mode is oidc.
    #[serde(default)]
    pub openshell_oidc_config: Option<crate::model::OpenShellOidcConfig>,
    /// Sealed mTLS PEMs (DB ciphertext). Decrypt only via `secrets`; never expose on GET.
    #[serde(default)]
    pub openshell_mtls_sealed: Option<String>,
    /// Sealed OIDC tokens (DB ciphertext). Decrypt only via `secrets`; never expose on GET.
    #[serde(default)]
    pub openshell_oidc_sealed: Option<String>,
    /// Sealed GitHub App credentials (DB ciphertext). Decrypt only via `secrets`.
    #[serde(default)]
    pub github_app_sealed: Option<String>,
    /// GitHub App installation id for minting sandbox `GH_TOKEN`s.
    #[serde(default)]
    pub github_app_installation_id: Option<u64>,
    /// Sealed local-admin auth (password hash + session key). Decrypt via `secrets`.
    #[serde(default)]
    pub auth_sealed: Option<String>,
    /// GitHub logins allowed to Sign in with GitHub (not secret).
    #[serde(default)]
    pub auth_allowed_users: Vec<String>,
    /// Org teams (`org/team_slug`) whose members may Sign in with GitHub.
    #[serde(default)]
    pub auth_allowed_teams: Vec<String>,
    /// Process agent knobs (Settings → Agent runtime). Seeded from compiled
    /// defaults; Board SoT after.
    #[serde(default)]
    pub agent_runtime: Option<AgentRuntimeConfig>,
    /// Desired OpenShell providers (Settings → OpenShell → Providers). Board SoT.
    #[serde(default)]
    pub openshell_providers: Vec<OpenShellProviderDesired>,
    /// Custom OpenShell provider type profiles (YAML). Board SoT; Sync imports.
    #[serde(default)]
    pub openshell_provider_types: BTreeMap<String, OpenShellProviderTypeDesired>,
    /// Shipped type ids the operator deleted — ensure will not resurrect them.
    #[serde(default)]
    pub openshell_provider_type_tombstones: BTreeSet<String>,
    /// Settings → Forge: webhook polling fallback.
    #[serde(default)]
    pub webhook_poll: Option<WebhookPollConfig>,
    /// Last-seen default-branch tip SHAs keyed by `owner/name` (poll path).
    #[serde(default)]
    pub webhook_poll_tips: BTreeMap<String, String>,
    /// Last-seen PR review ids keyed by `owner/name#number` (poll path).
    /// First observation seeds without applying historical reviews.
    #[serde(default)]
    pub webhook_poll_pr_reviews: BTreeMap<String, u64>,
    /// GitHub App `owner/repo` → installation lookup (Settings → Repo access).
    #[serde(default)]
    pub github_repo_access: crate::github_app::GitHubRepoAccessCache,
    /// Durable control-plane cockpit (sandbox + conversation + hold).
    /// Singleton — not a WorkItem; reconnect reads this, not a chatbot shim.
    #[serde(default)]
    pub cockpit_session: Option<CockpitSession>,
    /// Parent → child ids. Rebuilt after load; maintained on create/delete.
    /// Avoids full `items` scans in `children_of` / `has_children` / snapshot members.
    #[serde(skip)]
    pub children_by_parent: BTreeMap<ItemId, BTreeSet<ItemId>>,
    /// State → item ids. Rebuilt after load; maintained on create/delete/transition.
    /// Avoids full `items` scans in list_backlog / list_awaiting_dispatch / sweep_leases.
    #[serde(skip)]
    pub ids_by_state: HashMap<State, BTreeSet<ItemId>>,
}

impl BoardState {
    /// Snapshot for durable flush — drops in-process-only agent log rings.
    fn clone_for_persist(&self) -> Self {
        Self {
            next_id: self.next_id,
            items: self.items.clone(),
            stories: self.stories.clone(),
            sandbox_profiles: self.sandbox_profiles.clone(),
            openshell_policies: self.openshell_policies.clone(),
            mcp_servers: self.mcp_servers.clone(),
            default_sandbox_profile_id: self.default_sandbox_profile_id.clone(),
            cockpit_sandbox_profile_id: self.cockpit_sandbox_profile_id.clone(),
            workspace: self.workspace.clone(),
            openshell_gateway_endpoint: self.openshell_gateway_endpoint.clone(),
            openshell_auth_mode: self.openshell_auth_mode,
            openshell_oidc_config: self.openshell_oidc_config.clone(),
            openshell_mtls_sealed: self.openshell_mtls_sealed.clone(),
            openshell_oidc_sealed: self.openshell_oidc_sealed.clone(),
            github_app_sealed: self.github_app_sealed.clone(),
            github_app_installation_id: self.github_app_installation_id,
            auth_sealed: self.auth_sealed.clone(),
            auth_allowed_users: self.auth_allowed_users.clone(),
            auth_allowed_teams: self.auth_allowed_teams.clone(),
            agent_runtime: self.agent_runtime.clone(),
            openshell_providers: self.openshell_providers.clone(),
            openshell_provider_types: self.openshell_provider_types.clone(),
            openshell_provider_type_tombstones: self.openshell_provider_type_tombstones.clone(),
            webhook_poll: self.webhook_poll.clone(),
            webhook_poll_tips: self.webhook_poll_tips.clone(),
            webhook_poll_pr_reviews: self.webhook_poll_pr_reviews.clone(),
            github_repo_access: self.github_repo_access.clone(),
            cockpit_session: self.cockpit_session.clone(),
            children_by_parent: BTreeMap::new(),
            ids_by_state: HashMap::new(),
        }
    }

    /// Rebuild secondary indexes from `items`. Call after JSON/DB load.
    pub fn rebuild_hot_indexes(&mut self) {
        self.children_by_parent.clear();
        self.ids_by_state.clear();
        let snapshot: Vec<(ItemId, Option<ItemId>, State)> = self
            .items
            .values()
            .map(|i| (i.id, i.parent, i.state))
            .collect();
        for (id, parent, state) in snapshot {
            if let Some(p) = parent {
                self.children_by_parent.entry(p).or_default().insert(id);
            }
            self.ids_by_state.entry(state).or_default().insert(id);
        }
    }

    fn index_link_item(&mut self, item: &WorkItem) {
        if let Some(p) = item.parent {
            self.children_by_parent
                .entry(p)
                .or_default()
                .insert(item.id);
        }
        self.ids_by_state
            .entry(item.state)
            .or_default()
            .insert(item.id);
    }

    fn index_unlink_item(&mut self, item: &WorkItem) {
        if let Some(p) = item.parent {
            if let Some(set) = self.children_by_parent.get_mut(&p) {
                set.remove(&item.id);
                if set.is_empty() {
                    self.children_by_parent.remove(&p);
                }
            }
        }
        if let Some(set) = self.ids_by_state.get_mut(&item.state) {
            set.remove(&item.id);
            if set.is_empty() {
                self.ids_by_state.remove(&item.state);
            }
        }
    }

    fn index_set_state(&mut self, id: ItemId, from: State, to: State) {
        if from == to {
            return;
        }
        if let Some(set) = self.ids_by_state.get_mut(&from) {
            set.remove(&id);
            if set.is_empty() {
                self.ids_by_state.remove(&from);
            }
        }
        self.ids_by_state.entry(to).or_default().insert(id);
    }

    /// Insert a new item and update hot indexes.
    pub fn insert_item(&mut self, item: WorkItem) {
        self.index_link_item(&item);
        self.items.insert(item.id, item);
    }

    /// Remove an item and update hot indexes.
    pub fn remove_item(&mut self, id: ItemId) -> Option<WorkItem> {
        let item = self.items.remove(&id)?;
        self.index_unlink_item(&item);
        Some(item)
    }

    /// Non-retired child count (denorm field mirrored into SQLite on flush).
    pub fn non_retired_child_count(&self, id: ItemId) -> u32 {
        self.children_by_parent
            .get(&id)
            .map(|kids| {
                kids.iter()
                    .filter(|cid| {
                        self.items
                            .get(cid)
                            .map(|i| i.state != State::Retired)
                            .unwrap_or(false)
                    })
                    .count() as u32
            })
            .unwrap_or(0)
    }

    /// Unresolved blocker count (denorm field mirrored into SQLite on flush).
    pub fn open_blocker_count(&self, item: &WorkItem) -> u32 {
        item.blocked_by
            .iter()
            .filter(|b| {
                self.items
                    .get(b)
                    .map(|i| !i.state.is_terminal())
                    .unwrap_or(false)
            })
            .count() as u32
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoryLine {
    pub at: DateTime<Utc>,
    pub text: String,
}

// ------------------------------------------------------------------ read views

#[derive(Debug, Clone, Serialize)]
pub struct ChunkSummary {
    pub count: usize,
    /// Chunked, never compressed: `+7 more` hides seven items and tells you
    /// nothing. `7 in backlog · 2 blocked on #41 · oldest 40m` is smaller *and*
    /// answers the question.
    pub text: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ColumnView {
    pub column: Column,
    pub summary: ChunkSummary,
}

/// Child card summary for Project `item_detail` (and similar).
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema, PartialEq, Eq)]
pub struct ChildSummary {
    pub id: ItemId,
    pub title: String,
    /// Debug-style state name (`Retired`, `Running`, …) — matches MCP card lines.
    pub state: String,
    /// Last transition reason when present (retire/cut reasons show up here).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_reason: Option<String>,
}

/// A leaf retired under an active Project — mid-flight scope cuts the snapshot
/// used to hide, which made "what did we just cut?" unanswerable from MCP alone.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema, PartialEq, Eq)]
pub struct RetiredSnippet {
    pub id: ItemId,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub at: DateTime<Utc>,
}

/// Keyword search hit across title / intent / DoD / notes / history reasons.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema, PartialEq, Eq)]
pub struct SearchHit {
    pub id: ItemId,
    pub title: String,
    pub state: String,
    /// Containing Project id when the hit is a child Task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal: Option<ItemId>,
    /// Where the query matched (`title`, `intent`, `definition_of_done`, `notes`, `history`).
    pub matched_in: String,
    /// Short excerpt for triage (not the full field).
    pub detail: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct GoalView {
    pub id: ItemId,
    pub title: String,
    pub intent: String,
    pub progress: f32,
    pub leaves_done: usize,
    pub leaves_total: usize,
    pub agents_live: usize,
    pub needs_you: usize,
    /// Project auto mode — supervisor queues claimable Backlog leaves.
    #[serde(default)]
    pub auto_dispatch: bool,
    /// `no_plan` | `awaiting_approval` | `approved_vN`
    pub plan_status: String,
    /// Soft-retired Project — hidden from the default board view, available via
    /// "Show archived". Digests still omit these.
    pub archived: bool,
    pub columns: Vec<ColumnView>,
    pub story: Vec<StoryLine>,
    /// Recent retired children on an *active* Project (newest first, capped).
    /// Empty for archived Projects — the whole tree is already out of scope.
    #[serde(default)]
    pub recent_retired: Vec<RetiredSnippet>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Snapshot {
    pub items: Vec<WorkItem>,
    pub levels: Vec<Level>,
    pub goals: Vec<GoalView>,
    pub server_time: DateTime<Utc>,
    /// Wall-clock cap for a run (same as `agents.agent_timeout_secs`).
    pub agent_timeout_secs: u64,
    pub seq: u64,
    pub default_engine: String,
    pub default_model: String,
}

/// What `claim` hands back. The card alone would be a title; the chain is why
/// the agent knows "air-gapped" rules out the SDK's phone-home telemetry.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AncestryLine {
    pub level: String,
    pub title: String,
    pub intent: String,
}

#[derive(Debug, Clone, Default)]
struct ClaimPlanContext {
    project_title: Option<String>,
    project_prompt: Option<String>,
    plan_summary: Option<String>,
    plan_tasks: Vec<crate::model::PlanTaskBrief>,
    plan_task_key: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ClaimGrant {
    pub item_id: ItemId,
    pub title: String,
    /// Card intent from the board WorkItem — injected into the agent briefing
    /// so sandboxed agents never need `bd show` for description context.
    pub intent: String,
    pub definition_of_done: Option<String>,
    /// Containing Project title (when this card is a Task).
    pub project_title: Option<String>,
    /// Board standing prompt from Settings → Agent runtime (`standing_prompt`).
    pub board_prompt: Option<String>,
    /// Project-only standing extras (`project_prompt`); board policy is separate.
    pub project_prompt: Option<String>,
    /// Resolved sandbox profile prompt at claim (seat notes for this run).
    pub sandbox_prompt: Option<String>,
    /// Plan summary from the Project artifact.
    pub plan_summary: Option<String>,
    /// Plan tasks (deps included); `current` marks this card's row when known.
    pub plan_tasks: Vec<crate::model::PlanTaskBrief>,
    /// Plan key for this card when matched via `item_id`.
    pub plan_task_key: Option<String>,
    pub notes: Vec<String>,
    /// Alias of `run_deadline_at` at claim — not extended by heartbeats.
    pub lease_expires_at: DateTime<Utc>,
    pub run_deadline_at: DateTime<Utc>,
    pub engine: Option<String>,
    /// Resolved model for agent CLI argv (card → spec → engine default).
    pub model: Option<String>,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct NeedsYou {
    pub id: ItemId,
    pub title: String,
    pub question: String,
    pub options: Vec<String>,
    pub recommended: usize,
    pub blocked_secs: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema, PartialEq)]
pub struct ReadyCard {
    pub id: ItemId,
    pub title: String,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct GoalDigest {
    pub goal_id: ItemId,
    pub goal: String,
    pub merged: usize,
    pub needs_you: Vec<NeedsYou>,
    pub running: usize,
    pub running_stalled: usize,
    pub backlog: usize,
    pub in_review: usize,
    pub ready_to_dispatch: Vec<ReadyCard>,
    pub latest_story: Option<String>,
    /// Mid-project cuts still on this live goal (newest first, capped).
    #[serde(default)]
    pub recently_retired: Vec<RetiredSnippet>,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct Digest {
    pub since: DateTime<Utc>,
    pub goals: Vec<GoalDigest>,
}

pub const DEFAULT_EVENT_BUFFER_CAPACITY: usize = 1000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum CatchUpResult {
    /// Replayed missed events in sequence order.
    Events(Vec<BoardEvent>),
    /// Lagged beyond buffer capacity or future sequence; client must reset state.
    Reset { seq: u64 },
}

// ------------------------------------------------------------------ the board

pub struct Board {
    state: RwLock<BoardState>,
    /// Live agent stdout rings for the drawer. Separate from `state` so the
    /// per-line stream follower does not take the board write lock (that lock
    /// serializes every `/api/items` / `/api/board` read and hung the UI).
    live_agent_logs: Mutex<BTreeMap<ItemId, std::collections::VecDeque<String>>>,
    tx: broadcast::Sender<BoardEvent>,
    seq: AtomicU64,
    event_buffer: RwLock<std::collections::VecDeque<BoardEvent>>,
    buffer_capacity: usize,
    dirty: AtomicBool,
    pub schema: Schema,
    /// Legacy JSON path: import source when the DB is empty.
    /// When [`Self::store`] is set, flush no longer rewrites this file.
    path: PathBuf,
    /// SQLite or Postgres row store. `None` in unit tests that stay in-memory/JSON.
    store: Option<Arc<DurableBoardStore>>,
    started_at: DateTime<Utc>,
    pub openshell: Option<crate::openshell::OpenShell>,
    /// Last minted installation-token expiry (in-process; not durable).
    github_app_token_cache: Mutex<crate::github_app::TokenCache>,
}

pub type SharedBoard = Arc<Board>;

/// Outcome of a Review catch-up check after main advanced (GitHub API mergeable).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RebaseOutcome {
    /// GitHub reports MERGEABLE — stay in Review; no catch-up work signal.
    Clean,
    /// GitHub reports CONFLICTING — bounce to Backlog for an agent to rebase.
    Conflict {
        conflicting_files: Vec<String>,
        reason: Option<String>,
    },
}

/// Binding note when a Review card bounces because GitHub reports CONFLICTING.
/// Must reach the next claim briefing so a reused conversation does not
/// hollow-report while the PR is still unmergeable.
pub fn conflict_bounce_note(conflicting_files: &[String]) -> String {
    let files = if conflicting_files.is_empty() {
        "GitHub PR mergeable is CONFLICTING".to_string()
    } else {
        format!("conflicting files: {}", conflicting_files.join(", "))
    };
    format!(
        "BINDING: main advanced — {files}. \
         do-not-re-report-while-CONFLICTING; rebase onto upstream base before finishing."
    )
}

/// Parse `https://github.com/{owner}/{repo}/pull/{n}` (optional scheme / trailing slash).
/// Returns `(owner/repo, pull_number)`.
pub fn parse_github_pr_url(url: &str) -> Option<(String, u64)> {
    let url = url.trim().trim_end_matches('/');
    let marker = "github.com/";
    let idx = url.to_ascii_lowercase().find(marker)?;
    let rest = &url[idx + marker.len()..];
    let mut parts = rest.split('/');
    let owner = parts.next()?.trim();
    let repo = parts.next()?.trim();
    let pull = parts.next()?;
    let num = parts.next()?;
    if !pull.eq_ignore_ascii_case("pull") || owner.is_empty() || repo.is_empty() {
        return None;
    }
    // Ignore query/fragment on the number segment.
    let num = num.split(&['?', '#'][..]).next().unwrap_or(num);
    let n: u64 = num.parse().ok()?;
    Some((format!("{owner}/{repo}"), n))
}

impl Board {
    pub fn new(schema: Schema, path: PathBuf) -> Self {
        let (tx, _) = broadcast::channel(1024);
        Self {
            state: RwLock::new(BoardState {
                next_id: 1,
                ..Default::default()
            }),
            live_agent_logs: Mutex::new(BTreeMap::new()),
            tx,
            seq: AtomicU64::new(0),
            event_buffer: RwLock::new(std::collections::VecDeque::new()),
            buffer_capacity: DEFAULT_EVENT_BUFFER_CAPACITY,
            dirty: AtomicBool::new(false),
            schema,
            path,
            store: None,
            started_at: Utc::now(),
            // None → build from Settings (endpoint + sealed mTLS). Tests inject mocks.
            openshell: None,
            github_app_token_cache: Mutex::new(crate::github_app::TokenCache::default()),
        }
    }

    #[allow(dead_code)]
    pub fn with_buffer_capacity(mut self, capacity: usize) -> Self {
        self.buffer_capacity = capacity;
        self
    }

    /// Apply legacy renames on load. Returns whether the state was mutated.
    fn heal_loaded_state(state: &mut BoardState) -> (usize, usize) {
        let mut healed = 0usize;
        let mut renamed = 0usize;
        let rename_to: Vec<(ItemId, String)> = state
            .items
            .values()
            .filter(|i| i.title == crate::model::INITIAL_PLAN_TITLE_LEGACY)
            .filter_map(|i| {
                let parent = i.parent?;
                let pname = state.items.get(&parent)?.title.clone();
                Some((i.id, crate::model::initial_plan_title(&pname)))
            })
            .collect();
        for (id, title) in rename_to {
            if let Some(item) = state.items.get_mut(&id) {
                item.title = title;
                renamed += 1;
            }
        }
        for item in state.items.values_mut() {
            item.migrate_legacy_pr_url();
            // A brief experiment left Initial plan in Shaping; restore
            // them to Backlog so dedicated planning agents can claim.
            if item.is_initial_plan_task() && item.state == State::Shaping {
                item.state = State::Backlog;
                healed += 1;
            }
        }
        (healed, renamed)
    }

    /// Load a previously persisted board from `sandboard.json`, or start empty.
    ///
    /// Prefer [`Self::load_with_store`] in production — JSON is the one-shot
    /// import source once a `BoardStore` is attached.
    pub fn load_or_new(schema: Schema, path: PathBuf) -> Self {
        let board = Self::new(schema, path.clone());
        if let Ok(raw) = std::fs::read_to_string(&path) {
            match serde_json::from_str::<BoardState>(&raw) {
                Ok(mut state) => {
                    let (healed, renamed) = Self::heal_loaded_state(&mut state);
                    tracing::info!(items = state.items.len(), "restored board from {path:?}");
                    if healed > 0 {
                        tracing::info!("healed {healed} Initial plan Task(s) Shaping → Backlog");
                    }
                    if renamed > 0 {
                        tracing::info!(
                            "renamed {renamed} Initial plan Task(s) to include Project name"
                        );
                    }
                    state.rebuild_hot_indexes();
                    *board.state.write() = state;
                    let path_migrated = board.migrate_sandbox_policies_to_inline();
                    let catalog_migrated = board.migrate_inline_policies_to_catalog();
                    if healed > 0 || renamed > 0 || path_migrated > 0 || catalog_migrated > 0 {
                        if path_migrated > 0 {
                            tracing::info!(
                                "migrated {path_migrated} sandbox profile polic(ies) from host path to inline YAML"
                            );
                        }
                        if catalog_migrated > 0 {
                            tracing::info!(
                                "migrated {catalog_migrated} inline sandbox polic(ies) into Policies catalog"
                            );
                        }
                        board.dirty.store(true, Ordering::Relaxed);
                        board.flush();
                    }
                }
                Err(e) => tracing::warn!("ignoring unreadable {path:?}: {e}"),
            }
        }
        // Operator-created sandbox specs are left alone. The five
        // sandbox-<engine> rows below are the exception: shipped, one per
        // split image, so Cockpit works before an operator writes anything.
        if board.ensure_minimal_policy() {
            tracing::info!("seeded minimal OpenShell policy");
            board.flush();
        }
        if board.ensure_shipped_mcp_servers() {
            tracing::info!("seeded shipped MCP servers");
            board.flush();
        }
        if board.ensure_shipped_sandbox_profiles() {
            tracing::info!("seeded shipped per-engine sandbox profiles");
            board.flush();
        }
        if board.ensure_cockpit_sandboard_mcp_attach() {
            tracing::info!("attached shipped sandboard MCP to Cockpit sandbox profile");
            board.flush();
        }
        if board.migrate_github_app_provider_name() {
            tracing::info!("migrated GitHub App provider attach name github → github-app");
            board.flush();
        }
        if board.migrate_github_app_sealed_into_provider() {
            tracing::info!("migrated GitHub App sealed credentials onto provider github-app");
            board.flush();
        }
        if board.prune_sandbox_attach_names() {
            tracing::info!("pruned sandbox attach names that are not in Providers");
            board.flush();
        }
        if board.seed_workspace_binding_if_empty() {
            tracing::info!("seeded workspace forge binding");
            board.flush();
        }
        if board.seed_agent_runtime_if_empty() {
            tracing::info!("seeded agent runtime from compiled defaults");
            board.flush();
        }
        if crate::provider_types::ensure_shipped_on_board(&board) > 0 {
            tracing::info!("seeded shipped OpenShell provider types");
            board.flush();
        }
        board
    }

    /// Boot from the configured board database: one-shot import from `json_path`
    /// when the DB is empty, otherwise restore rows. Mutations flush as row
    /// updates, not a JSON rewrite.
    pub async fn load_with_store(
        schema: Schema,
        json_path: PathBuf,
        store: Arc<DurableBoardStore>,
    ) -> Result<Self, crate::db::StoreError> {
        let imported = store.import_json_if_empty(&json_path).await?;
        if imported {
            tracing::info!(
                path = %json_path.display(),
                "imported board from JSON into database (one-shot)"
            );
        }
        let mut state = store.load_board_state().await?;
        let (healed, renamed) = Self::heal_loaded_state(&mut state);
        state.rebuild_hot_indexes();
        if healed > 0 {
            tracing::info!("healed {healed} Initial plan Task(s) Shaping → Backlog");
        }
        if renamed > 0 {
            tracing::info!("renamed {renamed} Initial plan Task(s) to include Project name");
        }
        tracing::info!(items = state.items.len(), "restored board from database");
        let mut board = Self::new(schema, json_path);
        board.store = Some(store);
        *board.state.write() = state;
        let path_migrated = board.migrate_sandbox_policies_to_inline();
        let catalog_migrated = board.migrate_inline_policies_to_catalog();
        if healed > 0 || renamed > 0 || path_migrated > 0 || catalog_migrated > 0 {
            if path_migrated > 0 {
                tracing::info!(
                    "migrated {path_migrated} sandbox profile polic(ies) from host path to inline YAML"
                );
            }
            if catalog_migrated > 0 {
                tracing::info!(
                    "migrated {catalog_migrated} inline sandbox polic(ies) into Policies catalog"
                );
            }
            board.dirty.store(true, Ordering::Relaxed);
            board.flush();
        }
        // Operator-created sandbox specs are left alone. The five
        // sandbox-<engine> rows below are the exception: shipped, one per
        // split image, so Cockpit works before an operator writes anything.
        if board.ensure_minimal_policy() {
            tracing::info!("seeded minimal OpenShell policy");
            board.flush();
        }
        if board.ensure_shipped_mcp_servers() {
            tracing::info!("seeded shipped MCP servers");
            board.flush();
        }
        if board.ensure_shipped_sandbox_profiles() {
            tracing::info!("seeded shipped per-engine sandbox profiles");
            board.flush();
        }
        if board.ensure_cockpit_sandboard_mcp_attach() {
            tracing::info!("attached shipped sandboard MCP to Cockpit sandbox profile");
            board.flush();
        }
        if board.migrate_github_app_provider_name() {
            tracing::info!("migrated GitHub App provider attach name github → github-app");
            board.flush();
        }
        if board.migrate_github_app_sealed_into_provider() {
            tracing::info!("migrated GitHub App sealed credentials onto provider github-app");
            board.flush();
        }
        if board.prune_sandbox_attach_names() {
            tracing::info!("pruned sandbox attach names that are not in Providers");
            board.flush();
        }
        if board.seed_workspace_binding_if_empty() {
            tracing::info!("seeded workspace forge binding");
            board.flush();
        }
        if board.seed_agent_runtime_if_empty() {
            tracing::info!("seeded agent runtime from compiled defaults");
            board.flush();
        }
        if crate::provider_types::ensure_shipped_on_board(&board) > 0 {
            tracing::info!("seeded shipped OpenShell provider types");
            board.flush();
        }
        Ok(board)
    }

    pub fn subscribe(&self) -> broadcast::Receiver<BoardEvent> {
        self.tx.subscribe()
    }

    /// Whether the supervisor may claim this Backlog card right now.
    ///
    /// Parked cards are never claimable until unparked (which also queues
    /// dispatch — Resume is Start).
    pub fn may_claim(&self, id: ItemId) -> bool {
        !self.state.read().items.get(&id).is_some_and(|it| it.parked)
    }

    fn next_seq(&self) -> u64 {
        self.seq.fetch_add(1, Ordering::Relaxed) + 1
    }

    pub fn current_seq(&self) -> u64 {
        self.seq.load(Ordering::Relaxed)
    }

    fn record_and_send(&self, event: BoardEvent) {
        {
            let mut buffer = self.event_buffer.write();
            if buffer.len() >= self.buffer_capacity {
                buffer.pop_front();
            }
            buffer.push_back(event.clone());
        }
        let _ = self.tx.send(event);
    }

    pub fn catch_up(&self, last_seq: u64) -> CatchUpResult {
        let current_seq = self.current_seq();
        if last_seq > current_seq {
            return CatchUpResult::Reset { seq: current_seq };
        }
        if last_seq == current_seq {
            return CatchUpResult::Events(Vec::new());
        }

        let buffer = self.event_buffer.read();
        if buffer.is_empty() {
            return CatchUpResult::Reset { seq: current_seq };
        }

        let oldest_seq = buffer.front().unwrap().seq();
        let needed_seq = last_seq + 1;

        if needed_seq < oldest_seq {
            CatchUpResult::Reset { seq: current_seq }
        } else {
            let missed: Vec<BoardEvent> = buffer
                .iter()
                .filter(|ev| ev.seq() > last_seq)
                .cloned()
                .collect();
            CatchUpResult::Events(missed)
        }
    }

    fn emit(&self, item: &WorkItem) {
        let mut item = item.clone();
        {
            let s = self.state.read();
            Self::populate_blockers(&s, &mut item);
        }
        self.record_and_send(BoardEvent::Upsert {
            seq: self.next_seq(),
            item: Box::new(item),
        });
        self.dirty.store(true, Ordering::Relaxed);
    }

    /// Flush durable state if anything changed. Called on an interval so a
    /// fleet of heartbeating agents doesn't turn into a write storm.
    ///
    /// With a [`DurableBoardStore`] attached, this writes rows (not `sandboard.json`).
    /// Without a store (unit tests), the legacy whole-file JSON path remains.
    pub fn flush(&self) {
        if !self.dirty.swap(false, Ordering::Relaxed) {
            return;
        }
        if let Some(store) = &self.store {
            let snapshot = self.state.read().clone_for_persist();
            let result = match tokio::runtime::Handle::try_current() {
                Ok(handle)
                    if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread =>
                {
                    tokio::task::block_in_place(|| {
                        handle.block_on(store.save_board_state(&snapshot))
                    })
                }
                Ok(_) | Err(_) => {
                    // current-thread runtime (tests) or no runtime: own thread so
                    // we never call block_in_place / nest block_on on CurrentThread.
                    let store = Arc::clone(store);
                    std::thread::spawn(move || {
                        let rt = tokio::runtime::Builder::new_current_thread()
                            .enable_all()
                            .build()
                            .expect("board flush runtime");
                        rt.block_on(store.save_board_state(&snapshot))
                    })
                    .join()
                    .unwrap_or_else(|_| {
                        Err(crate::db::StoreError::Query(
                            "board flush thread panicked".into(),
                        ))
                    })
                }
            };
            if let Err(e) = result {
                tracing::error!("board database flush failed: {e}");
                self.dirty.store(true, Ordering::Relaxed);
            }
            return;
        }
        let json = { serde_json::to_string_pretty(&*self.state.read()) };
        let Ok(json) = json else { return };
        let tmp = self.path.with_extension("json.tmp");
        if std::fs::write(&tmp, json).is_ok() {
            let _ = std::fs::rename(&tmp, &self.path);
        }
    }

    pub fn populate_blockers(s: &BoardState, item: &mut WorkItem) {
        item.blockers = item
            .blocked_by
            .iter()
            .filter_map(|&bid| {
                s.items.get(&bid).map(|b| BlockerSummary {
                    id: b.id,
                    title: b.title.clone(),
                    state: b.state,
                })
            })
            .collect();
    }

    // ------------------------------------------------------------ tree reads

    pub fn get(&self, id: ItemId) -> Option<WorkItem> {
        let s = self.state.read();
        s.items.get(&id).map(|i| {
            let mut item = i.clone();
            Self::populate_blockers(&s, &mut item);
            item
        })
    }

    pub fn children_of(&self, id: ItemId) -> Vec<ItemId> {
        let s = self.state.read();
        Self::children_of_indexed(&s, id)
    }

    /// Children with title/state/last transition reason — Project triage without
    /// guessing ids from a bare list.
    pub fn child_summaries(&self, id: ItemId) -> Vec<ChildSummary> {
        let s = self.state.read();
        let mut kids = Self::children_of_indexed(&s, id);
        kids.sort_unstable();
        kids.into_iter()
            .filter_map(|cid| {
                let child = s.items.get(&cid)?;
                Some(Self::child_summary_of(child))
            })
            .collect()
    }

    fn child_summary_of(item: &WorkItem) -> ChildSummary {
        let last_reason = item.history.last().and_then(|t| t.reason.clone());
        ChildSummary {
            id: item.id,
            title: item.title.clone(),
            state: format!("{:?}", item.state),
            last_reason,
        }
    }

    /// Case-insensitive substring search over title, intent, DoD, notes, history.
    /// Results prefer title hits, then intent, then the rest; capped by `limit`.
    pub fn search_items(
        &self,
        query: &str,
        goal: Option<ItemId>,
        limit: usize,
    ) -> Vec<SearchHit> {
        let q = query.trim().to_lowercase();
        if q.is_empty() || limit == 0 {
            return Vec::new();
        }
        let limit = limit.min(50);
        let s = self.state.read();

        let mut hits: Vec<(u8, SearchHit)> = Vec::new();
        for item in s.items.values() {
            if let Some(g) = goal {
                if Self::goal_of(&s, item.id) != g {
                    continue;
                }
            }
            if let Some((rank, matched_in, detail)) = Self::search_match(item, &q) {
                hits.push((
                    rank,
                    SearchHit {
                        id: item.id,
                        title: item.title.clone(),
                        state: format!("{:?}", item.state),
                        goal: Some(Self::goal_of(&s, item.id)),
                        matched_in: matched_in.into(),
                        detail,
                    },
                ));
            }
        }
        hits.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.id.cmp(&b.1.id)));
        hits.into_iter().take(limit).map(|(_, h)| h).collect()
    }

    /// Lower rank = better. Returns (rank, field name, excerpt).
    fn search_match(item: &WorkItem, q: &str) -> Option<(u8, &'static str, String)> {
        let excerpt = |s: &str| -> String {
            let flat = s.replace('\n', " ");
            if flat.len() <= 160 {
                flat
            } else {
                format!("{}…", &flat[..160])
            }
        };
        if item.title.to_lowercase().contains(q) {
            return Some((0, "title", excerpt(&item.title)));
        }
        if item.intent.to_lowercase().contains(q) {
            return Some((1, "intent", excerpt(&item.intent)));
        }
        if let Some(dod) = item.definition_of_done.as_ref() {
            if dod.to_lowercase().contains(q) {
                return Some((2, "definition_of_done", excerpt(dod)));
            }
        }
        for n in &item.notes {
            if n.text.to_lowercase().contains(q) {
                return Some((3, "notes", excerpt(&n.text)));
            }
        }
        for t in item.history.iter().rev() {
            if let Some(reason) = &t.reason {
                if reason.to_lowercase().contains(q) {
                    return Some((4, "history", excerpt(reason)));
                }
            }
        }
        None
    }

    /// Children via `children_by_parent` (not a full items scan).
    fn children_of_indexed(s: &BoardState, id: ItemId) -> Vec<ItemId> {
        s.children_by_parent
            .get(&id)
            .map(|kids| kids.iter().copied().collect())
            .unwrap_or_default()
    }

    fn recent_retired_of(members: &[&WorkItem], cap: usize) -> Vec<RetiredSnippet> {
        let mut out: Vec<RetiredSnippet> = members
            .iter()
            .filter(|i| i.state == State::Retired)
            .map(|i| {
                let retire = i.history.iter().rev().find(|t| t.to == State::Retired);
                RetiredSnippet {
                    id: i.id,
                    title: i.title.clone(),
                    reason: retire.and_then(|t| t.reason.clone()),
                    at: retire.map(|t| t.at).unwrap_or(i.entered_state_at),
                }
            })
            .collect();
        out.sort_by(|a, b| b.at.cmp(&a.at).then_with(|| a.id.cmp(&b.id)));
        out.truncate(cap);
        out
    }

    fn has_children(s: &BoardState, id: ItemId) -> bool {
        s.non_retired_child_count(id) > 0
    }

    fn depth(s: &BoardState, id: ItemId) -> usize {
        let mut depth = 0;
        let mut cur = s.items.get(&id).and_then(|i| i.parent);
        while let Some(p) = cur {
            depth += 1;
            if depth > 32 {
                break; // cycle guard
            }
            cur = s.items.get(&p).and_then(|i| i.parent);
        }
        depth
    }

    fn chain(s: &BoardState, id: ItemId) -> Vec<ItemId> {
        let mut out = Vec::new();
        let mut cur = Some(id);
        while let Some(c) = cur {
            out.push(c);
            if out.len() > 32 {
                break;
            }
            cur = s.items.get(&c).and_then(|i| i.parent);
        }
        out.reverse();
        out
    }

    /// The Project a card belongs to. Swimlanes go by Project, never by agent.
    fn goal_of(s: &BoardState, id: ItemId) -> ItemId {
        let chain = Self::chain(s, id);
        // Roots are Projects; every Task's swimlane is its Project root.
        chain.first().copied().unwrap_or(id)
    }

    fn level_name(&self, s: &BoardState, id: ItemId) -> String {
        if let Some(l) = s.items.get(&id).and_then(|i| i.level.clone()) {
            return l;
        }
        // Machine-created depth collapses into the nearest declared rung.
        self.schema
            .level_for_depth(Self::depth(s, id))
            .map(|l| l.name.clone())
            .unwrap_or_else(|| "Item".into())
    }

    /// Which goal swimlane a card belongs to.
    pub fn goal_for(&self, id: ItemId) -> ItemId {
        let s = self.state.read();
        Self::goal_of(&s, id)
    }

    pub fn ancestry(&self, id: ItemId) -> Vec<AncestryLine> {
        let s = self.state.read();
        Self::chain(&s, id)
            .into_iter()
            .filter_map(|cid| {
                s.items.get(&cid).map(|i| AncestryLine {
                    level: self.level_name(&s, cid),
                    title: i.title.clone(),
                    intent: i.intent.clone(),
                })
            })
            .collect()
    }

    pub fn append_agent_log(&self, id: ItemId, line: impl Into<String>) {
        let mut logs = self.live_agent_logs.lock();
        let ring = logs.entry(id).or_default();
        if ring.len() >= 300 {
            ring.pop_front();
        }
        ring.push_back(line.into());
    }

    pub fn get_agent_logs(&self, id: ItemId) -> Vec<String> {
        self.live_agent_logs
            .lock()
            .get(&id)
            .map(|l| l.iter().cloned().collect())
            .unwrap_or_default()
    }

    pub fn clear_agent_logs(&self, id: ItemId) {
        self.live_agent_logs.lock().remove(&id);
    }

    fn unresolved_blockers(s: &BoardState, item: &WorkItem) -> Vec<ItemId> {
        item.blocked_by
            .iter()
            .copied()
            .filter(|b| {
                s.items
                    .get(b)
                    .map(|i| !i.state.is_terminal())
                    .unwrap_or(false)
            })
            .collect()
    }

    // ------------------------------------------------------------- mutations

    /// The single write path. Everything else funnels through here so the
    /// invariants can't be routed around.
    fn transition_locked(
        s: &mut BoardState,
        id: ItemId,
        to: State,
        by: &str,
        reason: Option<String>,
    ) -> Result<WorkItem, TransitionError> {
        let has_children = Self::has_children(s, id);
        let item = s.items.get(&id).ok_or(TransitionError::NoSuchItem(id))?;
        let blockers = Self::unresolved_blockers(s, item);
        machine::check(item, to, has_children, &blockers)?;

        let now = Utc::now();
        let from = {
            let item = s.items.get_mut(&id).unwrap();
            let from = item.state;
            item.history.push(Transition {
                at: now,
                from,
                to,
                by: by.to_string(),
                reason,
            });
            item.state = to;
            if from != to {
                item.entered_state_at = now;
            }

            // States that imply no agent is holding the card.
            if matches!(
                to,
                State::Backlog | State::NeedsHuman | State::Done | State::Retired | State::Shaping
            ) {
                item.lease = None;
                item.run_deadline_at = None;
            }
            if to == State::Backlog {
                item.progress = 0.0;
                // Bounce / park / halt / deadline expiry all land here — never auto-start again.
                item.awaiting_dispatch = false;
                item.rebase_requested = false;
                if by == "human" {
                    item.run_failures = 0;
                    item.escalation = None;
                    item.last_bounce_reason = None;
                }
            }
            // Terminal cards discard the LLM session and sandbox environment.
            if to.is_terminal() {
                item.conversation_id = None;
                item.parked = false;
                item.awaiting_dispatch = false;
                item.rebase_requested = false;
                item.environment = None;
            }
            from
        };
        if from != to {
            s.index_set_state(id, from, to);
        }
        let mut item_out = s.items.get(&id).unwrap().clone();
        Self::populate_blockers(s, &mut item_out);
        Ok(item_out)
    }

    pub fn transition(
        &self,
        id: ItemId,
        to: State,
        by: &str,
        reason: Option<String>,
    ) -> Result<WorkItem, TransitionError> {
        let (item, env_to_delete) = {
            let mut s = self.state.write();
            let prev_env = s.items.get(&id).and_then(|i| i.environment.clone());
            let item = Self::transition_locked(&mut s, id, to, by, reason)?;
            let env_to_delete = if to.is_terminal() { prev_env } else { None };
            (item, env_to_delete)
        };
        self.emit(&item);

        if let Some(env) = env_to_delete {
            let os = self.openshell_client_local();
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                handle.spawn(async move {
                    let _ = os.delete(&env).await;
                });
            }
        }

        // Initial plan / split proposals become sibling Tasks when the card
        // reaches Done (Approve, or PR merge via webhook if Approve never ran).
        if to == State::Done {
            if let Err(e) = self.materialize_proposal_on_done(id, by) {
                tracing::warn!(id, error = %e, "materialize proposal on Done failed");
            }
            let unblocked = self.newly_unblocked_siblings(id);
            if unblocked.len() == 1 {
                let next = &unblocked[0];
                self.story(
                    id,
                    format!(
                        "Unblocked next sibling #{} ({}) — ready to dispatch.",
                        next.id, next.title
                    ),
                );
            } else if unblocked.len() > 1 {
                let list = unblocked
                    .iter()
                    .map(|u| format!("#{} ({})", u.id, u.title))
                    .collect::<Vec<_>>()
                    .join(", ");
                self.story(
                    id,
                    format!("Unblocked next siblings: {list} — ready to dispatch."),
                );
            }
        }

        // Re-read after Done-side materialize so callers see stamped item_ids.
        Ok(self.get(id).unwrap_or(item))
    }

    /// Create a Project (root) or a Task under a Project. Tasks are flat —
    /// nesting under another Task is refused.
    ///
    #[allow(clippy::too_many_arguments)]
    pub fn create(
        &self,
        parent: Option<ItemId>,
        title: impl Into<String>,
        intent: impl Into<String>,
        definition_of_done: Option<String>,
        origin: Origin,
        above_line: bool,
        capability: Option<String>,
    ) -> Result<WorkItem, String> {
        if let Some(pid) = parent {
            let s = self.state.read();
            let Some(p) = s.items.get(&pid) else {
                return Err(format!("no parent #{pid}"));
            };
            if p.parent.is_some() {
                return Err(
                    "tasks are flat under a Project; cannot nest under another task".into(),
                );
            }
            if p.level.as_deref() == Some("Task") {
                return Err("cannot add children to a Task; parent must be a Project".into());
            }
        }

        let item = {
            let mut s = self.state.write();
            let id = s.next_id;
            s.next_id += 1;

            let mut item = WorkItem::new(id, title, intent);
            item.parent = parent;
            item.definition_of_done = definition_of_done;
            item.origin = origin;
            item.above_line = above_line;
            item.capability = capability;
            item.level = if parent.is_none() {
                self.schema
                    .project_level()
                    .map(|l| l.name.clone())
                    .or_else(|| Some("Project".into()))
            } else {
                self.schema
                    .task_level()
                    .map(|l| l.name.clone())
                    .or_else(|| Some("Task".into()))
            };
            if parent.is_none() {
                // Plan lives on the Initial plan Task (`proposal`), not the Project.
                item.plan = None;
                // Board standing prompt lives in Agent runtime; Project prompt
                // starts empty unless the operator sets Project-only extras.
                item.project_prompt = None;
            }
            s.insert_item(item.clone());
            let mut item_out = item;
            Self::populate_blockers(&s, &mut item_out);
            item_out
        };

        self.emit(&item);

        // Projects auto-seed a claimable Initial plan Task.
        if item.is_project() {
            if let Err(e) = self.seed_initial_plan(item.id) {
                tracing::warn!(project = item.id, error = %e, "auto-seed Initial plan failed");
            }
        }
        Ok(self.get(item.id).unwrap_or(item))
    }

    /// Create a Project with a required planning clone target (`owner/name`).
    ///
    /// Stamps the repo into Project intent and into the auto-seeded Initial
    /// plan so Remotes can clone without inventing a name. Optional
    /// `project_prompt` is Project-only extras (board policy is Agent runtime).
    pub fn create_project(
        &self,
        title: impl Into<String>,
        intent: impl Into<String>,
        clone_repo: &str,
        above_line: bool,
        project_prompt: Option<String>,
    ) -> Result<WorkItem, String> {
        let clone = crate::schema::parse_owner_name(clone_repo)?;
        let stamp = crate::schema::clone_repo_prose_line(&clone);
        let intent = intent.into();
        let intent = {
            let trimmed = intent.trim();
            if crate::schema::clone_repo_from_prose(trimmed).is_some() {
                trimmed.to_string()
            } else if trimmed.is_empty() {
                stamp.clone()
            } else {
                format!("{trimmed}\n\n{stamp}")
            }
        };
        let item = self.create(None, title, intent, None, Origin::Human, above_line, None)?;
        let prompt = project_prompt
            .filter(|p| !p.trim().is_empty())
            .map(|p| p.trim().to_string());
        if let Some(prompt) = prompt {
            let _ = self.update_item(item.id, None, None, None, None, Some(prompt));
        }
        // Re-stamp Initial plan if create's auto-seed ran before prompt update —
        // seed reads clone from Project intent (already stamped).
        let _ = self.seed_initial_plan(item.id);
        Ok(self.get(item.id).unwrap_or(item))
    }

    /// Create a flat Task under an existing Project, ready for dispatch (Backlog).
    ///
    /// Parent must be a Project (nesting under a Task is refused). Origin is
    /// Human. Transitions Draft → Shaping → Backlog like `materialize_proposal`.
    /// When intent/DoD omit an explicit `Clone repository:` line, stamps the
    /// Project default from Project intent — never invents an owner/name.
    /// Optional `blocked_by` ItemIds are applied via `set_blocked_by`.
    #[allow(clippy::too_many_arguments)]
    pub fn create_task(
        &self,
        parent: ItemId,
        title: impl Into<String>,
        intent: impl Into<String>,
        definition_of_done: impl Into<String>,
        blocked_by: Vec<ItemId>,
        capability: Option<String>,
        above_line: bool,
    ) -> Result<WorkItem, String> {
        let project = self
            .get(parent)
            .ok_or_else(|| format!("no parent #{parent}"))?;
        if !project.is_project() {
            return Err(
                "tasks are flat under a Project; cannot nest under another task".into(),
            );
        }

        let dod_raw = definition_of_done.into();
        let dod = dod_raw.trim();
        if dod.is_empty() {
            return Err(
                "definition_of_done is required so the Task can enter Backlog".into(),
            );
        }
        let definition_of_done = dod.to_string();

        let intent_raw = intent.into();
        let has_clone = crate::schema::clone_repo_from_prose(&intent_raw).is_some()
            || crate::schema::clone_repo_from_prose(&definition_of_done).is_some();
        let intent = if has_clone {
            intent_raw
        } else if let Some(clone) = crate::schema::clone_repo_from_prose(&project.intent) {
            let stamp = format!("Clone repository: {clone}.");
            let trimmed = intent_raw.trim();
            if trimmed.is_empty() {
                stamp
            } else {
                format!("{stamp} {trimmed}")
            }
        } else {
            // No Project default — leave prose alone; Remotes escalate when unnamed.
            intent_raw
        };

        let item = self.create(
            Some(parent),
            title,
            intent,
            Some(definition_of_done),
            Origin::Human,
            above_line,
            capability,
        )?;
        self.transition(item.id, State::Shaping, "human", Some("create task".into()))
            .map_err(|e| e.to_string())?;
        let item = self
            .transition(item.id, State::Backlog, "human", Some("create task".into()))
            .map_err(|e| e.to_string())?;

        if !blocked_by.is_empty() {
            self.set_blocked_by(item.id, blocked_by);
        }
        Ok(self.get(item.id).unwrap_or(item))
    }

    /// Seed the Project's claimable Initial plan Task (idempotent).
    ///
    /// Clone target comes from Project intent prose (`Clone repository: …`).
    /// The planner names clone targets in each proposed task's intent/DoD and
    /// finishes with `plan.json` → Review → Approve.
    pub fn seed_initial_plan(&self, project_id: ItemId) -> Result<WorkItem, String> {
        let project = self
            .get(project_id)
            .ok_or_else(|| format!("no work item #{project_id}"))?;
        if !project.is_project() {
            return Err(format!(
                "seed_initial_plan requires a Project; card #{project_id} is not one"
            ));
        }
        if let Some(existing) = self.initial_plan_of(project_id) {
            return Ok(existing);
        }

        let project_title = project.title.clone();
        let title = crate::model::initial_plan_title(&project_title);
        let clone = crate::schema::clone_repo_from_prose(&project.intent);
        let clone_line = clone
            .as_deref()
            .map(crate::schema::clone_repo_prose_line)
            .unwrap_or_default();
        let intent = if clone_line.is_empty() {
            format!(
                "Propose sibling Tasks for «{project_title}»: write /sandbox/.sandboard/plan.json \
                 (summary + tasks with key, title, intent, definition_of_done, \
                 optional blocked_by_keys). In each task's intent and/or DoD, name the \
                 exact repository to clone (`owner/name`, and fork if cross-fork). \
                 Do **not** open a PR. Do not write split.json. Card goes to Review — \
                 human Approve creates those Tasks."
            )
        } else {
            format!(
                "{clone_line} Propose sibling Tasks for «{project_title}»: write \
                 /sandbox/.sandboard/plan.json (summary + tasks with key, title, intent, \
                 definition_of_done, optional blocked_by_keys). In each task's intent \
                 and/or DoD, name the exact repository to clone (default `{clone}` \
                 unless a task targets another repo). Do **not** open a PR. Do not \
                 write split.json. Card goes to Review — human Approve creates those Tasks.",
                clone = clone.as_deref().unwrap_or("")
            )
        };
        let dod = if let Some(ref c) = clone {
            Some(format!(
                "Write plan.json with proposed Tasks (each names clone target; default {c}). Approve creates them."
            ))
        } else {
            Some(
                "Write plan.json with proposed Tasks (each names clone target). Approve creates them."
                    .into(),
            )
        };
        let seed = self.create(
            Some(project_id),
            title.clone(),
            intent,
            dod,
            Origin::Planner,
            false,
            None,
        )?;
        let _ = self.transition(
            seed.id,
            State::Shaping,
            "operator",
            Some("init plan".into()),
        );
        let seed = self
            .transition(
                seed.id,
                State::Backlog,
                "operator",
                Some("init plan".into()),
            )
            .map_err(|e| e.to_string())?;
        self.story(project_id, format!("Seeded {title} Task #{}.", seed.id));
        Ok(seed)
    }

    /// Compatibility alias — same as [`Self::seed_initial_plan`].
    pub fn init_plan(&self, project_id: ItemId) -> Result<WorkItem, String> {
        self.seed_initial_plan(project_id)
    }

    /// Resolve a Project id or Initial plan id to the Initial plan Task id.
    pub fn resolve_initial_plan_id(&self, id: ItemId) -> Result<ItemId, String> {
        let item = self.get(id).ok_or_else(|| format!("no work item #{id}"))?;
        if item.is_initial_plan_task() {
            return Ok(id);
        }
        if item.is_project() {
            return self
                .children_of(id)
                .into_iter()
                .find(|&cid| self.get(cid).is_some_and(|c| c.is_initial_plan_task()))
                .ok_or_else(|| format!("Project #{id} has no Initial plan Task"));
        }
        Err("plan operations require a Project or Initial plan Task".into())
    }

    fn initial_plan_of(&self, project_id: ItemId) -> Option<WorkItem> {
        let s = self.state.read();
        Self::initial_plan_of_locked(&s, project_id)
    }

    fn initial_plan_of_locked(s: &BoardState, project_id: ItemId) -> Option<WorkItem> {
        s.items
            .values()
            .find(|i| i.parent == Some(project_id) && i.is_initial_plan_task())
            .cloned()
    }

    /// GoalView label from the Initial plan card (not Project.plan).
    ///
    /// Takes `&BoardState` so callers that already hold `state` (e.g. snapshot)
    /// do not re-enter `RwLock` — std's lock is not reentrant and that freezes
    /// the whole process on `/api/board`.
    fn plan_status_label(s: &BoardState, project_id: ItemId) -> String {
        let Some(seed) = Self::initial_plan_of_locked(s, project_id) else {
            return "no_plan".into();
        };
        let has_proposal = seed.proposal.as_ref().is_some_and(|p| !p.tasks.is_empty());
        if seed.state == State::Done {
            return if has_proposal {
                "approved".into()
            } else {
                // Legacy boards: Tasks exist but proposal was cleared.
                let has_impl = s.items.values().any(|c| {
                    c.parent == Some(project_id)
                        && !c.is_initial_plan_task()
                        && c.state != State::Retired
                });
                if has_impl {
                    "approved".into()
                } else {
                    "no_plan".into()
                }
            };
        }
        if has_proposal {
            "awaiting_approval".into()
        } else {
            "no_plan".into()
        }
    }

    /// Write / revise the proposal on the Initial plan card. Does not create
    /// board Tasks — Approve materializes them. `id` may be the Project or the
    /// Initial plan Task. `cancel_keys` is ignored (replan is out of scope).
    pub fn propose_plan(
        &self,
        id: ItemId,
        summary: impl Into<String>,
        tasks: Vec<PlanTaskSpec>,
        _cancel_keys: Vec<String>,
    ) -> Result<TaskProposal, String> {
        if tasks.is_empty() {
            return Err("a plan needs at least one task".into());
        }
        for t in &tasks {
            if t.definition_of_done.trim().is_empty() {
                return Err(format!(
                    "task '{}' has no definition of done; without one the board is a wish list",
                    t.title
                ));
            }
            if t.key.trim().is_empty() {
                return Err(format!("task '{}' needs a stable plan key", t.title));
            }
        }
        let seed_id = self.resolve_initial_plan_id(id)?;
        let seed = self
            .get(seed_id)
            .ok_or_else(|| format!("no work item #{seed_id}"))?;
        if seed.state.is_terminal() {
            return Err("Initial plan already accepted — proposal is frozen".into());
        }
        let proposal = TaskProposal {
            summary: summary.into(),
            tasks,
        };
        self.set_proposal(seed_id, proposal.clone())?;
        let project_id = seed.parent.unwrap_or(seed_id);
        self.story(
            project_id,
            format!(
                "Plan proposed on Initial plan ({} tasks) — awaiting Approve.",
                proposal.tasks.len()
            ),
        );
        Ok(proposal)
    }

    /// Approve the Initial plan proposal: materialize Tasks and finish the card.
    /// `id` may be the Project or the Initial plan Task.
    pub fn approve_plan(&self, id: ItemId) -> Result<Vec<ItemId>, String> {
        let seed_id = self.resolve_initial_plan_id(id)?;
        let seed = self
            .get(seed_id)
            .ok_or_else(|| format!("no work item #{seed_id}"))?;
        if seed.state.is_terminal() {
            return Err("Initial plan already accepted".into());
        }

        // Legacy boards: proposal empty but Project still holds an awaiting Plan.
        if !seed.proposal.as_ref().is_some_and(|p| !p.tasks.is_empty()) {
            if let Some(project_id) = seed.parent {
                if let Some(project) = self.get(project_id) {
                    if let Some(plan) = project.plan.as_ref().filter(|p| !p.tasks.is_empty()) {
                        self.set_proposal(
                            seed_id,
                            TaskProposal {
                                summary: plan.summary.clone(),
                                tasks: plan.tasks.clone(),
                            },
                        )?;
                        {
                            let mut s = self.state.write();
                            if let Some(p) = s.items.get_mut(&project_id) {
                                p.plan = None;
                                let snap = p.clone();
                                drop(s);
                                self.emit(&snap);
                            }
                        }
                    }
                }
            }
        }

        let seed = self
            .get(seed_id)
            .ok_or_else(|| format!("no work item #{seed_id}"))?;
        if !seed.proposal.as_ref().is_some_and(|p| !p.tasks.is_empty()) {
            return Err(
                "no proposal on Initial plan — run propose_breakdown or wait for plan.json".into(),
            );
        }

        let done = self.approve_review(seed_id)?;
        let published: Vec<ItemId> = done
            .proposal
            .as_ref()
            .map(|p| p.tasks.iter().filter_map(|t| t.item_id).collect())
            .unwrap_or_default();
        if published.is_empty() {
            return Err("Initial plan reached Done but created no Tasks".into());
        }
        Ok(published)
    }

    /// Transition board Project cards to Done when all child tasks are completed or superseded.
    pub async fn heal_completed_epics(self: &Arc<Self>) -> usize {
        let mut healed = 0usize;

        let project_ids: Vec<(ItemId, State)> = {
            let s = self.state.read();
            s.items
                .values()
                .filter(|i| {
                    i.parent.is_none() && i.state != State::Done && i.state != State::Retired
                })
                .map(|i| (i.id, i.state))
                .collect()
        };

        for (pid, state) in project_ids {
            let (child_count, all_done) = {
                let s = self.state.read();
                let children: Vec<&WorkItem> =
                    s.items.values().filter(|i| i.parent == Some(pid)).collect();
                let count = children.len();
                let done = count > 0
                    && children
                        .iter()
                        .all(|c| c.state == State::Done || c.state == State::Retired);
                (count, done)
            };

            if child_count > 0 && all_done {
                if state == State::Draft {
                    let _ = self.transition(pid, State::Shaping, "epic-hygiene", None);
                }
                let _ = self.transition(
                    pid,
                    State::Done,
                    "epic-hygiene",
                    Some("All child tasks completed".into()),
                );
                healed += 1;
            }
        }

        healed
    }

    pub fn record_run_failure(
        &self,
        id: ItemId,
        reason: &str,
        max_attempts: u32,
    ) -> Result<WorkItem, String> {
        let (failures, title, state) = {
            let mut s = self.state.write();
            let it = s.items.get_mut(&id).ok_or("no such item")?;
            it.run_failures += 1;
            (it.run_failures, it.title.clone(), it.state)
        };

        // A run that died before its first heartbeat is still Claimed, and
        // Claimed -> NeedsHuman is not a legal edge. Promote first so the
        // escalation below has somewhere to go.
        if state == State::Claimed {
            let _ = self.transition(id, State::Running, "supervisor", None);
        }

        if failures < max_attempts {
            let item = self
                .transition(
                    id,
                    State::Backlog,
                    "supervisor",
                    Some(format!("run failed: {reason}")),
                )
                .map_err(|e| e.to_string())?;
            self.story(
                id,
                format!("{title} failed to run ({failures}/{max_attempts}): {reason}"),
            );
            return Ok(item);
        }

        self.escalate(
            id,
            "supervisor",
            format!(
                "{title} failed to run {failures} times without producing any work. \
                 Last failure: {reason}"
            ),
            vec![
                EscalationOption {
                    label: "Investigate the environment".into(),
                    detail: "Repeated early failure usually means the sandbox, policy or \
                             credentials are wrong rather than the card being wrong. \
                             `openshell logs` on the kept sandbox is the place to start."
                        .into(),
                },
                EscalationOption {
                    label: "Cut scope".into(),
                    detail: "Retire the card if it is not worth the environment work it is \
                             asking for."
                        .into(),
                },
            ],
            0,
        )
    }

    /// A run produced work, so the retry budget resets. Without this a card
    /// that failed twice long ago would escalate on its next single failure.
    pub fn clear_run_failures(&self, id: ItemId) {
        let item = {
            let mut s = self.state.write();
            let Some(it) = s.items.get_mut(&id) else {
                return;
            };
            if it.run_failures == 0 {
                return;
            }
            it.run_failures = 0;
            it.clone()
        };
        self.emit(&item);
    }

    /// Backlog → Claimed so restart can adopt a sandbox that survived Ctrl-C.
    ///
    /// Older supervisors treated follower exit -1 as a card failure and bounced
    /// Running → Backlog while leaving the setsid agent alive. `environment`
    /// still names that sandbox; this re-opens the lease without a full claim
    /// briefing (adoption only needs Claimed + lease + deadline).
    pub fn reopen_for_adoption(
        &self,
        id: ItemId,
        agent_id: &str,
        timeout_secs: i64,
    ) -> Result<WorkItem, String> {
        let now = Utc::now();
        let deadline = now + Duration::seconds(timeout_secs.max(1));
        let item = {
            let mut s = self.state.write();
            let parked = s.items.get(&id).is_some_and(|it| it.parked);
            if parked {
                return Err(format!("#{id} is parked"));
            }
            let state = s.items.get(&id).map(|it| it.state);
            if state != Some(State::Backlog) {
                return Err(format!("#{id} is not Backlog (reopen needs Backlog)"));
            }
            Self::transition_locked(
                &mut s,
                id,
                State::Claimed,
                "supervisor",
                Some("reopened for sandbox re-adoption after supervisor restart".into()),
            )
            .map_err(|e| e.to_string())?;
            let it = s.items.get_mut(&id).unwrap();
            it.awaiting_dispatch = false;
            it.run_deadline_at = Some(deadline);
            it.lease = Some(Lease {
                agent_id: agent_id.to_string(),
                granted_at: now,
                last_heartbeat: now,
                expires_at: deadline,
            });
            it.clone()
        };
        self.emit(&item);
        Ok(item)
    }

    /// Which sandbox this card is running in. Written before the agent starts,
    /// so a sandboard that dies mid-run can still find the sandbox on restart.
    pub fn set_environment(&self, id: ItemId, sandbox: Option<String>) {
        let item = {
            let mut s = self.state.write();
            let Some(it) = s.items.get_mut(&id) else {
                return;
            };
            it.environment = sandbox;
            it.clone()
        };
        self.emit(&item);
    }

    /// Persist (or clear) the agy conversation id for park/resume.
    pub fn set_conversation_id(&self, id: ItemId, conversation_id: Option<String>) {
        let item = {
            let mut s = self.state.write();
            let Some(it) = s.items.get_mut(&id) else {
                return;
            };
            if it.conversation_id == conversation_id {
                return;
            }
            it.conversation_id = conversation_id;
            it.clone()
        };
        self.emit(&item);
    }

    /// Replace the card's PR list. `None` / empty clears. A single `Some` is
    /// the one-PR test helper (does not append).
    #[cfg(test)]
    pub fn set_pull_request(&self, id: ItemId, pr: Option<crate::model::PullRequest>) {
        self.set_pull_requests(id, pr.into_iter().collect());
    }

    /// Replace the card's [`crate::model::PullRequest`] list.
    pub fn set_pull_requests(&self, id: ItemId, prs: Vec<crate::model::PullRequest>) {
        let item = {
            let mut s = self.state.write();
            let Some(it) = s.items.get_mut(&id) else {
                return;
            };
            it.pull_requests = prs;
            it.legacy_pull_request = None;
            it.legacy_pr_url = None;
            it.clone()
        };
        self.emit(&item);
    }

    /// Append a PR opened mid-run. Same URL updates forge ends without
    /// replacing siblings or clearing `merged`. Does not change card state.
    pub fn report_pull_request(
        &self,
        id: ItemId,
        mut pr: crate::model::PullRequest,
    ) -> Result<WorkItem, String> {
        let url = pr
            .url_str()
            .ok_or_else(|| format!("card #{id} report_pull_request needs a PR url"))?
            .to_string();
        let needle = Self::normalize_pr_url(&url);
        if pr.reported_at.is_none() {
            pr.reported_at = Some(Utc::now());
        }
        let item = {
            let mut s = self.state.write();
            let it = s
                .items
                .get_mut(&id)
                .ok_or_else(|| format!("no such item #{id}"))?;
            if let Some(existing) = it
                .pull_requests
                .iter_mut()
                .find(|p| p.url_str().is_some_and(|u| Self::normalize_pr_url(u) == needle))
            {
                if pr.base.is_some() {
                    existing.base = pr.base;
                }
                if pr.head.is_some() {
                    existing.head = pr.head;
                }
                if existing.url.trim().is_empty() {
                    existing.url = pr.url;
                }
            } else {
                it.pull_requests.push(pr);
            }
            it.legacy_pull_request = None;
            it.legacy_pr_url = None;
            it.clone()
        };
        self.emit(&item);
        self.story(
            id,
            format!("{}: recorded PR {url} ({})", item.title, item.pull_requests.len()),
        );
        Ok(item)
    }

    /// Test helper: write the unused `WorkItem.repo` field for DB round-trips.
    /// [`Self::resolve_card_repo`] reads `pull_requests` only.
    #[cfg(test)]
    pub fn set_task_repo(&self, id: ItemId, repo: Option<RepoConfig>) -> Result<WorkItem, String> {
        let normalized = match repo {
            None => None,
            Some(r) => {
                let r = r.normalized();
                if !r.is_complete() {
                    return Err(format!(
                        "card #{id} task repo requires non-empty upstream (owner/name)"
                    ));
                }
                Some(r)
            }
        };
        let item = {
            let mut s = self.state.write();
            let it = s
                .items
                .get_mut(&id)
                .ok_or_else(|| format!("no such item #{id}"))?;
            if it.is_project() {
                return Err(format!(
                    "card #{id} is a Project — product remotes are not Project-owned"
                ));
            }
            it.repo = normalized;
            it.clone()
        };
        self.emit(&item);
        Ok(item)
    }

    /// Set or clear the first listed PR URL, preserving base/head when present.
    /// `None` clears the whole list (legacy single-PR helper).
    pub fn set_pr_url(&self, id: ItemId, url: Option<String>) {
        let url = url.map(|u| u.trim().to_string()).filter(|u| !u.is_empty());
        let next = {
            let mut cur = self.get(id).map(|i| i.pull_requests).unwrap_or_default();
            match url {
                None => Vec::new(),
                Some(u) => {
                    if let Some(pr) = cur.first_mut() {
                        pr.url = u;
                    } else {
                        cur.push(crate::model::PullRequest::from_url(u));
                    }
                    cur
                }
            }
        };
        self.set_pull_requests(id, next);
    }

    pub fn set_blocked_by(&self, id: ItemId, blockers: Vec<ItemId>) {
        let item = {
            let mut s = self.state.write();
            let Some(it) = s.items.get_mut(&id) else {
                return;
            };
            it.blocked_by = blockers;
            it.clone()
        };
        self.emit(&item);
    }

    /// Tweak an item's title, intent, definition of done, engine, or project prompt.
    /// Used from Shaping (pre-Backlog), Backlog (pre-dispatch), and Review (with
    /// Request changes) so humans can rewrite the contract the next agent sees.
    pub fn update_item(
        &self,
        id: ItemId,
        title: Option<String>,
        intent: Option<String>,
        definition_of_done: Option<String>,
        engine: Option<String>,
        project_prompt: Option<String>,
    ) -> Result<WorkItem, String> {
        let item = {
            let mut s = self.state.write();
            let it = s
                .items
                .get_mut(&id)
                .ok_or_else(|| format!("no such item #{id}"))?;
            if let Some(t) = title {
                if !t.trim().is_empty() {
                    it.title = t;
                }
            }
            if let Some(i) = intent {
                if !i.trim().is_empty() {
                    // Keep Plan summary aligned with Project Why.
                    if it.is_project() {
                        if let Some(plan) = it.plan.as_mut() {
                            plan.summary = i.trim().to_string();
                        }
                    }
                    it.intent = i;
                }
            }
            if let Some(d) = definition_of_done {
                it.definition_of_done = if d.trim().is_empty() { None } else { Some(d) };
            }
            if let Some(e) = engine {
                it.engine = if e.trim().is_empty() { None } else { Some(e) };
            }
            if let Some(p) = project_prompt {
                if it.is_project() {
                    it.project_prompt = if p.trim().is_empty() { None } else { Some(p) };
                }
            }
            it.clone()
        };
        self.emit(&item);
        Ok(item)
    }

    // ------------------------------------------------ sandbox profiles (board state)
    //
    // Operator-owned create-specs. No boot-time seed — create the first one in
    // Settings (minimal defaults). Cockpit uses the global default unless an
    // explicit cockpit profile id is set.

    /// Rename App-managed OpenShell provider `github` → `github-app`, rewrite
    /// type builtin `github` → shipped `github-app`, and rewrite sandbox
    /// `provider_names` so attaches survive the product rename.
    ///
    /// Returns true when durable state changed.
    pub fn migrate_github_app_provider_name(&self) -> bool {
        use crate::github_app::{
            LEGACY_BUILTIN_TYPE, LEGACY_PROVIDER_NAME, PROVIDER_NAME, PROVIDER_TYPE,
        };

        let changed = {
            let mut s = self.state.write();
            let mut changed = false;

            let has_new = s
                .openshell_providers
                .iter()
                .any(|p| p.name == PROVIDER_NAME);
            let legacy_idx = s.openshell_providers.iter().position(|p| {
                p.name == LEGACY_PROVIDER_NAME
                    && (p.provider_type == LEGACY_BUILTIN_TYPE
                        || p.provider_type == PROVIDER_TYPE)
            });

            if let Some(idx) = legacy_idx {
                if has_new {
                    // Prefer the already-renamed row; drop the legacy duplicate.
                    s.openshell_providers.remove(idx);
                    changed = true;
                } else {
                    s.openshell_providers[idx].name = PROVIDER_NAME.into();
                    s.openshell_providers[idx].provider_type = PROVIDER_TYPE.into();
                    changed = true;
                }
            }

            for p in &mut s.openshell_providers {
                if p.name == PROVIDER_NAME && p.provider_type == LEGACY_BUILTIN_TYPE {
                    p.provider_type = PROVIDER_TYPE.into();
                    changed = true;
                }
            }

            for profile in s.sandbox_profiles.values_mut() {
                let mut rewritten = false;
                for name in &mut profile.provider_names {
                    if name == LEGACY_PROVIDER_NAME {
                        *name = PROVIDER_NAME.into();
                        rewritten = true;
                    }
                }
                if rewritten {
                    profile.provider_names.sort();
                    profile.provider_names.dedup();
                    changed = true;
                }
            }
            changed
        };
        if changed {
            self.dirty.store(true, Ordering::Relaxed);
        }
        changed
    }

    /// Move legacy board `github_app_sealed` / `github_app_installation_id`
    /// into the `github-app` OpenShell provider row, then clear the legacy fields.
    ///
    /// Returns true when durable state changed.
    pub fn migrate_github_app_sealed_into_provider(&self) -> bool {
        use crate::github_app::{
            CONFIG_APP_ID, CONFIG_INSTALLATION_ID, CRED_CLIENT_ID, CRED_CLIENT_SECRET,
            CRED_PRIVATE_KEY, CRED_WEBHOOK_SECRET, CREDENTIAL_KEY, PROVIDER_NAME, PROVIDER_TYPE,
        };
        use crate::secrets::{open_string_map, seal_string_map};

        let legacy_sealed = self.github_app_sealed();
        let legacy_install = {
            let s = self.state.read();
            s.github_app_installation_id
        };
        if legacy_sealed.is_none() && legacy_install.is_none() {
            return false;
        }

        let bundle =
            crate::secrets::github_app_view_from_sealed(legacy_sealed.as_deref());

        let existing = self
            .openshell_providers()
            .into_iter()
            .find(|p| p.name == PROVIDER_NAME);
        let refresh = existing.as_ref().and_then(|e| e.refresh.clone());

        let mut config = existing
            .as_ref()
            .map(|e| e.config.clone())
            .unwrap_or_default();
        if let Some(ref b) = bundle {
            if !b.app_id.trim().is_empty() {
                config.insert(CONFIG_APP_ID.into(), b.app_id.trim().to_string());
            }
        }
        if let Some(id) = legacy_install.filter(|&n| n > 0) {
            config.insert(CONFIG_INSTALLATION_ID.into(), id.to_string());
        }

        let mut map = existing
            .as_ref()
            .and_then(|e| e.credentials_sealed.as_deref())
            .and_then(|s| open_string_map(s).ok())
            .unwrap_or_default();
        if let Some(b) = bundle {
            if !b.private_key_pem.trim().is_empty() {
                map.insert(CRED_PRIVATE_KEY.into(), b.private_key_pem);
            }
            if !b.webhook_secret.is_empty() {
                map.insert(CRED_WEBHOOK_SECRET.into(), b.webhook_secret);
            }
            if !b.client_id.trim().is_empty() {
                map.insert(CRED_CLIENT_ID.into(), b.client_id.trim().to_string());
            }
            if !b.client_secret.is_empty() {
                map.insert(CRED_CLIENT_SECRET.into(), b.client_secret);
            }
        }

        let credentials_sealed = if map.is_empty() {
            existing
                .as_ref()
                .and_then(|e| e.credentials_sealed.clone())
        } else {
            match seal_string_map(&map) {
                Ok(s) => Some(s),
                Err(e) => {
                    tracing::warn!(error = %e, "migrate github-app sealed into provider: seal failed");
                    return false;
                }
            }
        };
        let mut credential_keys: Vec<String> = map.keys().cloned().collect();
        if credential_keys.is_empty() {
            if let Some(ref e) = existing {
                credential_keys = e.credential_keys.clone();
            }
        }
        let _ = CREDENTIAL_KEY; // minted token key preserved when already in map
        credential_keys.sort();
        credential_keys.dedup();

        self.upsert_openshell_provider(
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

        {
            let mut s = self.state.write();
            s.github_app_sealed = None;
            s.github_app_installation_id = None;
        }
        self.dirty.store(true, Ordering::Relaxed);
        true
    }

    /// Drop attach names that are not in the Providers catalog.
    /// Keeps Sandbox specs honest — no ghost attach wishlist.
    pub fn prune_sandbox_attach_names(&self) -> bool {
        let changed = {
            let mut s = self.state.write();
            let known: BTreeSet<String> = s
                .openshell_providers
                .iter()
                .map(|p| p.name.clone())
                .collect();
            let mut changed = false;
            for profile in s.sandbox_profiles.values_mut() {
                let before = profile.provider_names.len();
                profile.provider_names.retain(|n| known.contains(n));
                if profile.provider_names.len() != before {
                    changed = true;
                }
            }
            changed
        };
        if changed {
            self.dirty.store(true, Ordering::Relaxed);
        }
        changed
    }

    // ------------------------------------------------ workspace binding (board state)

    /// Seed Forge binding when unbound. Always `github` — not from `sandboard.yaml`.
    /// Card work remotes come from `pull_requests` after publish.
    pub fn seed_workspace_binding_if_empty(&self) -> bool {
        self.seed_workspace_binding_from(&AgentConfig::default())
    }

    /// Same as [`Self::seed_workspace_binding_if_empty`] with an explicit AgentConfig
    /// (ignored; kept for call-site symmetry with other seed helpers).
    pub fn seed_workspace_binding_from(&self, _agents: &AgentConfig) -> bool {
        let mut s = self.state.write();
        if s.workspace.is_some() {
            return false;
        }
        s.workspace = Some(WorkspaceBinding {
            forge: "github".into(),
        });
        drop(s);
        self.dirty.store(true, Ordering::Relaxed);
        true
    }

    pub fn workspace_binding(&self) -> Option<WorkspaceBinding> {
        self.state.read().workspace.clone()
    }

    /// Replace the durable Forge binding. REST: `GET`/`PUT /api/workspace` (Settings → Forge).
    pub fn set_workspace_binding(
        &self,
        binding: WorkspaceBinding,
    ) -> Result<WorkspaceBinding, String> {
        let forge = binding.forge.trim();
        if forge.is_empty() {
            return Err("forge provider must not be empty".into());
        }
        if forge != "github" {
            return Err(format!(
                "forge {forge:?} is not supported yet (only github)"
            ));
        }
        let stored = WorkspaceBinding {
            forge: forge.to_string(),
        };
        {
            let mut s = self.state.write();
            s.workspace = Some(stored.clone());
        }
        self.dirty.store(true, Ordering::Relaxed);
        Ok(stored)
    }

    // ------------------------------------------------ webhook poll (board state)

    /// Settings → Forge polling fallback. Default is disabled / 60s.
    pub fn webhook_poll_config(&self) -> WebhookPollConfig {
        self.state
            .read()
            .webhook_poll
            .clone()
            .unwrap_or_default()
            .normalized()
    }

    /// Persist poll Settings. Board is SoT after save.
    pub fn set_webhook_poll_config(&self, cfg: WebhookPollConfig) -> WebhookPollConfig {
        let stored = cfg.normalized();
        {
            let mut s = self.state.write();
            s.webhook_poll = Some(stored.clone());
        }
        self.dirty.store(true, Ordering::Relaxed);
        stored
    }

    /// Last-seen default-branch tip SHA for `owner/name` (poll path).
    pub fn webhook_poll_tip(&self, repo: &str) -> Option<String> {
        self.state.read().webhook_poll_tips.get(repo).cloned()
    }

    /// Record a tip SHA after a successful poll (persisted on next flush).
    pub fn set_webhook_poll_tip(&self, repo: &str, sha: &str) {
        let repo = repo.trim().to_string();
        let sha = sha.trim().to_string();
        if repo.is_empty() || sha.is_empty() {
            return;
        }
        {
            let mut s = self.state.write();
            s.webhook_poll_tips.insert(repo, sha);
        }
        self.dirty.store(true, Ordering::Relaxed);
    }

    /// Poll cursor key for a PR: `owner/name#number`.
    pub fn webhook_poll_pr_review_key(owner_repo: &str, number: u64) -> String {
        format!("{}#{number}", owner_repo.trim())
    }

    /// Last-seen GitHub PR review id for `owner/name#number` (poll path).
    pub fn webhook_poll_pr_review_cursor(&self, owner_repo: &str, number: u64) -> Option<u64> {
        let key = Self::webhook_poll_pr_review_key(owner_repo, number);
        self.state.read().webhook_poll_pr_reviews.get(&key).copied()
    }

    /// Record the highest observed PR review id after a successful poll.
    pub fn set_webhook_poll_pr_review_cursor(&self, owner_repo: &str, number: u64, review_id: u64) {
        let key = Self::webhook_poll_pr_review_key(owner_repo, number);
        if key.starts_with('#') || owner_repo.trim().is_empty() {
            return;
        }
        {
            let mut s = self.state.write();
            s.webhook_poll_pr_reviews.insert(key, review_id);
        }
        self.dirty.store(true, Ordering::Relaxed);
    }

    // ------------------------------------------------ OpenShell connectivity (board state)

    pub fn openshell_gateway_endpoint(&self) -> Option<String> {
        self.state
            .read()
            .openshell_gateway_endpoint
            .as_ref()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }

    pub fn set_openshell_gateway_endpoint(&self, endpoint: Option<String>) -> Option<String> {
        let stored = endpoint
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        {
            let mut s = self.state.write();
            s.openshell_gateway_endpoint = stored.clone();
        }
        self.dirty.store(true, Ordering::Relaxed);
        stored
    }

    pub fn openshell_mtls_sealed(&self) -> Option<String> {
        self.state
            .read()
            .openshell_mtls_sealed
            .as_ref()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }

    /// Replace or clear the sealed mTLS blob. `None` / empty clears.
    pub fn set_openshell_mtls_sealed(&self, sealed: Option<String>) -> Option<String> {
        let stored = sealed
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        {
            let mut s = self.state.write();
            s.openshell_mtls_sealed = stored.clone();
        }
        self.dirty.store(true, Ordering::Relaxed);
        stored
    }

    pub fn openshell_mtls_status(&self) -> crate::secrets::OpenShellMtlsStatus {
        crate::secrets::mtls_status_from_sealed(self.openshell_mtls_sealed().as_deref())
    }

    /// Resolved auth mode. Legacy boards with mTLS material and no mode → `mtls`.
    pub fn openshell_auth_mode(&self) -> Option<crate::model::OpenShellAuthMode> {
        let s = self.state.read();
        if let Some(mode) = s.openshell_auth_mode {
            return Some(mode);
        }
        // Migration: existing mTLS installs did not store a mode.
        if s.openshell_mtls_sealed
            .as_ref()
            .is_some_and(|v| !v.trim().is_empty())
        {
            return Some(crate::model::OpenShellAuthMode::Mtls);
        }
        None
    }

    pub fn set_openshell_auth_mode(
        &self,
        mode: Option<crate::model::OpenShellAuthMode>,
    ) -> Option<crate::model::OpenShellAuthMode> {
        {
            let mut s = self.state.write();
            s.openshell_auth_mode = mode;
        }
        self.dirty.store(true, Ordering::Relaxed);
        mode
    }

    pub fn openshell_oidc_config(&self) -> Option<crate::model::OpenShellOidcConfig> {
        self.state.read().openshell_oidc_config.clone()
    }

    pub fn set_openshell_oidc_config(
        &self,
        cfg: Option<crate::model::OpenShellOidcConfig>,
    ) -> Option<crate::model::OpenShellOidcConfig> {
        let stored = cfg.map(|c| c.trimmed()).filter(|c| {
            !c.issuer.is_empty() || !c.client_id.is_empty() || !c.audience.is_empty()
        });
        {
            let mut s = self.state.write();
            s.openshell_oidc_config = stored.clone();
        }
        self.dirty.store(true, Ordering::Relaxed);
        stored
    }

    pub fn openshell_oidc_sealed(&self) -> Option<String> {
        self.state
            .read()
            .openshell_oidc_sealed
            .as_ref()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }

    pub fn set_openshell_oidc_sealed(&self, sealed: Option<String>) -> Option<String> {
        let stored = sealed
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        {
            let mut s = self.state.write();
            s.openshell_oidc_sealed = stored.clone();
        }
        self.dirty.store(true, Ordering::Relaxed);
        stored
    }

    pub fn openshell_oidc_status(&self) -> crate::secrets::OpenShellOidcStatus {
        crate::secrets::oidc_status_from_sealed(self.openshell_oidc_sealed().as_deref())
    }

    pub fn github_app_sealed(&self) -> Option<String> {
        self.state
            .read()
            .github_app_sealed
            .as_ref()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }

    pub fn github_app_status(&self) -> crate::secrets::GitHubAppStatus {
        crate::github_app::status_from_board(self)
    }

    /// App credentials from the `github-app` provider row (legacy board seal fallback).
    pub fn github_app_bundle(&self) -> Option<crate::secrets::GitHubAppBundle> {
        if let Some(b) = crate::github_app::bundle_from_provider(self) {
            return Some(b);
        }
        crate::secrets::github_app_view_from_sealed(self.github_app_sealed().as_deref())
    }

    pub fn github_app_installation_id(&self) -> Option<u64> {
        if let Some(id) = crate::github_app::installation_id_from_provider(self) {
            return Some(id);
        }
        self.state.read().github_app_installation_id
    }

    pub fn set_github_app_installation_id(&self, id: Option<u64>) {
        use crate::github_app::{CONFIG_INSTALLATION_ID, PROVIDER_NAME, PROVIDER_TYPE};
        let existing = self
            .openshell_providers()
            .into_iter()
            .find(|p| p.name == PROVIDER_NAME);
        let mut config = existing
            .as_ref()
            .map(|e| e.config.clone())
            .unwrap_or_default();
        match id.filter(|&n| n > 0) {
            Some(n) => {
                config.insert(CONFIG_INSTALLATION_ID.into(), n.to_string());
            }
            None => {
                config.remove(CONFIG_INSTALLATION_ID);
            }
        }
        self.upsert_openshell_provider(
            OpenShellProviderDesired {
                name: PROVIDER_NAME.into(),
                provider_type: existing
                    .as_ref()
                    .map(|e| e.provider_type.clone())
                    .unwrap_or_else(|| PROVIDER_TYPE.into()),
                config,
                credentials_sealed: existing.as_ref().and_then(|e| e.credentials_sealed.clone()),
                credential_keys: existing
                    .as_ref()
                    .map(|e| e.credential_keys.clone())
                    .unwrap_or_default(),
                refresh: existing.and_then(|e| e.refresh),
            }
            .normalized(),
        );
        // Clear legacy field if still set.
        {
            let mut s = self.state.write();
            if s.github_app_installation_id.is_some() {
                s.github_app_installation_id = None;
            }
        }
        // Force remint on next ensure.
        *self.github_app_token_cache.lock() = crate::github_app::TokenCache::default();
    }

    /// Write App bundle fields onto the `github-app` provider sealed map + config.
    pub fn set_github_app_bundle(
        &self,
        bundle: &crate::secrets::GitHubAppBundle,
    ) -> Result<(), String> {
        use crate::github_app::{
            CONFIG_APP_ID, CRED_CLIENT_ID, CRED_CLIENT_SECRET, CRED_PRIVATE_KEY,
            CRED_WEBHOOK_SECRET, PROVIDER_NAME, PROVIDER_TYPE,
        };
        use crate::secrets::{open_string_map, seal_string_map};

        bundle
            .validate_for_seal()
            .map_err(|e| format!("GitHub App: {e}"))?;

        let existing = self
            .openshell_providers()
            .into_iter()
            .find(|p| p.name == PROVIDER_NAME);
        let mut config = existing
            .as_ref()
            .map(|e| e.config.clone())
            .unwrap_or_default();
        config.insert(CONFIG_APP_ID.into(), bundle.app_id.trim().to_string());

        let mut map = existing
            .as_ref()
            .and_then(|e| e.credentials_sealed.as_deref())
            .and_then(|s| open_string_map(s).ok())
            .unwrap_or_default();
        map.insert(CRED_PRIVATE_KEY.into(), bundle.private_key_pem.clone());
        if bundle.webhook_secret.is_empty() {
            map.remove(CRED_WEBHOOK_SECRET);
        } else {
            map.insert(CRED_WEBHOOK_SECRET.into(), bundle.webhook_secret.clone());
        }
        if bundle.client_id.trim().is_empty() {
            map.remove(CRED_CLIENT_ID);
        } else {
            map.insert(CRED_CLIENT_ID.into(), bundle.client_id.trim().to_string());
        }
        if bundle.client_secret.is_empty() {
            map.remove(CRED_CLIENT_SECRET);
        } else {
            map.insert(CRED_CLIENT_SECRET.into(), bundle.client_secret.clone());
        }

        let credential_keys: Vec<String> = map.keys().cloned().collect();
        let credentials_sealed = Some(
            seal_string_map(&map).map_err(|e| format!("seal GitHub App credentials: {e}"))?,
        );

        self.upsert_openshell_provider(
            OpenShellProviderDesired {
                name: PROVIDER_NAME.into(),
                provider_type: PROVIDER_TYPE.into(),
                config,
                credentials_sealed,
                credential_keys,
                refresh: existing.and_then(|e| e.refresh),
            }
            .normalized(),
        );
        // Drop legacy board seal once material lives on the provider.
        if self.state.read().github_app_sealed.is_some() {
            let mut s = self.state.write();
            s.github_app_sealed = None;
            self.dirty.store(true, Ordering::Relaxed);
        }
        Ok(())
    }

    /// Clear App material from the provider row (and legacy board fields).
    pub fn clear_github_app(&self) {
        use crate::github_app::{
            CONFIG_APP_ID, CONFIG_INSTALLATION_ID, CRED_CLIENT_ID, CRED_CLIENT_SECRET,
            CRED_PRIVATE_KEY, CRED_WEBHOOK_SECRET, CREDENTIAL_KEY, PROVIDER_NAME,
        };
        use crate::secrets::{open_string_map, seal_string_map};

        if let Some(existing) = self
            .openshell_providers()
            .into_iter()
            .find(|p| p.name == PROVIDER_NAME)
        {
            let mut config = existing.config;
            config.remove(CONFIG_APP_ID);
            config.remove(CONFIG_INSTALLATION_ID);
            let mut map = existing
                .credentials_sealed
                .as_deref()
                .and_then(|s| open_string_map(s).ok())
                .unwrap_or_default();
            for k in [
                CRED_PRIVATE_KEY,
                CRED_WEBHOOK_SECRET,
                CRED_CLIENT_ID,
                CRED_CLIENT_SECRET,
                CREDENTIAL_KEY,
                "GITHUB_TOKEN",
            ] {
                map.remove(k);
            }
            let credential_keys: Vec<String> = map.keys().cloned().collect();
            let credentials_sealed = if map.is_empty() {
                None
            } else {
                seal_string_map(&map).ok()
            };
            self.upsert_openshell_provider(
                OpenShellProviderDesired {
                    name: PROVIDER_NAME.into(),
                    provider_type: existing.provider_type,
                    config,
                    credentials_sealed,
                    credential_keys,
                    refresh: existing.refresh,
                }
                .normalized(),
            );
        }
        {
            let mut s = self.state.write();
            s.github_app_sealed = None;
            s.github_app_installation_id = None;
        }
        self.dirty.store(true, Ordering::Relaxed);
        *self.github_app_token_cache.lock() = crate::github_app::TokenCache::default();
    }

    pub fn github_app_token_cache(&self) -> crate::github_app::TokenCache {
        self.github_app_token_cache.lock().clone()
    }

    pub fn set_github_app_token_cache(&self, cache: crate::github_app::TokenCache) {
        *self.github_app_token_cache.lock() = cache;
    }

    /// Durable GitHub App repo-access cache (`owner/repo` → installation).
    pub fn github_repo_access_cache(&self) -> crate::github_app::GitHubRepoAccessCache {
        self.state.read().github_repo_access.clone()
    }

    pub fn set_github_repo_access_cache(&self, cache: crate::github_app::GitHubRepoAccessCache) {
        {
            let mut s = self.state.write();
            s.github_repo_access = cache;
        }
        self.dirty.store(true, Ordering::Relaxed);
    }

    pub fn auth_sealed(&self) -> Option<String> {
        self.state
            .read()
            .auth_sealed
            .as_ref()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }

    pub fn set_auth_sealed(&self, sealed: Option<String>) -> Option<String> {
        let stored = sealed
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        {
            let mut s = self.state.write();
            s.auth_sealed = stored.clone();
        }
        self.dirty.store(true, Ordering::Relaxed);
        stored
    }

    pub fn auth_bundle(&self) -> Option<crate::secrets::AuthBundle> {
        crate::secrets::auth_from_sealed(self.auth_sealed().as_deref())
    }

    pub fn auth_allowed_users(&self) -> Vec<String> {
        self.state.read().auth_allowed_users.clone()
    }

    pub fn auth_allowed_teams(&self) -> Vec<String> {
        self.state.read().auth_allowed_teams.clone()
    }

    pub fn set_auth_allowlists(&self, users: Vec<String>, teams: Vec<String>) {
        let users: Vec<String> = users
            .into_iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        let teams: Vec<String> = teams
            .into_iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        {
            let mut s = self.state.write();
            s.auth_allowed_users = users;
            s.auth_allowed_teams = teams;
        }
        self.dirty.store(true, Ordering::Relaxed);
    }

    /// Desired OpenShell providers (credentials sealed).
    pub fn openshell_providers(&self) -> Vec<OpenShellProviderDesired> {
        self.state.read().openshell_providers.clone()
    }

    /// Replace the full desired-provider list.
    #[allow(dead_code)]
    pub fn set_openshell_providers(&self, providers: Vec<OpenShellProviderDesired>) {
        let stored: Vec<_> = providers.into_iter().map(|p| p.normalized()).collect();
        {
            let mut s = self.state.write();
            s.openshell_providers = stored;
        }
        self.dirty.store(true, Ordering::Relaxed);
    }

    /// Upsert one desired provider by name.
    pub fn upsert_openshell_provider(
        &self,
        provider: OpenShellProviderDesired,
    ) -> OpenShellProviderDesired {
        let stored = provider.normalized();
        {
            let mut s = self.state.write();
            if let Some(slot) = s
                .openshell_providers
                .iter_mut()
                .find(|p| p.name == stored.name)
            {
                *slot = stored.clone();
            } else {
                s.openshell_providers.push(stored.clone());
            }
        }
        self.dirty.store(true, Ordering::Relaxed);
        stored
    }

    /// Remove a desired provider by name. Returns true when something was removed.
    pub fn delete_openshell_provider(&self, name: &str) -> bool {
        let name = name.trim();
        let removed = {
            let mut s = self.state.write();
            let before = s.openshell_providers.len();
            s.openshell_providers.retain(|p| p.name != name);
            s.openshell_providers.len() != before
        };
        if removed {
            self.dirty.store(true, Ordering::Relaxed);
        }
        removed
    }

    /// Board-owned custom OpenShell provider type profiles.
    pub fn openshell_provider_types(&self) -> BTreeMap<String, OpenShellProviderTypeDesired> {
        self.state.read().openshell_provider_types.clone()
    }

    /// Upsert a board provider type. Clears tombstone for this id.
    pub fn upsert_openshell_provider_type(
        &self,
        entry: OpenShellProviderTypeDesired,
    ) -> Result<OpenShellProviderTypeDesired, String> {
        let stored = entry.normalized();
        if stored.id.is_empty() {
            return Err("provider type id required".into());
        }
        if stored.yaml.is_empty() {
            return Err("provider type yaml required".into());
        }
        {
            let mut s = self.state.write();
            s.openshell_provider_type_tombstones.remove(&stored.id);
            s.openshell_provider_types
                .insert(stored.id.clone(), stored.clone());
        }
        self.dirty.store(true, Ordering::Relaxed);
        Ok(stored)
    }

    /// Delete a board provider type. Shipped ids are tombstoned so ensure won't resurrect.
    pub fn delete_openshell_provider_type(&self, id: &str) -> bool {
        let id = id.trim();
        if id.is_empty() {
            return false;
        }
        let removed = {
            let mut s = self.state.write();
            let had = s.openshell_provider_types.remove(id).is_some();
            if had {
                s.openshell_provider_type_tombstones.insert(id.to_string());
            }
            had
        };
        if removed {
            self.dirty.store(true, Ordering::Relaxed);
        }
        removed
    }

    /// Insert shipped provider types that are missing and not tombstoned.
    /// Returns how many entries were added.
    pub fn ensure_shipped_provider_types(
        &self,
        shipped: Vec<OpenShellProviderTypeDesired>,
    ) -> usize {
        let mut added = 0;
        {
            let mut s = self.state.write();
            for entry in shipped {
                let entry = entry.normalized();
                if entry.id.is_empty() {
                    continue;
                }
                if s.openshell_provider_type_tombstones.contains(&entry.id) {
                    continue;
                }
                if s.openshell_provider_types.contains_key(&entry.id) {
                    continue;
                }
                s.openshell_provider_types
                    .insert(entry.id.clone(), entry);
                added += 1;
            }
        }
        if added > 0 {
            self.dirty.store(true, Ordering::Relaxed);
        }
        added
    }

    /// Providers to attach for a resolved create-spec.
    ///
    /// Uses the profile's `provider_names` (empty = attach none). Unknown names
    /// are dropped; order follows the profile list.
    pub fn attach_providers_for_resolved(&self, resolved: &ResolvedSandboxCreate) -> Vec<String> {
        let s = self.state.read();
        let known: std::collections::BTreeSet<&str> = s
            .openshell_providers
            .iter()
            .map(|p| p.name.as_str())
            .collect();
        resolved
            .providers
            .iter()
            .map(|n| n.trim().to_string())
            .filter(|n| !n.is_empty() && known.contains(n.as_str()))
            .collect()
    }

    /// Client using Settings gateway endpoint + selected auth (or an injected mock).
    ///
    /// Prefer [`Self::openshell_client`] on a `SharedBoard` (`Arc<Board>`) so OIDC
    /// refresh can re-seal rotated refresh tokens. This `&self` path is for
    /// in-board helpers (sandbox delete on halt) where persist is optional.
    pub fn openshell_client_local(&self) -> crate::openshell::OpenShell {
        self.openshell_client_with_persist(None)
    }

    /// Gateway client that re-seals OIDC tokens after refresh (needs `Arc`).
    pub fn openshell_client(self: &std::sync::Arc<Self>) -> crate::openshell::OpenShell {
        let board = std::sync::Arc::clone(self);
        self.openshell_client_with_persist(Some(std::sync::Arc::new(move |next| {
            if let Ok(sealed) = crate::secrets::seal_oidc(&next) {
                board.set_openshell_oidc_sealed(Some(sealed));
            }
        })))
    }

    fn openshell_client_with_persist(
        &self,
        on_refresh: Option<
            std::sync::Arc<dyn Fn(crate::secrets::OpenShellOidcBundle) + Send + Sync>,
        >,
    ) -> crate::openshell::OpenShell {
        if let Some(os) = &self.openshell {
            return os.clone();
        }
        let endpoint = self.openshell_gateway_endpoint();
        let auth = match self.openshell_auth_mode() {
            Some(crate::model::OpenShellAuthMode::Mtls) => {
                let mtls = self
                    .openshell_mtls_sealed()
                    .as_deref()
                    .and_then(|s| crate::secrets::open_mtls(s).ok())
                    .unwrap_or(crate::secrets::OpenShellMtlsBundle {
                        ca_pem: String::new(),
                        client_cert_pem: String::new(),
                        client_key_pem: String::new(),
                    });
                Some(crate::openshell::GatewayAuth::Mtls(mtls))
            }
            Some(crate::model::OpenShellAuthMode::Oidc) => {
                let cfg = self.openshell_oidc_config().unwrap_or_default();
                let server_ca_pem = self
                    .openshell_mtls_sealed()
                    .as_deref()
                    .and_then(|s| crate::secrets::open_mtls(s).ok())
                    .map(|b| b.ca_pem)
                    .filter(|s| !s.trim().is_empty());
                let tokens = self
                    .openshell_oidc_sealed()
                    .as_deref()
                    .and_then(|s| crate::secrets::open_oidc(s).ok());
                match tokens {
                    Some(bundle) => Some(crate::openshell::GatewayAuth::oidc(
                        cfg,
                        bundle,
                        on_refresh,
                        server_ca_pem,
                    )),
                    None => Some(crate::openshell::GatewayAuth::OidcIncomplete { config: cfg }),
                }
            }
            None => None,
        };
        crate::openshell::OpenShell::new(endpoint, auth, std::time::Duration::from_secs(120))
    }

    // ------------------------------------------------ agent runtime (board state)

    /// Seed Agent runtime when unset from compiled [`AgentRuntimeConfig::default`].
    /// Settings/API edit thereafter.
    pub fn seed_agent_runtime_if_empty(&self) -> bool {
        self.seed_agent_runtime_config(AgentRuntimeConfig::default())
    }

    /// Same as [`Self::seed_agent_runtime_if_empty`] but map knobs from an
    /// explicit AgentConfig (tests).
    #[cfg(test)]
    pub fn seed_agent_runtime_from(&self, agents: &AgentConfig) -> bool {
        self.seed_agent_runtime_config(AgentRuntimeConfig {
            engine: agents.engine.clone(),
            max_concurrent: agents.max_concurrent,
            agent_timeout_secs: agents.agent_timeout_secs,
            max_attempts: agents.max_attempts,
            ..AgentRuntimeConfig::default()
        })
    }

    fn seed_agent_runtime_config(&self, runtime: AgentRuntimeConfig) -> bool {
        let mut s = self.state.write();
        if s.agent_runtime.is_some() {
            return false;
        }
        s.agent_runtime = Some(runtime.normalized());
        drop(s);
        self.dirty.store(true, Ordering::Relaxed);
        true
    }

    /// Durable Agent runtime (None until seeded or set via Settings).
    pub fn agent_runtime(&self) -> Option<AgentRuntimeConfig> {
        self.state.read().agent_runtime.clone()
    }

    /// Persist Agent runtime from Settings. Board is SoT after save.
    pub fn set_agent_runtime(&self, runtime: AgentRuntimeConfig) -> AgentRuntimeConfig {
        let stored = runtime.normalized();
        {
            let mut s = self.state.write();
            s.agent_runtime = Some(stored.clone());
        }
        self.dirty.store(true, Ordering::Relaxed);
        stored
    }

    /// AgentConfig for supervisor / sandbox create: durable Settings overlay on
    /// compiled create-knob defaults. Image/policy/cpu/memory live on board
    /// sandbox specs; optional legacy `repo` may still come from schema agents.
    pub fn effective_agents(&self) -> AgentConfig {
        self.agents_with_workspace(&self.schema.execution.agents)
    }

    /// Yaml `execution.agents.repo` when upstream is set.
    /// Card work remotes come from [`crate::model::PullRequest`] when present.
    /// Kept as a board read API (tests + future status overlays); not on the
    /// live supervisor path today.
    #[allow(dead_code)]
    pub fn yaml_work_repo(&self) -> Option<RepoConfig> {
        let yaml = &self.schema.execution.agents.repo;
        if yaml.is_complete() {
            Some(yaml.clone().normalized())
        } else {
            None
        }
    }

    /// Compiled create-knob defaults with durable Settings → Agent runtime overlay.
    /// Legacy `repo` may still come from schema agents. Remotes for a run still
    /// come from [`Self::resolve_card_repo`].
    pub fn agents_with_workspace(&self, yaml_agents: &AgentConfig) -> AgentConfig {
        let rt = self.agent_runtime();
        Self::overlay_agent_runtime(yaml_agents, rt.as_ref())
    }

    /// Pure overlay — safe to call while already holding `state` (RwLock is
    /// not reentrant; [`Self::snapshot`] must not call [`Self::agent_runtime`]).
    fn overlay_agent_runtime(
        yaml_agents: &AgentConfig,
        rt: Option<&AgentRuntimeConfig>,
    ) -> AgentConfig {
        let mut cfg = AgentConfig {
            // Legacy remotes from schema; create knobs stay compiled defaults.
            repo: yaml_agents.repo.clone(),
            ..AgentConfig::default()
        };
        let Some(rt) = rt else {
            return cfg;
        };
        cfg.engine = rt.engine.clone();
        cfg.max_concurrent = rt.max_concurrent;
        cfg.agent_timeout_secs = rt.agent_timeout_secs;
        cfg.max_attempts = rt.max_attempts;
        cfg
    }

    /// Per-card work remotes for clone / push / rebase / PR-lookup.
    ///
    /// Resolution order:
    /// 1. oldest unmerged `pull_requests` entry's base/head (or URL-only stub)
    /// 2. else first listed PR
    /// 3. else `Ok(None)` — unbound; briefing tells the agent to clone from
    ///    card intent/DoD/notes or escalate
    ///
    /// `Err` only for a malformed PR url. Does not invent remotes
    /// from yaml or Settings when the card has no `pull_requests`.
    pub fn resolve_card_repo(&self, item_id: ItemId) -> Result<Option<RepoConfig>, String> {
        let item = self
            .get(item_id)
            .ok_or_else(|| format!("no such item #{item_id}"))?;

        let pr = item
            .unmerged_prs()
            .next()
            .or_else(|| item.pull_requests.first());
        if let Some(pr) = pr {
            if let Some(repo) = pr.to_repo_config() {
                return Ok(Some(repo));
            }
            if let Some(url) = pr.url_str() {
                if let Some((upstream, _)) = parse_github_pr_url(url) {
                    return Ok(Some(
                        RepoConfig {
                            upstream: upstream.clone(),
                            fork: upstream,
                            base: "main".into(),
                        }
                        .normalized(),
                    ));
                }
                return Err(format!(
                    "card #{item_id} pull_request.url is not a parseable GitHub pull URL: {url}"
                ));
            }
        }

        // Unbound until pull_requests exist — briefing uses card prose / escalate.
        Ok(None)
    }

    /// Upgrade catalog entries that still store a host path as legacy `policy`.
    /// Returns how many profiles were rewritten (bytes moved into the legacy
    /// inline field for the subsequent catalog migration).
    pub fn migrate_sandbox_policies_to_inline(&self) -> usize {
        let mut s = self.state.write();
        let mut n = 0usize;
        for profile in s.sandbox_profiles.values_mut() {
            let Some(legacy) = profile.policy_inline_legacy.as_deref() else {
                continue;
            };
            if let Some(content) = migrate_profile_policy_to_inline(legacy) {
                profile.policy_inline_legacy = Some(content);
                n += 1;
            }
        }
        if n > 0 {
            drop(s);
            self.dirty.store(true, Ordering::Relaxed);
        }
        n
    }

    /// One-shot: move each profile's legacy inline `policy` YAML into the
    /// Policies catalog and set `policy_id`. Preserves YAML bytes exactly.
    /// Returns how many profiles were rewritten.
    pub fn migrate_inline_policies_to_catalog(&self) -> usize {
        let mut s = self.state.write();
        let mut n = 0usize;
        // Collect (profile_id, yaml) first so we can allocate unique policy ids
        // without holding conflicting borrows.
        let pending: Vec<(String, String, String)> = s
            .sandbox_profiles
            .values()
            .filter(|p| p.policy_id.trim().is_empty())
            .filter_map(|p| {
                let yaml = p.policy_inline_legacy.clone()?;
                if yaml.trim().is_empty() {
                    return None;
                }
                Some((p.id.clone(), p.name.clone(), yaml))
            })
            .collect();
        for (profile_id, profile_name, yaml) in pending {
            let base = format!("{profile_id}-policy");
            let policy_id =
                Self::allocate_unique_openshell_policy_id(&s.openshell_policies, &base);
            let name = {
                let trimmed = profile_name.trim();
                if trimmed.is_empty() {
                    policy_id.clone()
                } else {
                    format!("{trimmed} policy")
                }
            };
            s.openshell_policies.insert(
                policy_id.clone(),
                OpenShellPolicy {
                    id: policy_id.clone(),
                    name,
                    // Exact bytes from the profile — do not trim or rewrite.
                    yaml,
                },
            );
            if let Some(profile) = s.sandbox_profiles.get_mut(&profile_id) {
                profile.policy_id = policy_id;
                profile.policy_inline_legacy = None;
                n += 1;
            }
        }
        if n > 0 {
            drop(s);
            self.dirty.store(true, Ordering::Relaxed);
        }
        n
    }

    /// Seed the minimal Policies catalog row when missing (create-form default).
    /// Does not overwrite an operator-edited `minimal` entry.
    pub fn ensure_minimal_policy(&self) -> bool {
        let mut s = self.state.write();
        let id = crate::seed_policies::MINIMAL_POLICY_ID;
        if s.openshell_policies.contains_key(id) {
            return false;
        }
        s.openshell_policies.insert(
            id.into(),
            OpenShellPolicy {
                id: id.into(),
                name: crate::seed_policies::MINIMAL_POLICY_NAME.into(),
                yaml: crate::seed_policies::MINIMAL_SANDBOX_POLICY.to_string(),
            },
        );
        drop(s);
        self.dirty.store(true, Ordering::Relaxed);
        true
    }

    /// Seed / migrate the shipped `sandboard` MCP catalog row.
    ///
    /// Inserts when missing. When the existing row is still `shipped` but not
    /// the current stdio placeholder (legacy HTTP + CockpitBearer), rewrites
    /// it — operator-edited non-shipped entries are left alone.
    pub fn ensure_shipped_mcp_servers(&self) -> bool {
        let mut s = self.state.write();
        let mut changed = false;
        let sandboard = McpServerDesired::shipped_sandboard();
        match s.mcp_servers.get(&sandboard.id) {
            None => {
                s.mcp_servers.insert(sandboard.id.clone(), sandboard);
                changed = true;
            }
            Some(existing) if existing.shipped => {
                let is_stdio_placeholder = matches!(
                    &existing.transport,
                    McpTransport::Stdio { command, args, cwd: None }
                        if command.is_empty() && args.is_empty()
                );
                if !is_stdio_placeholder {
                    s.mcp_servers.insert(sandboard.id.clone(), sandboard);
                    changed = true;
                }
            }
            _ => {}
        }
        drop(s);
        if changed {
            self.dirty.store(true, Ordering::Relaxed);
        }
        changed
    }

    /// Attach shipped `sandboard` to the Cockpit sandbox profile's `mcp_server_ids`.
    ///
    /// Uses `cockpit_sandbox_profile_id`, else the global default profile (same
    /// resolve order as [`Self::resolve_cockpit_sandbox_create`]). Idempotent.
    /// Matches inject: cockpit always gets the host operator MCP seat.
    pub fn ensure_cockpit_sandboard_mcp_attach(&self) -> bool {
        self.ensure_shipped_mcp_servers();
        let mut s = self.state.write();
        if !s.mcp_servers.contains_key(SANDBOARD_MCP_SERVER_ID) {
            return false;
        }
        let profile_id = s
            .cockpit_sandbox_profile_id
            .clone()
            .or_else(|| s.default_sandbox_profile_id.clone());
        let Some(pid) = profile_id else {
            return false;
        };
        let Some(p) = s.sandbox_profiles.get_mut(&pid) else {
            return false;
        };
        if p.mcp_server_ids.iter().any(|id| id == SANDBOARD_MCP_SERVER_ID) {
            return false;
        }
        p.mcp_server_ids.insert(0, SANDBOARD_MCP_SERVER_ID.into());
        drop(s);
        self.dirty.store(true, Ordering::Relaxed);
        true
    }

    /// Seed the five per-engine Cockpit policy rows (`cockpit-cursor`,
    /// `cockpit-agy`, `cockpit-claude`, `cockpit-opencode`, `cockpit-hermes`) matching the
    /// split `sandbox-<engine>` images in `sandbox/Containerfile`. Insert
    /// only — an operator who has already edited one of these ids keeps
    /// their edit; this never overwrites.
    fn ensure_shipped_cockpit_policies(&self) -> bool {
        use crate::seed_policies::*;
        let rows: [(&str, &str, &str); 6] = [
            (COCKPIT_CURSOR_POLICY_ID, COCKPIT_CURSOR_POLICY_NAME, COCKPIT_CURSOR_POLICY),
            (COCKPIT_AGY_POLICY_ID, COCKPIT_AGY_POLICY_NAME, COCKPIT_AGY_POLICY),
            (COCKPIT_CLAUDE_POLICY_ID, COCKPIT_CLAUDE_POLICY_NAME, COCKPIT_CLAUDE_POLICY),
            (COCKPIT_OPENCODE_POLICY_ID, COCKPIT_OPENCODE_POLICY_NAME, COCKPIT_OPENCODE_POLICY),
            (COCKPIT_HERMES_POLICY_ID, COCKPIT_HERMES_POLICY_NAME, COCKPIT_HERMES_POLICY),
            (COCKPIT_PI_POLICY_ID, COCKPIT_PI_POLICY_NAME, COCKPIT_PI_POLICY),
        ];
        let mut s = self.state.write();
        let mut changed = false;
        for (id, name, yaml) in rows {
            if s.openshell_policies.contains_key(id) {
                continue;
            }
            s.openshell_policies.insert(
                id.into(),
                OpenShellPolicy {
                    id: id.into(),
                    name: name.into(),
                    yaml: yaml.into(),
                },
            );
            changed = true;
        }
        drop(s);
        if changed {
            self.dirty.store(true, Ordering::Relaxed);
        }
        changed
    }

    /// Seed one `SandboxProfile` per split `sandbox-<engine>` image (cursor,
    /// agy, claude, opencode, hermes) so Cockpit and card workers have something to
    /// pick from without an operator having to hand-write a profile + policy
    /// first. Insert only, by stable id — an operator-edited row (even one
    /// that started `shipped`) is left alone. Deliberately does **not** pick
    /// one as the default: which engine an operator wants is a real decision
    /// (see the Welcome "Sandbox spec" readiness check), not something sandboard
    /// should guess on a fresh board.
    pub fn ensure_shipped_sandbox_profiles(&self) -> bool {
        self.ensure_shipped_mcp_servers();
        self.ensure_shipped_cockpit_policies();
        use crate::seed_policies::*;
        let rows: [(&str, &str, &str, &str, &str); 6] = [
            ("sandbox-cursor", "Sandbox (cursor)", "quay.io/sandboard-app/sandbox-cursor:latest", COCKPIT_CURSOR_POLICY_ID, "cursor"),
            ("sandbox-agy", "Sandbox (agy)", "quay.io/sandboard-app/sandbox-agy:latest", COCKPIT_AGY_POLICY_ID, "agy"),
            ("sandbox-claude", "Sandbox (claude)", "quay.io/sandboard-app/sandbox-claude:latest", COCKPIT_CLAUDE_POLICY_ID, "claude"),
            ("sandbox-opencode", "Sandbox (opencode)", "quay.io/sandboard-app/sandbox-opencode:latest", COCKPIT_OPENCODE_POLICY_ID, "opencode"),
            ("sandbox-hermes", "Sandbox (hermes)", "quay.io/sandboard-app/sandbox-hermes:latest", COCKPIT_HERMES_POLICY_ID, "hermes"),
            ("sandbox-pi", "Sandbox (pi)", "quay.io/sandboard-app/sandbox-pi:latest", COCKPIT_PI_POLICY_ID, "pi"),
        ];
        let mut s = self.state.write();
        let mut changed = false;
        for (id, name, image, policy_id, engine) in rows {
            if s.sandbox_profiles.contains_key(id) {
                continue;
            }
            s.sandbox_profiles.insert(
                id.into(),
                SandboxProfile {
                    id: id.into(),
                    name: name.into(),
                    image: image.into(),
                    policy_id: policy_id.into(),
                    policy_inline_legacy: None,
                    cpu: None,
                    memory: None,
                    engine: Some(engine.into()),
                    model: None,
                    provider_names: Vec::new(),
                    mcp_server_ids: vec![SANDBOARD_MCP_SERVER_ID.into()],
                    env: BTreeMap::new(),
                    prompt: None,
                    shipped: true,
                },
            );
            changed = true;
        }
        // No auto-default here: an operator picks one via Settings → Sandbox
        // specs (surfaced as a Welcome readiness check until they do).
        drop(s);
        if changed {
            self.dirty.store(true, Ordering::Relaxed);
        }
        changed
    }

    pub fn list_mcp_servers(&self) -> Vec<McpServerDesired> {
        self.ensure_shipped_mcp_servers();
        self.state.read().mcp_servers.values().cloned().collect()
    }

    pub fn get_mcp_server(&self, id: &str) -> Option<McpServerDesired> {
        self.ensure_shipped_mcp_servers();
        self.state.read().mcp_servers.get(id).cloned()
    }

    /// Insert or replace an MCP server catalog entry. Empty `id` means create:
    /// slug from `name` with `-2`, `-3`, … on collision.
    pub fn upsert_mcp_server(
        &self,
        server: McpServerDesired,
    ) -> Result<McpServerDesired, String> {
        let mut server = server.normalized()?;
        if let Some(ref frag) = server.policy_fragment_yaml {
            // Validate fragment parses / merges against a trivial base.
            crate::mcp_policy::merge_policy_fragments("version: 1\nnetwork_policies: {}\n", [frag])
                .map_err(|e| format!("policy_fragment_yaml: {e}"))?;
        }
        let mut s = self.state.write();
        let id = {
            let trimmed = server.id.trim();
            if trimmed.is_empty() {
                let base = crate::model::slugify_sandbox_profile_id(&server.name);
                Self::allocate_unique_mcp_server_id(&s.mcp_servers, &base)
            } else {
                trimmed.to_string()
            }
        };
        server.id = id.clone();
        s.mcp_servers.insert(id, server.clone());
        drop(s);
        self.dirty.store(true, Ordering::Relaxed);
        Ok(server)
    }

    fn allocate_unique_mcp_server_id(
        map: &BTreeMap<String, McpServerDesired>,
        base: &str,
    ) -> String {
        if !map.contains_key(base) {
            return base.to_string();
        }
        let mut n = 2u32;
        loop {
            let candidate = format!("{base}-{n}");
            if !map.contains_key(&candidate) {
                return candidate;
            }
            n += 1;
        }
    }

    pub fn delete_mcp_server(&self, id: &str) -> Result<(), String> {
        let mut s = self.state.write();
        if !s.mcp_servers.contains_key(id) {
            return Err(format!("no mcp server `{id}`"));
        }
        if s.mcp_servers.get(id).is_some_and(|m| m.shipped) {
            return Err(format!("cannot delete shipped mcp server `{id}`"));
        }
        for p in s.sandbox_profiles.values() {
            if p.mcp_server_ids.iter().any(|x| x == id) {
                return Err(format!(
                    "mcp server `{id}` is attached to sandbox spec `{}`",
                    p.id
                ));
            }
        }
        s.mcp_servers.remove(id);
        drop(s);
        self.dirty.store(true, Ordering::Relaxed);
        Ok(())
    }

    /// MCP servers attached for a resolved create, filtered by audience.
    /// Unknown ids are dropped; order follows the profile list.
    pub fn attach_mcp_servers_for_resolved(
        &self,
        resolved: &ResolvedSandboxCreate,
        for_cockpit: bool,
    ) -> Vec<McpServerDesired> {
        self.ensure_shipped_mcp_servers();
        let s = self.state.read();
        resolved
            .mcp_server_ids
            .iter()
            .filter_map(|id| s.mcp_servers.get(id).cloned())
            .filter(|m| {
                if for_cockpit {
                    m.audience.allows_cockpit()
                } else {
                    m.audience.allows_worker()
                }
            })
            .filter(|m| {
                // Workers must never receive cockpit Bearer host /mcp.
                if !for_cockpit {
                    !matches!(
                        m.transport,
                        McpTransport::Http {
                            auth: McpHttpAuth::CockpitBearer,
                            ..
                        }
                    )
                } else {
                    true
                }
            })
            .collect()
    }

    pub fn list_openshell_policies(&self) -> Vec<OpenShellPolicy> {
        let s = self.state.read();
        s.openshell_policies.values().cloned().collect()
    }

    pub fn get_openshell_policy(&self, id: &str) -> Option<OpenShellPolicy> {
        self.state.read().openshell_policies.get(id).cloned()
    }

    /// Insert or replace a Policies catalog entry. Empty `id` means create:
    /// slug from `name` with `-2`, `-3`, … on collision.
    pub fn upsert_openshell_policy(
        &self,
        policy: OpenShellPolicy,
    ) -> Result<OpenShellPolicy, String> {
        if policy.name.trim().is_empty() {
            return Err("policy name must not be empty".into());
        }
        // Empty-check with trim, but keep YAML text as submitted (trailing
        // newline is normal for policy files / textareas).
        if policy.yaml.trim().is_empty() {
            return Err("policy yaml must not be empty".into());
        }
        let name = policy.name.trim().to_string();
        let yaml = policy.yaml;
        let mut s = self.state.write();
        let id = {
            let trimmed = policy.id.trim();
            if trimmed.is_empty() {
                let base = crate::model::slugify_sandbox_profile_id(&name);
                Self::allocate_unique_openshell_policy_id(&s.openshell_policies, &base)
            } else {
                trimmed.to_string()
            }
        };
        let stored = OpenShellPolicy { id, name, yaml };
        s.openshell_policies
            .insert(stored.id.clone(), stored.clone());
        drop(s);
        self.dirty.store(true, Ordering::Relaxed);
        Ok(stored)
    }

    /// Delete a policy. Refused while any sandbox spec references it.
    pub fn delete_openshell_policy(&self, id: &str) -> Result<(), String> {
        let mut s = self.state.write();
        if !s.openshell_policies.contains_key(id) {
            return Err(format!("no policy `{id}`"));
        }
        let in_use: Vec<String> = s
            .sandbox_profiles
            .values()
            .filter(|p| p.policy_id == id)
            .map(|p| p.id.clone())
            .collect();
        if !in_use.is_empty() {
            return Err(format!(
                "cannot delete policy `{id}`: in use by sandbox spec(s) {:?}; \
                 reassign those specs first",
                in_use
            ));
        }
        s.openshell_policies.remove(id);
        drop(s);
        self.dirty.store(true, Ordering::Relaxed);
        Ok(())
    }

    /// Materialize YAML for a profile's `policy_id`. Missing catalog rows fall
    /// through to the minimal built-in (same last-resort as empty catalog).
    fn policy_yaml_for_profile(s: &BoardState, p: &SandboxProfile) -> String {
        let id = p.policy_id.trim();
        if !id.is_empty() {
            if let Some(pol) = s.openshell_policies.get(id) {
                return pol.yaml.clone();
            }
        }
        crate::seed_policies::MINIMAL_SANDBOX_POLICY.to_string()
    }

    pub fn list_sandbox_profiles(&self) -> Vec<SandboxProfile> {
        let s = self.state.read();
        s.sandbox_profiles.values().cloned().collect()
    }

    pub fn default_sandbox_profile_id(&self) -> Option<String> {
        self.state.read().default_sandbox_profile_id.clone()
    }

    pub fn cockpit_sandbox_profile_id(&self) -> Option<String> {
        self.state.read().cockpit_sandbox_profile_id.clone()
    }

    pub fn get_sandbox_profile(&self, id: &str) -> Option<SandboxProfile> {
        self.state.read().sandbox_profiles.get(id).cloned()
    }

    /// Insert or replace a profile. Empty `id` means create: derive a slug from
    /// `name` and append `-2`, `-3`, … when that slug is already taken. Explicit
    /// ids still upsert in place (edit / seed). Does not change the global default.
    /// Requires a known `policy_id` in the Policies catalog.
    pub fn upsert_sandbox_profile(
        &self,
        profile: SandboxProfile,
    ) -> Result<SandboxProfile, String> {
        // So Cockpit force-attach can see shipped `sandboard` even on a cold board.
        self.ensure_shipped_mcp_servers();
        if profile.name.trim().is_empty() {
            return Err("sandbox profile name must not be empty".into());
        }
        if profile.image.trim().is_empty() {
            return Err("sandbox profile image must not be empty".into());
        }
        let policy_id = profile.policy_id.trim().to_string();
        if policy_id.is_empty() {
            return Err("sandbox profile policy_id must not be empty".into());
        }
        let name = profile.name.trim().to_string();
        let image = profile.image.trim().to_string();
        let cpu = profile.cpu.filter(|c| !c.trim().is_empty());
        let memory = profile.memory.filter(|m| !m.trim().is_empty());
        let engine = profile
            .engine
            .map(|e| e.trim().to_string())
            .filter(|e| !e.is_empty());
        let model = profile
            .model
            .map(|m| m.trim().to_string())
            .filter(|m| !m.is_empty());
        let provider_names: Vec<String> = profile
            .provider_names
            .into_iter()
            .map(|n| n.trim().to_string())
            .filter(|n| !n.is_empty())
            .collect();
        let mut mcp_server_ids: Vec<String> = profile
            .mcp_server_ids
            .into_iter()
            .map(|n| n.trim().to_string())
            .filter(|n| !n.is_empty())
            .collect();
        let env: BTreeMap<String, String> = profile
            .env
            .into_iter()
            .map(|(k, v)| (k.trim().to_string(), v))
            .filter(|(k, _)| !k.is_empty())
            .collect();
        let prompt = profile
            .prompt
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty());
        let mut s = self.state.write();
        if !s.openshell_policies.contains_key(&policy_id) {
            return Err(format!("no policy `{policy_id}`"));
        }
        for mid in &mcp_server_ids {
            if !s.mcp_servers.contains_key(mid) {
                return Err(format!("no mcp server `{mid}`"));
            }
        }
        let id = {
            let trimmed = profile.id.trim();
            if trimmed.is_empty() {
                let base = crate::model::slugify_sandbox_profile_id(&name);
                Self::allocate_unique_sandbox_profile_id(&s.sandbox_profiles, &base)
            } else {
                trimmed.to_string()
            }
        };
        // Cockpit target cannot drop shipped sandboard — resolve/inject re-add it.
        let cockpit_target = s
            .cockpit_sandbox_profile_id
            .as_deref()
            .or(s.default_sandbox_profile_id.as_deref());
        let becomes_first_default =
            s.sandbox_profiles.is_empty() && s.default_sandbox_profile_id.is_none();
        if (cockpit_target == Some(id.as_str()) || becomes_first_default)
            && s.mcp_servers.contains_key(SANDBOARD_MCP_SERVER_ID)
            && !mcp_server_ids.iter().any(|x| x == SANDBOARD_MCP_SERVER_ID)
        {
            mcp_server_ids.insert(0, SANDBOARD_MCP_SERVER_ID.into());
        }
        let stored = SandboxProfile {
            id,
            name,
            image,
            policy_id,
            policy_inline_legacy: None,
            cpu,
            memory,
            engine,
            model,
            provider_names,
            mcp_server_ids,
            env,
            prompt,
            // Operator-authored path — shipped rows only come from
            // `ensure_shipped_sandbox_profiles`.
            shipped: false,
        };
        let first = s.sandbox_profiles.is_empty();
        s.sandbox_profiles.insert(stored.id.clone(), stored.clone());
        // First catalog entry becomes the global default. Cockpit inherits it
        // until an explicit cockpit preference is set.
        if first && s.default_sandbox_profile_id.is_none() {
            s.default_sandbox_profile_id = Some(stored.id.clone());
        }
        drop(s);
        self.dirty.store(true, Ordering::Relaxed);
        Ok(stored)
    }

    pub fn set_default_sandbox_profile(&self, id: &str) -> Result<(), String> {
        let mut s = self.state.write();
        if !s.sandbox_profiles.contains_key(id) {
            return Err(format!("no sandbox profile `{id}`"));
        }
        s.default_sandbox_profile_id = Some(id.to_string());
        drop(s);
        self.dirty.store(true, Ordering::Relaxed);
        // When Cockpit inherits the global default, keep shipped sandboard attached.
        if self.cockpit_sandbox_profile_id().is_none() {
            let _ = self.ensure_cockpit_sandboard_mcp_attach();
        }
        Ok(())
    }

    pub fn set_cockpit_sandbox_profile(&self, id: &str) -> Result<(), String> {
        let mut s = self.state.write();
        if !s.sandbox_profiles.contains_key(id) {
            return Err(format!("no sandbox profile `{id}`"));
        }
        s.cockpit_sandbox_profile_id = Some(id.to_string());
        drop(s);
        self.dirty.store(true, Ordering::Relaxed);
        let _ = self.ensure_cockpit_sandboard_mcp_attach();
        Ok(())
    }

    /// Clear the Cockpit override so Cockpit uses the global default profile.
    pub fn clear_cockpit_sandbox_profile(&self) {
        let mut s = self.state.write();
        if s.cockpit_sandbox_profile_id.take().is_some() {
            drop(s);
            self.dirty.store(true, Ordering::Relaxed);
            let _ = self.ensure_cockpit_sandboard_mcp_attach();
        }
    }

    /// Assign a Project's sandbox profile override. `None` clears (inherit global default).
    pub fn set_project_sandbox_profile(
        &self,
        project_id: ItemId,
        profile_id: Option<String>,
    ) -> Result<WorkItem, String> {
        let item = {
            let mut s = self.state.write();
            let it = s
                .items
                .get(&project_id)
                .ok_or_else(|| format!("no such item #{project_id}"))?;
            if !it.is_project() {
                return Err(format!("#{project_id} is not a Project"));
            }
            let resolved = match profile_id {
                None => None,
                Some(pid) => {
                    let pid = pid.trim().to_string();
                    if pid.is_empty() {
                        None
                    } else {
                        if !s.sandbox_profiles.contains_key(&pid) {
                            return Err(format!("no sandbox profile `{pid}`"));
                        }
                        Some(pid)
                    }
                }
            };
            let it = s.items.get_mut(&project_id).unwrap();
            it.sandbox_profile_id = resolved;
            it.clone()
        };
        self.emit(&item);
        Ok(item)
    }

    /// Delete a profile. Refused while assigned to any Project — clear those
    /// overrides first. Clearing the global default / Cockpit designation is
    /// allowed (prefs become unset).
    pub fn delete_sandbox_profile(&self, id: &str) -> Result<(), String> {
        let mut s = self.state.write();
        if !s.sandbox_profiles.contains_key(id) {
            return Err(format!("no sandbox profile `{id}`"));
        }
        let in_use: Vec<ItemId> = s
            .items
            .values()
            .filter(|i| i.is_project() && i.sandbox_profile_id.as_deref() == Some(id))
            .map(|i| i.id)
            .collect();
        if !in_use.is_empty() {
            return Err(format!(
                "cannot delete sandbox profile `{id}`: in use by Project(s) {:?}; \
                 clear or reassign those overrides first",
                in_use
            ));
        }
        if s.default_sandbox_profile_id.as_deref() == Some(id) {
            s.default_sandbox_profile_id = None;
        }
        if s.cockpit_sandbox_profile_id.as_deref() == Some(id) {
            s.cockpit_sandbox_profile_id = None;
        }
        s.sandbox_profiles.remove(id);
        drop(s);
        self.dirty.store(true, Ordering::Relaxed);
        Ok(())
    }

    // ------------------------------------------------ cockpit session (board state)
    //
    // Durable control-plane seat. Mutations only here / machine.rs — not a
    // second lifecycle in api/mcp/supervisor glue, and not card claim/report.

    pub fn cockpit_session(&self) -> Option<CockpitSession> {
        self.state.read().cockpit_session.clone()
    }

    /// Create the singleton cockpit session (`Running`). Fails if one already exists.
    pub fn create_cockpit_session(
        &self,
        environment: Option<String>,
        conversation_id: Option<String>,
    ) -> Result<CockpitSession, String> {
        let mut s = self.state.write();
        machine::check_cockpit_create(&s.cockpit_session).map_err(|e| e.to_string())?;
        let session = CockpitSession::new(environment, conversation_id);
        s.cockpit_session = Some(session.clone());
        drop(s);
        self.dirty.store(true, Ordering::Relaxed);
        Ok(session)
    }

    /// Patch sandbox environment and/or conversation id. `None` leaves a field
    /// unchanged; `Some(s)` sets it (blank clears). Hold status is unchanged.
    /// Sandbox phase is supervisor-owned via [`Self::set_cockpit_sandbox_phase`].
    pub fn update_cockpit_session(
        &self,
        environment: Option<String>,
        conversation_id: Option<String>,
    ) -> Result<CockpitSession, String> {
        let mut s = self.state.write();
        machine::check_cockpit_present(&s.cockpit_session).map_err(|e| e.to_string())?;
        let session = s.cockpit_session.as_mut().expect("checked present");
        if environment.is_some() {
            session.environment = normalize_cockpit_field(environment);
        }
        if conversation_id.is_some() {
            session.conversation_id = normalize_cockpit_field(conversation_id);
        }
        session.updated_at = Utc::now();
        let out = session.clone();
        drop(s);
        self.dirty.store(true, Ordering::Relaxed);
        Ok(out)
    }

    /// Supervisor-owned sandbox lifecycle for Cockpit UI. No-op if no session.
    /// Returns the updated session when a session was present.
    pub fn set_cockpit_sandbox_phase(
        &self,
        phase: CockpitSandboxPhase,
        detail: Option<String>,
    ) -> Option<CockpitSession> {
        let mut s = self.state.write();
        let session = s.cockpit_session.as_mut()?;
        session.set_sandbox_phase(phase, detail);
        let out = session.clone();
        drop(s);
        self.dirty.store(true, Ordering::Relaxed);
        Some(out)
    }

    /// Park-like hold: keep sandbox + conversation; mark `Parked`.
    pub fn park_cockpit_session(&self) -> Result<CockpitSession, String> {
        let mut s = self.state.write();
        let session = machine::check_cockpit_present(&s.cockpit_session)
            .map_err(|e| e.to_string())?
            .clone();
        machine::check_cockpit_park(&session).map_err(|e| e.to_string())?;
        let slot = s.cockpit_session.as_mut().expect("checked present");
        slot.status = CockpitSessionStatus::Parked;
        slot.updated_at = Utc::now();
        let out = slot.clone();
        drop(s);
        self.dirty.store(true, Ordering::Relaxed);
        Ok(out)
    }

    /// Clear park hold → `Running` (attach / reconcile resume path).
    pub fn resume_cockpit_session(&self) -> Result<CockpitSession, String> {
        let mut s = self.state.write();
        let session = machine::check_cockpit_present(&s.cockpit_session)
            .map_err(|e| e.to_string())?
            .clone();
        machine::check_cockpit_resume(&session).map_err(|e| e.to_string())?;
        let slot = s.cockpit_session.as_mut().expect("checked present");
        slot.status = CockpitSessionStatus::Running;
        slot.updated_at = Utc::now();
        let out = slot.clone();
        drop(s);
        self.dirty.store(true, Ordering::Relaxed);
        Ok(out)
    }

    /// Stop and clear the durable cockpit session. Idempotent when already absent.
    ///
    /// Marks `stopping` briefly so a concurrent poll can see it, then clears.
    pub fn stop_cockpit_session(&self) -> Result<(), String> {
        let mut s = self.state.write();
        let Some(session) = s.cockpit_session.as_mut() else {
            return Ok(());
        };
        session.set_sandbox_phase(CockpitSandboxPhase::Stopping, None);
        s.cockpit_session = None;
        drop(s);
        self.dirty.store(true, Ordering::Relaxed);
        Ok(())
    }

    /// Create knobs for the Cockpit / cockpit sandbox.
    ///
    /// Order: explicit `cockpit_sandbox_profile_id` → `default_sandbox_profile_id`
    /// → minimal compiled last-resort ([`ResolvedSandboxCreate::from_agents`]).
    pub fn resolve_cockpit_sandbox_create(&self) -> ResolvedSandboxCreate {
        self.ensure_shipped_mcp_servers();
        let s = self.state.read();
        if let Some(ref cid) = s.cockpit_sandbox_profile_id {
            if let Some(p) = s.sandbox_profiles.get(cid) {
                return Self::materialize_resolved(&s, p, true);
            }
        }
        if let Some(ref did) = s.default_sandbox_profile_id {
            if let Some(p) = s.sandbox_profiles.get(did) {
                return Self::materialize_resolved(&s, p, true);
            }
        }
        drop(s);
        ResolvedSandboxCreate::from_agents(&AgentConfig::default())
    }

    /// Engine for Cockpit attach / chat: profile engine, else Agent runtime.
    pub fn resolve_cockpit_engine(&self) -> String {
        let resolved = self.resolve_cockpit_sandbox_create();
        if let Some(e) = resolved
            .engine
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            return e.to_string();
        }
        let fallback = self.effective_agents().engine;
        let t = fallback.trim();
        if t.is_empty() {
            "cursor".into()
        } else {
            t.to_string()
        }
    }

    /// Resolve create knobs for a card at sandbox create.
    ///
    /// Order: Project `sandbox_profile_id` → board `default_sandbox_profile_id`
    /// → compiled [`AgentConfig::default`] + minimal policy (last resort).
    /// Missing catalog entries fall through. Do not weaken this order.
    pub fn resolve_sandbox_create(&self, item_id: ItemId) -> ResolvedSandboxCreate {
        self.ensure_shipped_mcp_servers();
        let item = match self.get(item_id) {
            Some(i) => i,
            None => return ResolvedSandboxCreate::from_agents(&AgentConfig::default()),
        };
        let project = if item.is_project() {
            Some(item)
        } else {
            item.parent.and_then(|pid| self.get(pid))
        };
        let override_id = project.as_ref().and_then(|p| p.sandbox_profile_id.clone());

        let s = self.state.read();
        if let Some(ref oid) = override_id {
            if let Some(p) = s.sandbox_profiles.get(oid) {
                return Self::materialize_resolved(&s, p, false);
            }
        }
        if let Some(ref did) = s.default_sandbox_profile_id {
            if let Some(p) = s.sandbox_profiles.get(did) {
                return Self::materialize_resolved(&s, p, false);
            }
        }
        drop(s);
        ResolvedSandboxCreate::from_agents(&AgentConfig::default())
    }

    /// Profile → resolved create: audience-filter MCP ids, merge policy fragments,
    /// union provider names from attached MCP servers.
    ///
    /// Cockpit always includes shipped `sandboard` when present in the catalog, even
    /// if the profile list omitted it (same as inject).
    fn materialize_resolved(
        s: &BoardState,
        p: &SandboxProfile,
        for_cockpit: bool,
    ) -> ResolvedSandboxCreate {
        let base_yaml = Self::policy_yaml_for_profile(s, p);
        let mut servers: Vec<&McpServerDesired> = p
            .mcp_server_ids
            .iter()
            .filter_map(|id| s.mcp_servers.get(id))
            .filter(|m| {
                if for_cockpit {
                    m.audience.allows_cockpit()
                } else {
                    m.audience.allows_worker()
                        && !matches!(
                            m.transport,
                            McpTransport::Http {
                                auth: McpHttpAuth::CockpitBearer,
                                ..
                            }
                        )
                }
            })
            .collect();
        if for_cockpit {
            if let Some(sandboard) = s.mcp_servers.get(SANDBOARD_MCP_SERVER_ID) {
                if sandboard.audience.allows_cockpit()
                    && !servers.iter().any(|m| m.id == SANDBOARD_MCP_SERVER_ID)
                {
                    servers.insert(0, sandboard);
                }
            }
        }
        let fragments: Vec<&str> = servers
            .iter()
            .filter_map(|m| m.policy_fragment_yaml.as_deref())
            .filter(|f| !f.trim().is_empty())
            .collect();
        // No fragments → keep catalog YAML bytes exact (comments / spacing).
        let policy = if fragments.is_empty() {
            base_yaml
        } else {
            match crate::mcp_policy::merge_policy_fragments(&base_yaml, fragments) {
                Ok(y) => y,
                Err(e) => {
                    tracing::warn!(
                        profile = %p.id,
                        error = %e,
                        "mcp policy fragment merge failed; using base policy"
                    );
                    base_yaml
                }
            }
        };
        let mut providers = p.provider_names.clone();
        for m in &servers {
            for n in &m.provider_names {
                let n = n.trim();
                if !n.is_empty() && !providers.iter().any(|x| x == n) {
                    providers.push(n.to_string());
                }
            }
        }
        let mut resolved = ResolvedSandboxCreate::from_profile(p, &policy);
        resolved.providers = providers;
        resolved.mcp_server_ids = servers.iter().map(|m| m.id.clone()).collect();
        resolved.policy = policy;
        resolved
    }

    /// Engine for a card at claim/run: sandbox profile engine, else Agent runtime.
    ///
    /// Ignores stale `WorkItem.engine` — engine lives on the profile now.
    pub fn resolve_engine_for_card(&self, item_id: ItemId) -> String {
        let create = self.resolve_sandbox_create(item_id);
        if let Some(e) = create
            .engine
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            return e.to_string();
        }
        let fallback = self.effective_agents().engine;
        let t = fallback.trim();
        if t.is_empty() {
            "cursor".into()
        } else {
            t.to_string()
        }
    }

    /// Model for agent CLI argv: card.model → sandbox spec model → engine default.
    pub fn resolve_model_for_card(&self, item_id: ItemId) -> Option<String> {
        let engine = self.resolve_engine_for_card(item_id);
        let card_model = self
            .get(item_id)
            .and_then(|i| i.model.clone())
            .map(|m| m.trim().to_string())
            .filter(|m| !m.is_empty());
        if card_model.is_some() {
            return card_model;
        }
        let spec_model = self.resolve_sandbox_create(item_id).model;
        if let Some(m) = spec_model {
            let t = m.trim();
            if !t.is_empty() {
                return Some(t.to_string());
            }
        }
        crate::engine::default_model_for_engine(&engine).map(|s| s.to_string())
    }

    /// Model for Cockpit attach/chat: sandbox spec model → engine default (no card).
    pub fn resolve_cockpit_model(&self) -> Option<String> {
        let engine = self.resolve_cockpit_engine();
        let spec_model = self.resolve_cockpit_sandbox_create().model;
        if let Some(m) = spec_model {
            let t = m.trim();
            if !t.is_empty() {
                return Some(t.to_string());
            }
        }
        crate::engine::default_model_for_engine(&engine).map(|s| s.to_string())
    }

    // ------------------------------------------------------- the agent verbs

    /// A card still leased to this agent — survives a restart mid-flight. The
    /// supervisor's startup reconciliation is the next caller: sandboard restarts
    /// constantly while sandboard is what's being built, and sandboxes outlive it.
    #[allow(dead_code)]
    pub fn leased_to(&self, agent_id: &str) -> Option<ItemId> {
        let s = self.state.read();
        s.items
            .values()
            .find(|i| {
                matches!(i.state, State::Claimed | State::Running)
                    && i.lease
                        .as_ref()
                        .map(|l| l.agent_id == agent_id)
                        .unwrap_or(false)
            })
            .map(|i| i.id)
    }

    /// Backlog leaves that are unblocked and match capabilities. Not a start
    /// queue — operator must `enqueue_dispatch` before the supervisor claims.
    ///
    /// Uses `ids_by_state` + denormalized leaf/blocker checks (not a full scan).
    pub fn list_backlog(&self, capabilities: &[String]) -> Vec<WorkItem> {
        let s = self.state.read();
        let Some(ids) = s.ids_by_state.get(&State::Backlog) else {
            return Vec::new();
        };
        ids.iter()
            .filter_map(|id| s.items.get(id))
            .filter(|i| i.level.as_deref() != Some("Project"))
            .filter(|i| !Self::has_children(&s, i.id))
            .filter(|i| Self::unresolved_blockers(&s, i).is_empty())
            .filter(|i| match &i.capability {
                None => true,
                Some(c) if c == "any" => true,
                Some(c) => capabilities.iter().any(|have| have == c),
            })
            .map(|i| {
                let mut item = i.clone();
                Self::populate_blockers(&s, &mut item);
                item
            })
            .collect()
    }

    /// Legacy name for operator `list_ready` MCP tool.
    pub fn list_ready(&self, capabilities: &[String]) -> Vec<WorkItem> {
        self.list_backlog(capabilities)
    }

    /// Cards the operator asked to start, oldest first. Supervisor drains these.
    ///
    /// Uses `ids_by_state` + denormalized leaf/blocker checks (not a full scan).
    pub fn list_awaiting_dispatch(&self) -> Vec<WorkItem> {
        let s = self.state.read();
        let Some(ids) = s.ids_by_state.get(&State::Backlog) else {
            return Vec::new();
        };
        let mut items: Vec<_> = ids
            .iter()
            .filter_map(|id| s.items.get(id))
            .filter(|i| i.awaiting_dispatch && !i.parked)
            .filter(|i| i.level.as_deref() != Some("Project"))
            .filter(|i| !Self::has_children(&s, i.id))
            .filter(|i| Self::unresolved_blockers(&s, i).is_empty())
            .cloned()
            .collect();
        items.sort_by_key(|i| i.entered_state_at);
        items
    }

    /// Operator asked the supervisor to start this Backlog card.
    pub fn enqueue_dispatch(&self, id: ItemId) -> Result<WorkItem, String> {
        if !self.may_claim(id) {
            return Err("card is parked; unpark before dispatch".into());
        }
        let item = {
            let mut s = self.state.write();
            let has_children = Self::has_children(&s, id);
            let (state, parked, is_project, dod_missing, blockers) = {
                let it = s.items.get(&id).ok_or("no such item")?;
                (
                    it.state,
                    it.parked,
                    it.level.as_deref() == Some("Project"),
                    it.definition_of_done.is_none(),
                    Self::unresolved_blockers(&s, it),
                )
            };
            if state != State::Backlog {
                return Err("only a Backlog card can be dispatched".into());
            }
            if parked {
                return Err("card is parked; unpark before dispatch".into());
            }
            if is_project || has_children {
                return Err("containers are not dispatchable".into());
            }
            if !blockers.is_empty() {
                return Err(format!("card is blocked by {blockers:?}"));
            }
            if dod_missing {
                return Err("leaf needs a definition of done before dispatch".into());
            }
            let it = s.items.get_mut(&id).unwrap();
            it.awaiting_dispatch = true;
            let mut out = it.clone();
            Self::populate_blockers(&s, &mut out);
            out
        };
        self.emit(&item);
        self.story(
            id,
            format!(
                "{}: queued for dispatch — supervisor will claim when a slot opens.",
                item.title
            ),
        );
        Ok(item)
    }

    /// Cancel a pending start request without changing state.
    #[allow(dead_code)]
    pub fn clear_dispatch(&self, id: ItemId) -> Result<WorkItem, String> {
        let item = {
            let mut s = self.state.write();
            let it = s.items.get_mut(&id).ok_or("no such item")?;
            it.awaiting_dispatch = false;
            it.rebase_requested = false;
            it.clone()
        };
        self.emit(&item);
        Ok(item)
    }

    /// Play/pause Project auto mode. On: queue claimable Backlog leaves now.
    /// Off: clear `awaiting_dispatch` on still-Backlog leaves (does not halt runners).
    pub fn set_auto_dispatch(&self, id: ItemId, enabled: bool) -> Result<WorkItem, String> {
        let (title, changed, item) = {
            let mut s = self.state.write();
            let it = s.items.get_mut(&id).ok_or("no such item")?;
            if it.level.as_deref() != Some("Project") && !it.is_project() {
                return Err("auto mode is only for Projects".into());
            }
            let changed = it.auto_dispatch != enabled;
            if changed {
                it.auto_dispatch = enabled;
            }
            let title = it.title.clone();
            let mut out = it.clone();
            Self::populate_blockers(&s, &mut out);
            (title, changed, out)
        };
        if !changed {
            return Ok(item);
        }
        self.emit(&item);
        if enabled {
            self.story(
                id,
                format!("{title}: auto mode on — claimable Backlog will start on its own."),
            );
            self.auto_enqueue_project(id);
        } else {
            let cleared = self.clear_awaiting_under_project(id);
            self.story(
                id,
                format!(
                    "{title}: auto mode off — cleared {cleared} queued Backlog card(s); \
                     in-flight runs continue."
                ),
            );
        }
        self.get(id).ok_or_else(|| "no such item".into())
    }

    /// Clear `awaiting_dispatch` on Backlog leaves under a Project. Returns count cleared.
    fn clear_awaiting_under_project(&self, project_id: ItemId) -> usize {
        let ids: Vec<ItemId> = {
            let s = self.state.read();
            s.items
                .values()
                .filter(|i| i.parent == Some(project_id))
                .filter(|i| i.state == State::Backlog && i.awaiting_dispatch)
                .map(|i| i.id)
                .collect()
        };
        for id in &ids {
            let _ = self.clear_dispatch(*id);
        }
        ids.len()
    }

    /// Queue every claimable Backlog leaf under Projects with `auto_dispatch`.
    /// Called each supervisor tick — skips cards already awaiting.
    pub fn auto_enqueue_all(&self) {
        let project_ids: Vec<ItemId> = {
            let s = self.state.read();
            s.items
                .values()
                .filter(|i| i.auto_dispatch && i.state != State::Retired)
                .filter(|i| i.level.as_deref() == Some("Project") || i.is_project())
                .map(|i| i.id)
                .collect()
        };
        for pid in project_ids {
            self.auto_enqueue_project(pid);
        }
    }

    /// Enqueue claimable Backlog leaves under one Project (already-queued skipped).
    pub fn auto_enqueue_project(&self, project_id: ItemId) {
        let candidates: Vec<ItemId> = {
            let s = self.state.read();
            let Some(project) = s.items.get(&project_id) else {
                return;
            };
            if !project.auto_dispatch {
                return;
            }
            s.items
                .values()
                .filter(|i| i.parent == Some(project_id))
                .filter(|i| i.state == State::Backlog && !i.awaiting_dispatch && !i.parked)
                .filter(|i| i.level.as_deref() != Some("Project"))
                .filter(|i| !Self::has_children(&s, i.id))
                .filter(|i| i.definition_of_done.is_some())
                .filter(|i| Self::unresolved_blockers(&s, i).is_empty())
                .map(|i| i.id)
                .collect()
        };
        for id in candidates {
            let _ = self.enqueue_dispatch(id);
        }
    }

    /// `claim` — assigns the card and a fixed run deadline (`timeout_secs`).
    /// The deadline is not extended by heartbeats.
    pub fn claim(
        &self,
        id: ItemId,
        agent_id: &str,
        model: Option<String>,
        timeout_secs: i64,
    ) -> Result<ClaimGrant, TransitionError> {
        let now = Utc::now();
        let deadline = now + Duration::seconds(timeout_secs.max(1));

        let item = {
            let mut s = self.state.write();
            if s.items.get(&id).is_some_and(|it| it.parked) {
                return Err(TransitionError::Parked { id });
            }
            Self::transition_locked(&mut s, id, State::Claimed, agent_id, None)?;
            let it = s.items.get_mut(&id).unwrap();
            it.parked = false;
            it.awaiting_dispatch = false;
            it.rebase_requested = false;
            it.run_deadline_at = Some(deadline);
            it.lease = Some(Lease {
                agent_id: agent_id.to_string(),
                granted_at: now,
                last_heartbeat: now,
                expires_at: deadline,
            });
            if model.is_some() {
                it.model = model;
            }
            it.clone()
        };
        self.emit(&item);

        let ctx = self.claim_plan_context(id, &item);
        let engine = Some(self.resolve_engine_for_card(id));
        let model = self.resolve_model_for_card(id);
        let sandbox_prompt = self.resolve_sandbox_create(id).prompt;
        if self.agent_runtime().is_none() {
            let _ = self.seed_agent_runtime_if_empty();
        }
        let board_prompt = self
            .agent_runtime()
            .and_then(|rt| {
                let t = rt.standing_prompt.trim().to_string();
                if t.is_empty() {
                    None
                } else {
                    Some(t)
                }
            });

        Ok(ClaimGrant {
            item_id: id,
            title: item.title.clone(),
            intent: item.intent.clone(),
            definition_of_done: item.definition_of_done.clone(),
            project_title: ctx.project_title,
            board_prompt,
            project_prompt: ctx.project_prompt,
            sandbox_prompt,
            plan_summary: ctx.plan_summary,
            plan_tasks: ctx.plan_tasks,
            plan_task_key: ctx.plan_task_key,
            notes: item.notes.iter().map(|n| n.text.clone()).collect(),
            lease_expires_at: deadline,
            run_deadline_at: deadline,
            engine,
            model,
        })
    }

    /// Resolve Project prompt + Plan rows for the card being claimed.
    /// Plan rows come from the Initial plan card's (frozen) proposal.
    fn claim_plan_context(&self, id: ItemId, item: &WorkItem) -> ClaimPlanContext {
        let project = if item.is_project() {
            Some(item.clone())
        } else {
            item.parent.and_then(|pid| self.get(pid))
        };
        let Some(project) = project else {
            return ClaimPlanContext::default();
        };
        let project_title = Some(project.title.clone());
        let project_prompt = project.project_prompt.clone();

        let seed = self.initial_plan_of(project.id);
        let proposal = seed.as_ref().and_then(|s| s.proposal.as_ref());
        let Some(proposal) = proposal.filter(|p| !p.tasks.is_empty()) else {
            return ClaimPlanContext {
                project_title,
                project_prompt,
                ..Default::default()
            };
        };

        let plan_summary = if proposal.summary.trim().is_empty() {
            None
        } else {
            Some(proposal.summary.clone())
        };
        let title_key = Self::normalize_title(&item.title);
        let mut plan_task_key = None;
        let plan_tasks: Vec<crate::model::PlanTaskBrief> = proposal
            .tasks
            .iter()
            .map(|t| {
                let current = t.item_id == Some(id)
                    || (t.item_id.is_none() && Self::normalize_title(&t.title) == title_key);
                if current {
                    plan_task_key = Some(t.key.clone());
                }
                crate::model::PlanTaskBrief {
                    key: t.key.clone(),
                    title: t.title.clone(),
                    intent: t.intent.clone(),
                    definition_of_done: t.definition_of_done.clone(),
                    blocked_by_keys: t.blocked_by_keys.clone(),
                    current,
                }
            })
            .collect();
        ClaimPlanContext {
            project_title,
            project_prompt,
            plan_summary,
            plan_tasks,
            plan_task_key,
        }
    }

    /// `heartbeat` — progress only. Does **not** extend `run_deadline_at`.
    /// `lease_secs` is ignored (kept for MCP compatibility).
    ///
    /// Quiet while already Running with unchanged progress: still stamps
    /// `last_heartbeat`, but skips the SSE Upsert. The agent stream used to
    /// call this on every stdout line; emitting a full card each time wedged
    /// the board lock and made the drawer hang on `/api/items/:id`.
    pub fn heartbeat(
        &self,
        id: ItemId,
        agent_id: &str,
        progress: f32,
        _lease_secs: i64,
    ) -> Result<WorkItem, TransitionError> {
        let (item, emit) = {
            let mut s = self.state.write();
            let mut promoted = false;
            // First heartbeat promotes Claimed -> Running.
            if s.items.get(&id).map(|i| i.state) == Some(State::Claimed) {
                Self::transition_locked(&mut s, id, State::Running, agent_id, None)?;
                promoted = true;
            }
            let now = Utc::now();
            let progress = progress.clamp(0.0, 1.0);
            let it = s
                .items
                .get_mut(&id)
                .ok_or(TransitionError::NoSuchItem(id))?;
            let progress_changed = (it.progress - progress).abs() > f32::EPSILON;
            it.progress = progress;
            if let Some(l) = it.lease.as_mut() {
                l.last_heartbeat = now;
                // Do not touch expires_at / run_deadline_at — one fixed timeout.
            }
            (it.clone(), promoted || progress_changed)
        };
        if emit {
            self.emit(&item);
        }
        Ok(item)
    }

    fn normalize_title(title: &str) -> String {
        title
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase()
    }

    /// Pick `base`, or `base-2`, `base-3`, … until unused in the catalog.
    fn allocate_unique_sandbox_profile_id(
        existing: &BTreeMap<String, SandboxProfile>,
        base: &str,
    ) -> String {
        if !existing.contains_key(base) {
            return base.to_string();
        }
        let mut n = 2u32;
        loop {
            let candidate = format!("{base}-{n}");
            if !existing.contains_key(&candidate) {
                return candidate;
            }
            n = n.saturating_add(1);
            if n == u32::MAX {
                // Pathological; still must terminate.
                return format!("{base}-{n}");
            }
        }
    }

    fn allocate_unique_openshell_policy_id(
        existing: &BTreeMap<String, OpenShellPolicy>,
        base: &str,
    ) -> String {
        if !existing.contains_key(base) {
            return base.to_string();
        }
        let mut n = 2u32;
        loop {
            let candidate = format!("{base}-{n}");
            if !existing.contains_key(&candidate) {
                return candidate;
            }
            n = n.saturating_add(1);
            if n == u32::MAX {
                return format!("{base}-{n}");
            }
        }
    }

    fn tokenize_text(text: &str) -> HashSet<String> {
        let stop_words: HashSet<&'static str> = [
            "a", "an", "the", "and", "or", "but", "if", "because", "as", "until", "while", "of",
            "at", "by", "for", "with", "about", "against", "between", "into", "through", "during",
            "before", "after", "above", "below", "to", "from", "up", "down", "in", "out", "on",
            "off", "over", "under", "again", "further", "then", "once", "here", "there", "when",
            "where", "why", "how", "all", "any", "both", "each", "few", "more", "most", "other",
            "some", "such", "no", "nor", "not", "only", "own", "same", "so", "than", "too", "very",
            "s", "t", "can", "will", "just", "don", "should", "now", "is", "are", "was", "were",
            "be", "been", "being", "have", "has", "had", "do", "does", "did", "doing", "would",
            "could", "this", "that", "these", "those", "i", "you", "he", "she", "it", "we", "they",
            "me", "him", "her", "us", "them", "my", "your", "his", "their", "our", "its",
        ]
        .into_iter()
        .collect();

        text.to_lowercase()
            .split(|c: char| !c.is_alphanumeric())
            .filter(|s| s.len() >= 2 && !stop_words.contains(s))
            .map(|s| s.to_string())
            .collect()
    }

    fn is_token_related(child_token: &str, theme_token: &str) -> bool {
        if child_token == theme_token {
            return true;
        }
        let min_len = child_token.len().min(theme_token.len());
        if min_len >= 3 {
            if child_token.starts_with(theme_token) || theme_token.starts_with(child_token) {
                return true;
            }
            if min_len >= 4
                && child_token.is_char_boundary(4)
                && theme_token.is_char_boundary(4)
                && child_token[..4] == theme_token[..4]
            {
                return true;
            }
        }
        false
    }

    fn child_is_related(child_tokens: &HashSet<String>, theme_tokens: &HashSet<String>) -> bool {
        if theme_tokens.is_empty() {
            return true;
        }
        for c_tok in child_tokens {
            for t_tok in theme_tokens {
                if Self::is_token_related(c_tok, t_tok) {
                    return true;
                }
            }
        }
        false
    }

    fn check_split_relatedness(
        card: &WorkItem,
        project: &WorkItem,
        children: &[crate::model::SplitChildSpec],
    ) -> Result<(), String> {
        let mut theme_text = String::new();
        theme_text.push_str(&project.title);
        theme_text.push(' ');
        theme_text.push_str(&project.intent);
        theme_text.push(' ');
        theme_text.push_str(&card.title);
        theme_text.push(' ');
        theme_text.push_str(&card.intent);
        if let Some(ref dod) = card.definition_of_done {
            theme_text.push(' ');
            theme_text.push_str(dod);
        }

        let theme_tokens = Self::tokenize_text(&theme_text);

        for child in children {
            let mut child_text = String::new();
            child_text.push_str(&child.title);
            child_text.push(' ');
            child_text.push_str(&child.intent);
            child_text.push(' ');
            child_text.push_str(&child.definition_of_done);

            let child_tokens = Self::tokenize_text(&child_text);

            if !Self::child_is_related(&child_tokens, &theme_tokens) {
                return Err(format!(
                    "split child '{}' does not relate to parent card or project theme",
                    child.title
                ));
            }
        }

        Ok(())
    }

    /// Validate children and park them on the card as a proposal in Review.
    /// Does **not** create sibling Tasks — Approve materializes them.
    ///
    /// PR and proposal are mutually exclusive. Optional `key` / `blocked_by_keys`
    /// match the Plan task shape.
    pub fn propose_split(
        &self,
        id: ItemId,
        agent_id: &str,
        children: Vec<crate::model::SplitChildSpec>,
        max_children: usize,
    ) -> Result<WorkItem, String> {
        let card = self.get(id).ok_or("no such item")?;

        if card.is_initial_plan_task() {
            return Err(
                "Initial plan cannot use split.json; finish with plan.json (Review); \
                 Approve materializes sibling Tasks"
                    .into(),
            );
        }

        if let Some(pr_url) = card.pr_url() {
            let msg = format!(
                "cannot propose split on card #{id}: a PR already exists ({pr_url}); \
                 split and publish are mutually exclusive"
            );
            let _ = self.escalate(
                id,
                agent_id,
                msg.clone(),
                vec![
                    EscalationOption {
                        label: "Finish card via report".into(),
                        detail: "Complete the card with the existing PR using report.".into(),
                    },
                    EscalationOption {
                        label: "Close PR and retry split".into(),
                        detail: "Close or abandon the existing PR before splitting the card."
                            .into(),
                    },
                ],
                0,
            );
            return Err(msg);
        }

        if children.len() < 2 {
            return Err(
                "a split needs at least two siblings; use report if the work is one card".into(),
            );
        }
        if children.len() > max_children {
            return Err(format!(
                "split of {} siblings exceeds max_children_per_split={max_children}; escalating \
                 rather than fanning out",
                children.len()
            ));
        }

        let project_id = card.parent.ok_or_else(|| {
            "cannot split a Project; only Tasks under a Project can split into siblings".to_string()
        })?;
        let project = self
            .get(project_id)
            .ok_or_else(|| "project not found".to_string())?;

        {
            let s = self.state.read();
            if s.items.get(&project_id).and_then(|p| p.parent).is_some() {
                return Err("split target is not under a Project root".into());
            }
        }

        Self::check_split_relatedness(&card, &project, &children)?;

        let mut seen_req = HashSet::new();
        let mut specs = Vec::new();
        for (idx, child) in children.into_iter().enumerate() {
            let title_key = Self::normalize_title(&child.title);
            if title_key.is_empty() || !seen_req.insert(title_key) {
                continue;
            }
            let key = child
                .key
                .map(|k| k.trim().to_string())
                .filter(|k| !k.is_empty())
                .unwrap_or_else(|| format!("s{}", idx + 1));
            specs.push(PlanTaskSpec {
                key,
                title: child.title,
                intent: child.intent,
                definition_of_done: child.definition_of_done,
                blocked_by_keys: child.blocked_by_keys,
                capability: None,
                repo: child.repo,
                item_id: None,
            });
        }
        if specs.len() < 2 {
            return Err(
                "a split needs at least two distinct sibling titles; use report if the work is one card"
                    .into(),
            );
        }

        let summary = format!("Split of «{}»", card.title);
        self.set_proposal(
            id,
            TaskProposal {
                summary,
                tasks: specs.clone(),
            },
        )?;

        let item = self
            .transition(
                id,
                State::Review,
                agent_id,
                Some("proposed sibling Tasks — awaiting Approve".into()),
            )
            .map_err(|e| e.to_string())?;

        self.story(
            project_id,
            format!(
                "{} proposed {} sibling Tasks — Approve to create them: {}.",
                card.title,
                specs.len(),
                specs
                    .iter()
                    .map(|t| t.title.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        );
        Ok(item)
    }

    /// Store a TaskProposal on a card (Initial plan or impl split). Does not transition.
    /// Refuses once Initial plan is Done (frozen).
    pub fn set_proposal(&self, id: ItemId, proposal: TaskProposal) -> Result<WorkItem, String> {
        if proposal.tasks.is_empty() {
            return Err("proposal needs at least one task".into());
        }
        for t in &proposal.tasks {
            if t.key.trim().is_empty() {
                return Err(format!("proposal task '{}' needs a key", t.title));
            }
            if t.definition_of_done.trim().is_empty() {
                return Err(format!(
                    "proposal task '{}' needs a definition of done",
                    t.title
                ));
            }
        }
        let item = {
            let mut s = self.state.write();
            let it = s
                .items
                .get_mut(&id)
                .ok_or_else(|| format!("no work item #{id}"))?;
            if it.is_initial_plan_task() && it.state.is_terminal() {
                return Err("Initial plan already accepted — proposal is frozen".into());
            }
            it.proposal = Some(proposal);
            it.clone()
        };
        self.emit(&item);
        Ok(item)
    }

    /// Materialize `item.proposal` into sibling Tasks under the parent Project.
    /// Initial plan: keep proposal and stamp `item_id`s (freeze for briefings).
    /// Impl splits: clear the proposal. Does not transition the parent card.
    ///
    /// Each materialized task carries clone targets in its intent/DoD prose.
    fn materialize_proposal(
        &self,
        id: ItemId,
        by: &str,
        origin: Origin,
    ) -> Result<Vec<WorkItem>, String> {
        let card = self.get(id).ok_or("no such item")?;
        let keep_proposal = card.is_initial_plan_task();
        let proposal = card
            .proposal
            .clone()
            .filter(|p| !p.tasks.is_empty())
            .ok_or_else(|| format!("card #{id} has no proposal to materialize"))?;
        let project_id = card
            .parent
            .ok_or_else(|| "cannot materialize proposal on a Project root".to_string())?;

        let existing_by_title: HashMap<String, WorkItem> = {
            let s = self.state.read();
            s.items
                .values()
                .filter(|i| i.parent == Some(project_id))
                .filter(|i| i.id != id)
                .filter(|i| i.state != State::Retired)
                .map(|i| (Self::normalize_title(&i.title), i.clone()))
                .filter(|(k, _)| !k.is_empty())
                .collect()
        };

        let mut made = Vec::new();
        let mut key_to_id: BTreeMap<String, ItemId> = BTreeMap::new();
        for spec in &proposal.tasks {
            let title_key = Self::normalize_title(&spec.title);
            if let Some(existing) = existing_by_title.get(&title_key) {
                key_to_id.insert(spec.key.clone(), existing.id);
                made.push(existing.clone());
                continue;
            }
            let sibling = self.create(
                Some(project_id),
                spec.title.clone(),
                spec.intent.clone(),
                Some(spec.definition_of_done.clone()),
                origin.clone(),
                false,
                spec.capability.clone().or_else(|| card.capability.clone()),
            )?;
            self.transition(sibling.id, State::Shaping, by, Some("from proposal".into()))
                .map_err(|e| e.to_string())?;
            let sibling = self
                .transition(sibling.id, State::Backlog, by, Some("from proposal".into()))
                .map_err(|e| e.to_string())?;
            key_to_id.insert(spec.key.clone(), sibling.id);
            made.push(sibling);
        }

        for spec in &proposal.tasks {
            let Some(&sid) = key_to_id.get(&spec.key) else {
                continue;
            };
            let blockers: Vec<ItemId> = spec
                .blocked_by_keys
                .iter()
                .filter_map(|k| key_to_id.get(k).copied())
                .collect();
            if !blockers.is_empty() {
                self.set_blocked_by(sid, blockers);
            }
        }

        {
            let mut s = self.state.write();
            if let Some(it) = s.items.get_mut(&id) {
                if keep_proposal {
                    if let Some(prop) = it.proposal.as_mut() {
                        for t in prop.tasks.iter_mut() {
                            if let Some(&sid) = key_to_id.get(&t.key) {
                                t.item_id = Some(sid);
                            }
                        }
                    }
                } else {
                    it.proposal = None;
                }
                let snap = it.clone();
                drop(s);
                self.emit(&snap);
            }
        }

        Ok(made)
    }

    /// Create sibling Tasks from a frozen proposal when a card reaches Done.
    /// Idempotent: title-matched siblings are reused. No-op without a proposal.
    fn materialize_proposal_on_done(&self, id: ItemId, by: &str) -> Result<usize, String> {
        let card = match self.get(id) {
            Some(c) => c,
            None => return Ok(0),
        };
        let has_proposal = card.proposal.as_ref().is_some_and(|p| !p.tasks.is_empty());
        if !has_proposal {
            return Ok(0);
        }
        // Already linked every proposal row to a sibling — nothing to do.
        if card
            .proposal
            .as_ref()
            .is_some_and(|p| p.tasks.iter().all(|t| t.item_id.is_some()))
        {
            return Ok(0);
        }

        let origin = if card.is_initial_plan_task() {
            Origin::Planner
        } else {
            Origin::Split { from: id }
        };
        let made = self.materialize_proposal(id, by, origin)?;

        if card.is_initial_plan_task() {
            if let Some(project_id) = card.parent {
                let mut s = self.state.write();
                if let Some(p) = s.items.get_mut(&project_id) {
                    if p.plan.is_some() {
                        p.plan = None;
                        let snap = p.clone();
                        drop(s);
                        self.emit(&snap);
                    }
                }
            }
        }

        self.story(
            id,
            format!(
                "{} — {} Tasks created from proposal.",
                card.title,
                made.len()
            ),
        );
        Ok(made.len())
    }

    /// Heal: materialize a Done card's proposal if siblings were never created
    /// (e.g. merge-to-Done before materialize-on-Done shipped).
    pub fn materialize_pending_proposal(&self, id: ItemId) -> Result<Vec<WorkItem>, String> {
        let card = self.get(id).ok_or_else(|| format!("no work item #{id}"))?;
        if card.state != State::Done {
            return Err(format!("card #{id} is {:?}; expected Done", card.state));
        }
        let origin = if card.is_initial_plan_task() {
            Origin::Planner
        } else {
            Origin::Split { from: id }
        };
        let made = self.materialize_proposal(id, "operator", origin)?;
        if !made.is_empty() {
            self.story(
                id,
                format!(
                    "{} — {} Tasks created from proposal (heal).",
                    card.title,
                    made.len()
                ),
            );
        }
        Ok(made)
    }

    /// `escalate` — must carry options. An agent that hands back an open
    /// question has transferred the whole problem.
    pub fn escalate(
        &self,
        id: ItemId,
        agent_id: &str,
        question: String,
        options: Vec<EscalationOption>,
        recommended: usize,
    ) -> Result<WorkItem, String> {
        if options.len() < 2 {
            return Err(
                "escalate requires at least two concrete options and a recommendation — an \
                 open-ended question is not a decision a human can make in one tap"
                    .into(),
            );
        }
        if recommended >= options.len() {
            return Err(format!("recommended index {recommended} is out of range"));
        }

        {
            let mut s = self.state.write();
            let it = s.items.get_mut(&id).ok_or("no such item")?;
            it.escalation = Some(Escalation {
                question: question.clone(),
                options,
                recommended,
                blocked_since: Utc::now(),
                answer: None,
            });
            it.rebase_requested = false;
            it.awaiting_dispatch = false;
        }
        let item = self
            .transition(id, State::NeedsHuman, agent_id, Some(question.clone()))
            .map_err(|e| e.to_string())?;
        self.story(id, format!("{} is blocked: {question}", item.title));
        Ok(item)
    }

    /// `report` — agent finished and opened a PR; card goes to Review.
    ///
    /// Mechanical checks are CI on the PR, not a sandboard Verify column. `gates` is
    /// kept as optional agent-side notes only.
    ///
    /// Clears stale bounce UI (`last_bounce_reason` / `last_conflict_files`) —
    /// a successful report means the conflict / infra bounce is no longer the
    /// story the drawer should tell.
    pub fn report(
        &self,
        id: ItemId,
        agent_id: &str,
        added: u32,
        removed: u32,
        gates: Vec<String>,
    ) -> Result<WorkItem, TransitionError> {
        {
            let mut s = self.state.write();
            if let Some(it) = s.items.get_mut(&id) {
                it.diff_added = added;
                it.diff_removed = removed;
                it.progress = 1.0;
                it.last_bounce_reason = None;
                it.last_conflict_files.clear();
                it.gates = gates
                    .into_iter()
                    .map(|name| GateRun {
                        name,
                        status: GateStatus::Passed,
                        detail: Some("CI on the PR is the real gate".into()),
                    })
                    .collect();
            }
        }
        self.transition(id, State::Review, agent_id, None)
    }

    /// `release` — graceful surrender.
    pub fn release(&self, id: ItemId, agent_id: &str) -> Result<WorkItem, TransitionError> {
        self.release_with_reason(id, agent_id, None)
    }

    /// `release_with_reason` — graceful surrender with an explicit bounce reason recorded on WorkItem and transition history.
    pub fn release_with_reason(
        &self,
        id: ItemId,
        agent_id: &str,
        reason: Option<&str>,
    ) -> Result<WorkItem, TransitionError> {
        let reason_str = reason
            .map(|r| r.to_string())
            .unwrap_or_else(|| "released by agent".to_string());

        let item = {
            let mut s = self.state.write();
            if let Some(r) = reason {
                if let Some(it) = s.items.get_mut(&id) {
                    it.last_bounce_reason = Some(r.to_string());
                }
            }
            Self::transition_locked(&mut s, id, State::Backlog, agent_id, Some(reason_str))?
        };
        self.emit(&item);
        if let Some(r) = reason {
            self.story(id, format!("{}: released ({r})", item.title));
        }
        Ok(item)
    }

    /// Requeue runs past their fixed `run_deadline_at` (agent timeout).
    ///
    /// Iterates only Claimed/Running via `ids_by_state` (not a full items scan).
    pub fn sweep_leases(&self) -> Vec<ItemId> {
        let now = Utc::now();
        let expired: Vec<ItemId> = {
            let s = self.state.read();
            let mut out = Vec::new();
            for state in [State::Claimed, State::Running] {
                let Some(ids) = s.ids_by_state.get(&state) else {
                    continue;
                };
                for id in ids {
                    let Some(i) = s.items.get(id) else {
                        continue;
                    };
                    let expired = i
                        .run_deadline_at
                        .map(|d| now > d)
                        .or_else(|| i.lease.as_ref().map(|l| l.is_expired(now)))
                        .unwrap_or(false);
                    if expired {
                        out.push(*id);
                    }
                }
            }
            out
        };
        for id in &expired {
            let title = self.get(*id).map(|i| i.title).unwrap_or_default();
            let _ = self.transition(
                *id,
                State::Backlog,
                "deadline-sweeper",
                Some("run deadline exceeded".into()),
            );
            self.story(
                *id,
                format!("{title}: run deadline exceeded; card requeued."),
            );
        }
        expired
    }

    // ----------------------------------------------------------- human verbs

    /// Steer — free. No restart, no context loss. Without it the only way to
    /// correct a slightly-off agent is to kill it, which makes people hover.
    pub fn steer(&self, id: ItemId, text: String) -> Result<WorkItem, String> {
        let item = {
            let mut s = self.state.write();
            let it = s.items.get_mut(&id).ok_or("no such item")?;
            it.notes.push(Note {
                at: Utc::now(),
                author: "human".into(),
                text,
            });
            it.run_failures = 0;
            it.escalation = None;
            it.clone()
        };
        self.emit(&item);
        Ok(item)
    }

    /// True when the answer promises host-side work before the agent can finish,
    /// without embedding `pr_url=` proof facts yet.
    fn decision_defers_host_prerequisite(choice: &str) -> bool {
        let c = choice.to_ascii_lowercase();
        if c.contains("pr_url=") {
            return false;
        }
        let host_runs = c.contains("host runs")
            || c.contains("finish host")
            || c.contains("on the host board")
            || c.contains("on the host:");
        let reclaim_to_document = (c.contains("re-claim") || c.contains("reclaim"))
            && (c.contains("document") || c.contains("to document"));
        host_runs || (c.contains("host") && reclaim_to_document)
    }

    pub fn answer_escalation(&self, id: ItemId, choice: String) -> Result<WorkItem, String> {
        let title = {
            let mut s = self.state.write();
            let it = s.items.get_mut(&id).ok_or("no such item")?;
            if it.escalation.is_none() {
                return Err("that item is not waiting on anyone".into());
            }
            // Clear it, don't just annotate it. An answered escalation that
            // stays attached keeps rendering as a live question — the card goes
            // on saying "blocked 15m" while the agent is happily working, which
            // is the board lying about the one thing it exists to tell you.
            // The decision is not lost: it becomes a note below, and the
            // transition history records it.
            it.escalation = None;
            // A human has looked at it, so the run budget starts over —
            // otherwise the next single failure would escalate again
            // immediately and the answer would have bought nothing.
            it.run_failures = 0;
            it.last_conflict_files.clear();
            // The answer becomes standing context for whoever picks it up next.
            it.notes.push(Note {
                at: Utc::now(),
                author: "human".into(),
                text: format!("Decision: {choice}"),
            });
            it.title.clone()
        };
        let item = self
            .transition(
                id,
                State::Backlog,
                "human",
                Some(format!("answered: {choice}")),
            )
            .map_err(|e| e.to_string())?;
        self.story(id, format!("{title}: unblocked — {choice}"));
        // "Host runs X; re-claim to document" without Proof facts is a promise,
        // not evidence. Auto mode would reclaim immediately and the agent would
        // re-escalate (#174). Park until operator pastes `Proof: …` and unparks.
        // Already in Backlog — do not call `park()` (Backlog→Backlog is illegal).
        if Self::decision_defers_host_prerequisite(&choice) {
            let reason = "Decision defers a host-side prerequisite — parked until Proof \
facts are pasted, then unpark";
            let item = {
                let mut s = self.state.write();
                let it = s.items.get_mut(&id).ok_or("no such item")?;
                it.parked = true;
                it.awaiting_dispatch = false;
                it.notes.push(Note {
                    at: Utc::now(),
                    author: "human".into(),
                    text: format!("Parked: {reason}"),
                });
                it.clone()
            };
            self.emit(&item);
            self.story(
                id,
                format!("{title}: parked after host-deferred answer — {reason}."),
            );
            return Ok(item);
        }
        Ok(item)
    }

    /// Park — stop the agent, return the card to Backlog, keep sandbox + conversation,
    /// and hold the card until [`Self::unpark`]. Prefer this over halt when the
    /// run is wedged or needs a human nudge without amnesia.
    pub fn park(&self, id: ItemId, reason: Option<String>) -> Result<WorkItem, String> {
        let reason = reason.filter(|r| !r.trim().is_empty());
        let title = {
            let mut s = self.state.write();
            let it = s.items.get_mut(&id).ok_or("no such item")?;
            if let Some(ref r) = reason {
                it.notes.push(Note {
                    at: Utc::now(),
                    author: "human".into(),
                    text: format!("Parked: {r}"),
                });
            }
            it.title.clone()
        };
        self.transition(
            id,
            State::Backlog,
            "human",
            Some(reason.clone().unwrap_or_else(|| "parked".into())),
        )
        .map_err(|e| e.to_string())?;
        let item = {
            let mut s = self.state.write();
            let it = s.items.get_mut(&id).ok_or("no such item")?;
            it.parked = true;
            it.clone()
        };
        self.emit(&item);
        let session = item
            .conversation_id
            .as_deref()
            .map(|c| format!(" session {c} kept"))
            .unwrap_or_default();
        self.story(
            id,
            format!(
                "{title}: parked — agent stopped, sandbox kept;{session} \
                 unpark to resume."
            ),
        );
        Ok(item)
    }

    /// Clear the park hold and queue the card for the supervisor — same as Start.
    /// Park exists to pause without amnesia; making the human click Start again
    /// after Resume is just ceremony.
    pub fn unpark(&self, id: ItemId) -> Result<WorkItem, String> {
        {
            let mut s = self.state.write();
            let it = s.items.get_mut(&id).ok_or("no such item")?;
            if it.state != State::Backlog {
                return Err("only a Backlog card can be resumed from park".into());
            }
            if !it.parked {
                return Err("that card is not parked".into());
            }
            it.parked = false;
        }
        let item = self.enqueue_dispatch(id)?;
        self.story(
            id,
            format!(
                "{}: unparked and queued — supervisor will resume{}.",
                item.title,
                if item.conversation_id.is_some() {
                    " the same conversation"
                } else {
                    ""
                }
            ),
        );
        Ok(item)
    }

    /// Halt — kill the agent, return the card to Backlog, discard the LLM session
    /// and delete the sandbox. Park is the keep-context path; halt starts clean.
    pub fn halt(&self, id: ItemId, reason: Option<String>) -> Result<WorkItem, String> {
        let env_to_delete = {
            let mut s = self.state.write();
            if let Some(it) = s.items.get_mut(&id) {
                it.conversation_id = None;
                it.parked = false;
                // Cleared before Backlog transition so the sweeper will not
                // preserve `sandboard-card-{id}-*` via the non-terminal prefix keep.
                it.environment.take()
            } else {
                None
            }
        };
        let item = self
            .transition(
                id,
                State::Backlog,
                "human",
                reason.or(Some("halted".into())),
            )
            .map_err(|e| e.to_string())?;
        if let Some(env) = env_to_delete {
            let os = self.openshell_client_local();
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                handle.spawn(async move {
                    let _ = os.delete(&env).await;
                });
            }
        }
        self.story(
            item.id,
            format!(
                "{}: halted — session and sandbox discarded; next claim starts clean.",
                item.title
            ),
        );
        Ok(item)
    }

    /// Cut scope — the subtree is retired, not deleted. Archived Projects drop
    /// off the board/digest; items remain in state because "we chose not to"
    /// is a fact you will need later.
    pub fn cut_scope(&self, id: ItemId, reason: Option<String>) -> Result<Vec<ItemId>, String> {
        let mut stack = vec![id];
        let mut touched = Vec::new();
        while let Some(cur) = stack.pop() {
            stack.extend(self.children_of(cur));
            if self
                .transition(cur, State::Retired, "human", reason.clone())
                .is_ok()
            {
                touched.push(cur);
            }
        }
        if let Some(t) = self.get(id) {
            self.story(
                id,
                format!("Scope cut: {} retired ({} items).", t.title, touched.len()),
            );
        }
        Ok(touched)
    }

    /// Inverse of [`Self::cut_scope`]: restore a retired root (archived Project
    /// or mid-tree cut) and its Retired subtree from history.
    ///
    /// Prior Claimed/Running/Splitting/NeedsHuman become Backlog (or Shaping
    /// when a leaf lacks DoD). Leases and `awaiting_dispatch` stay cleared.
    pub fn unarchive_scope(&self, id: ItemId, reason: Option<String>) -> Result<Vec<ItemId>, String> {
        let root = self
            .get(id)
            .ok_or_else(|| format!("no work item #{id}"))?;
        if root.state != State::Retired {
            return Err(format!("#{id} is not retired (state={:?})", root.state));
        }

        // Pre-order walk, then reverse so deepest nodes restore first — parents
        // then see non-retired children when `has_children` / DoD checks run.
        let mut stack = vec![id];
        let mut order = Vec::new();
        while let Some(cur) = stack.pop() {
            order.push(cur);
            stack.extend(self.children_of(cur));
        }
        order.reverse();

        let reason = reason.or_else(|| Some("unarchived".into()));
        let mut touched = Vec::new();
        for cur in order {
            let item = match self.get(cur) {
                Some(i) if i.state == State::Retired => i,
                _ => continue,
            };
            let prior = item
                .history
                .iter()
                .rev()
                .find(|t| t.to == State::Retired)
                .map(|t| t.from)
                .unwrap_or(State::Backlog);
            // Structural children (including still-retired siblings further up
            // the walk) decide leaf vs container for the in-flight remap.
            let has_children = !self.children_of(cur).is_empty();
            let has_dod = item.definition_of_done.is_some();
            let target = machine::unarchive_target(prior, has_children, has_dod);

            self.transition(cur, target, "human", reason.clone())
                .map_err(|e| e.to_string())?;

            // Retired already cleared these; re-assert so restore never revives
            // a lease or auto-dispatch bit from a partial path.
            {
                let mut s = self.state.write();
                if let Some(it) = s.items.get_mut(&cur) {
                    it.lease = None;
                    it.run_deadline_at = None;
                    it.awaiting_dispatch = false;
                }
            }

            touched.push(cur);
        }

        // Match cut_scope's parent-first return order.
        touched.reverse();

        if let Some(t) = self.get(id) {
            self.story(
                id,
                format!(
                    "Scope unarchived: {} restored ({} items).",
                    t.title,
                    touched.len()
                ),
            );
        }
        Ok(touched)
    }

    /// Delete item — removes the item (and its subtree) permanently from the board.
    pub fn delete_item(&self, id: ItemId) -> Result<(), String> {
        // Build the client before taking the write lock — openshell_client reads
        // board state, and parking_lot RwLock is not reentrant.
        let os = self.openshell_client_local();
        let mut s = self.state.write();
        if !s.items.contains_key(&id) {
            return Err(format!("item #{id} not found"));
        }

        let mut to_delete = Vec::new();
        let mut stack = vec![id];
        while let Some(cur) = stack.pop() {
            to_delete.push(cur);
            if let Some(kids) = s.children_by_parent.get(&cur) {
                for &cid in kids {
                    if !to_delete.contains(&cid) && !stack.contains(&cid) {
                        stack.push(cid);
                    }
                }
            }
        }

        for del_id in &to_delete {
            if let Some(it) = s.remove_item(*del_id) {
                let env = it.environment.clone();
                let os = os.clone();
                if let Ok(handle) = tokio::runtime::Handle::try_current() {
                    handle.spawn(async move {
                        if let Some(env_name) = env {
                            let _ = os.delete(&env_name).await;
                        }
                    });
                }
            }
        }

        for item in s.items.values_mut() {
            item.blocked_by.retain(|b| !to_delete.contains(b));
        }

        drop(s);
        self.dirty.store(true, Ordering::Relaxed);
        self.flush();

        self.record_and_send(BoardEvent::Delete {
            seq: self.next_seq(),
            id,
        });
        Ok(())
    }

    pub fn approve_review(&self, id: ItemId) -> Result<WorkItem, String> {
        let item = self.get(id).ok_or_else(|| format!("no work item #{id}"))?;

        let has_proposal = item.proposal.as_ref().is_some_and(|p| !p.tasks.is_empty());

        if has_proposal {
            // UI: "Approve — create Tasks". Materialize now from the proposal.
            let done = self
                .transition(id, State::Done, "human", Some("proposal approved".into()))
                .map_err(|e| e.to_string())?;
            let n = done.proposal.as_ref().map(|p| p.tasks.len()).unwrap_or(0);
            self.story(
                id,
                format!("{} approved — {} Tasks created.", done.title, n),
            );
            return Ok(done);
        }

        // Legacy: Initial plan with Project Plan awaiting but no card proposal.
        if item.is_initial_plan_task() {
            if let Some(parent) = item.parent {
                if let Some(project) = self.get(parent) {
                    let awaiting = project.plan.as_ref().is_some_and(|p| !p.tasks.is_empty());
                    if awaiting {
                        let published = self.approve_plan(parent)?;
                        let done = self.get(id).ok_or_else(|| format!("no work item #{id}"))?;
                        self.story(
                            id,
                            format!(
                                "{} approved — {} Tasks created from legacy Project Plan.",
                                done.title,
                                published.len()
                            ),
                        );
                        return Ok(done);
                    }
                }
            }
        }

        // UI: "Approve & Move to Done". Waiting on the merge webhook alone strands
        // cards when the forwarder missed the event or the PR was already merged
        // while the card was still Running. Webhook → Done stays idempotent.
        let item = self
            .transition(id, State::Done, "human", Some("approved".into()))
            .map_err(|e| e.to_string())?;
        let story = match item.pr_url().filter(|u| !u.trim().is_empty()) {
            Some(url) => format!("{} approved — Done ({}).", item.title, url),
            None => format!("{} approved — no PR; marked Done.", item.title),
        };
        self.story(id, story);
        Ok(item)
    }

    pub fn request_changes(&self, id: ItemId, note: String) -> Result<WorkItem, String> {
        self.steer(id, format!("Changes requested: {note}"))?;
        {
            let mut s = self.state.write();
            if let Some(it) = s.items.get_mut(&id) {
                it.proposal = None;
            }
        }
        let item = self
            .transition(
                id,
                State::Backlog,
                "human",
                Some(format!("changes requested: {note}")),
            )
            .map_err(|e| e.to_string())?;
        self.emit(&item);
        self.story(id, format!("{}: changes requested — {note}", item.title));
        Ok(item)
    }

    // ------------------------------------------------------------ narrative

    pub fn story(&self, near: ItemId, text: String) {
        let (goal, line) = {
            let mut s = self.state.write();
            let goal = Self::goal_of(&s, near);
            let line = StoryLine {
                at: Utc::now(),
                text,
            };
            let entries = s.stories.entry(goal).or_default();
            entries.push(line.clone());
            // A story, not an event log.
            if entries.len() > 200 {
                let excess = entries.len() - 200;
                entries.drain(0..excess);
            }
            (goal, line)
        };
        self.dirty.store(true, Ordering::Relaxed);
        self.record_and_send(BoardEvent::Story {
            seq: self.next_seq(),
            goal,
            at: line.at.to_rfc3339(),
            text: line.text,
        });
    }

    #[allow(dead_code)]
    pub fn stories_for(&self, near: ItemId) -> Vec<StoryLine> {
        let s = self.state.read();
        let goal = Self::goal_of(&s, near);
        s.stories.get(&goal).cloned().unwrap_or_default()
    }

    /// Notify that the default branch advanced (push or merge into main).
    ///
    /// Emits [`BoardEvent::MainAdvanced`]. Live Claimed/Running cards on the
    /// advancing upstream (or unbound cards whose default upstream matches) get
    /// a steer note and park+unpark so the resume briefing carries rebase
    /// instructions. When a card is already `awaiting_dispatch` from a recent
    /// main-advanced steer on the same repo, a second event coalesces: no
    /// park/unpark; the steer note refreshes only when the commit sha changes.
    /// Each steered card also gets a goal-story line naming the advanced ref/sha
    /// and that the live run was interrupted for rebase (distinct from manual park).
    ///
    /// Returns steered card ids from the live path. Callers also run
    /// [`supervisor::process_main_advanced_review_catch_up`] with the advancing
    /// `owner/name` to observe GitHub `mergeable` on scoped Review PRs.
    pub fn notify_main_advanced(
        &self,
        advanced_repo: &str,
        ref_name: &str,
        commit_sha: Option<String>,
    ) -> Vec<ItemId> {
        tracing::info!(
            "main advanced: repo={advanced_repo}, ref={ref_name}, commit={commit_sha:?}"
        );
        self.record_and_send(BoardEvent::MainAdvanced {
            seq: self.next_seq(),
            ref_name: ref_name.to_string(),
            commit_sha: commit_sha.clone(),
        });
        self.steer_live_cards_on_main_advanced(advanced_repo, ref_name, commit_sha.as_deref())
    }

    /// Whether a card's resolved upstream (or unbound default upstream) matches
    /// the repository whose default branch advanced.
    fn card_matches_main_advanced_repo(&self, card_id: ItemId, advanced_repo: &str) -> bool {
        let advanced = advanced_repo.trim();
        if advanced.is_empty() {
            return false;
        }
        match self.resolve_card_repo(card_id).ok().flatten() {
            Some(repo) => repo.upstream == advanced,
            None => {
                let default = self.schema.execution.agents.repo.upstream.trim();
                !default.is_empty() && default == advanced
            }
        }
    }

    /// Binding note for live runs when main moves under them.
    /// Uses the card's resolved base branch when available.
    fn main_advanced_steer_note(ref_name: &str, commit_sha: Option<&str>, base: &str) -> String {
        let where_main = match commit_sha {
            Some(sha) if !sha.is_empty() => format!("{ref_name} @ {sha}"),
            _ => ref_name.to_string(),
        };
        let base = if base.trim().is_empty() {
            "main"
        } else {
            base.trim()
        };
        format!(
            "Main advanced ({where_main}). First action: fetch upstream {base} and rebase \
             this card's branch onto upstream/{base} (not origin/{base} alone — the fork's \
             base freezes at create time), then continue the card."
        )
    }

    /// Goal-story line when main advance auto-steers a live run (distinct from manual park).
    fn main_advanced_steer_story(
        title: &str,
        ref_name: &str,
        commit_sha: Option<&str>,
        base: &str,
    ) -> String {
        let where_main = match commit_sha {
            Some(sha) if !sha.is_empty() => format!("{ref_name} @ {sha}"),
            _ => ref_name.to_string(),
        };
        let base = if base.trim().is_empty() {
            "main"
        } else {
            base.trim()
        };
        format!(
            "{title}: {where_main} advanced — live run interrupted for rebase (auto-steered; \
             fetch upstream {base} and continue)."
        )
    }

    fn is_main_advanced_steer_note(text: &str) -> bool {
        text.starts_with("Main advanced (")
    }

    fn has_main_advanced_steer_note(notes: &[Note]) -> bool {
        notes
            .iter()
            .any(|n| Self::is_main_advanced_steer_note(&n.text))
    }

    fn last_main_advanced_steer_commit_sha(notes: &[Note]) -> Option<String> {
        notes
            .iter()
            .rev()
            .filter(|n| Self::is_main_advanced_steer_note(&n.text))
            .find_map(|n| Self::parse_main_advanced_steer_commit_sha(&n.text))
            .map(str::to_string)
    }

    fn parse_main_advanced_steer_commit_sha(text: &str) -> Option<&str> {
        let rest = text.strip_prefix("Main advanced (")?;
        let end = rest.find(')')?;
        let inside = &rest[..end];
        let (_, sha) = inside.split_once(" @ ")?;
        let sha = sha.trim();
        if sha.is_empty() {
            None
        } else {
            Some(sha)
        }
    }

    fn main_advanced_sha_unchanged(new_sha: Option<&str>, prev_sha: Option<&str>) -> bool {
        fn norm(s: Option<&str>) -> Option<String> {
            s.map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        }
        norm(new_sha) == norm(prev_sha)
    }

    /// Steer scoped Claimed/Running cards, then park+unpark so the resume briefing
    /// carries the rebase instruction. Steer alone does not inject mid-turn.
    /// Already-parked cards are left alone (no second park/unpark). Sandbox
    /// `environment` and `conversation_id` are preserved through the bounce.
    ///
    /// Cards already `awaiting_dispatch` from a prior main-advanced steer on the
    /// same upstream coalesce: skip park/unpark; refresh the steer note only when
    /// the commit sha changes.
    fn steer_live_cards_on_main_advanced(
        &self,
        advanced_repo: &str,
        ref_name: &str,
        commit_sha: Option<&str>,
    ) -> Vec<ItemId> {
        let candidate_ids: Vec<ItemId> = {
            let s = self.state.read();
            s.items
                .values()
                .filter(|i| {
                    matches!(i.state, State::Claimed | State::Running)
                        || (i.state == State::Backlog
                            && i.awaiting_dispatch
                            && !i.parked
                            && Self::has_main_advanced_steer_note(&i.notes))
                })
                .map(|i| i.id)
                .collect()
        };
        let mut steered = Vec::new();
        for id in candidate_ids {
            if !self.card_matches_main_advanced_repo(id, advanced_repo) {
                continue;
            }
            let (title, base) = {
                let s = self.state.read();
                let it = s.items.get(&id).expect("candidate card");
                let base = self
                    .resolve_card_repo(id)
                    .ok()
                    .flatten()
                    .map(|r| r.base)
                    .unwrap_or_else(|| "main".into());
                (it.title.clone(), base)
            };
            let note = Self::main_advanced_steer_note(ref_name, commit_sha, &base);

            let (state, awaiting, parked, prev_sha, has_steer_note) = {
                let s = self.state.read();
                let it = s.items.get(&id).expect("candidate card");
                (
                    it.state,
                    it.awaiting_dispatch,
                    it.parked,
                    Self::last_main_advanced_steer_commit_sha(&it.notes),
                    Self::has_main_advanced_steer_note(&it.notes),
                )
            };

            let coalesced =
                state == State::Backlog && awaiting && !parked && has_steer_note;
            if coalesced {
                if Self::main_advanced_sha_unchanged(commit_sha, prev_sha.as_deref()) {
                    continue;
                }
                if let Err(e) = self.steer(id, note) {
                    tracing::warn!("main-advanced steer refresh failed for #{id}: {e}");
                    continue;
                }
                self.story(
                    id,
                    Self::main_advanced_steer_story(&title, ref_name, commit_sha, &base),
                );
                steered.push(id);
                continue;
            }

            if let Err(e) = self.steer(id, note.clone()) {
                tracing::warn!("main-advanced steer failed for #{id}: {e}");
                continue;
            }
            self.story(
                id,
                Self::main_advanced_steer_story(&title, ref_name, commit_sha, &base),
            );
            steered.push(id);
            // Skip park/unpark for cards already parked — steer may have been a
            // no-op on a weird state, and a second park would be wrong.
            let already_parked = {
                let s = self.state.read();
                s.items.get(&id).is_some_and(|i| i.parked)
            };
            if already_parked {
                continue;
            }
            if let Err(e) = self.park(id, Some("main advanced".into())) {
                tracing::warn!("main-advanced park failed for #{id}: {e}");
                continue;
            }
            if let Err(e) = self.unpark(id) {
                tracing::warn!("main-advanced unpark failed for #{id}: {e}");
            }
        }
        steered
    }

    /// Identify open sibling PRs in Review that are behind main for a given item's parent.
    pub fn identify_behind_sibling_prs(&self, near_id: ItemId) -> Vec<WorkItem> {
        let s = self.state.read();
        let mut results = Vec::new();
        if let Some(item) = s.items.get(&near_id) {
            let parent_id = item.parent.unwrap_or(item.id);
            for child_id in s
                .items
                .values()
                .filter(|i| i.parent == Some(parent_id))
                .map(|i| i.id)
            {
                if child_id == near_id {
                    continue;
                }
                if let Some(child) = s.items.get(&child_id) {
                    if child.state == State::Review && child.pr_url().is_some() {
                        results.push(child.clone());
                    }
                }
            }
        }
        results.sort_by_key(|i| i.entered_state_at);
        results
    }

    /// Identify Review cards with open PRs on the advancing upstream for
    /// tip-driven catch-up after [`Self::notify_main_advanced`].
    pub fn identify_review_prs_for_main_advanced(&self, advanced_repo: &str) -> Vec<WorkItem> {
        self.identify_all_behind_sibling_prs()
            .into_iter()
            .filter(|item| self.card_matches_main_advanced_repo(item.id, advanced_repo))
            .collect()
    }

    /// Identify every Review card with an open PR for tip-driven catch-up.
    ///
    /// Callers pass these to a host GitHub `mergeable` observation after
    /// `MainAdvanced`. MERGEABLE is a no-op; CONFLICTING bounces to Backlog;
    /// UNKNOWN queues [`Self::dispatch_rebase`] for retry. Same-parent sibling
    /// merge uses [`Self::identify_behind_sibling_prs`].
    pub fn identify_all_behind_sibling_prs(&self) -> Vec<WorkItem> {
        let s = self.state.read();
        let mut results: Vec<_> = s
            .items
            .values()
            .filter(|i| i.state == State::Review && i.pr_url().is_some())
            .cloned()
            .collect();
        results.sort_by_key(|i| i.entered_state_at);
        results
    }

    /// Queue a Review card for mergeable-check retry (UNKNOWN / deferred).
    ///
    /// Not used for MERGEABLE — main advanced under a mergeable Review PR is a
    /// no-op with no catch-up work signal.
    pub fn dispatch_rebase(&self, id: ItemId) -> Result<WorkItem, String> {
        let item = {
            let mut s = self.state.write();
            let it = s
                .items
                .get_mut(&id)
                .ok_or_else(|| format!("no such item {id}"))?;
            if it.state != State::Review {
                return Err(format!(
                    "only Review cards can queue mergeable retry, #{id} is in {:?}",
                    it.state
                ));
            }
            if it.pr_url().is_none() {
                return Err(format!("card #{id} has no pull_request.url for mergeable check"));
            }
            if it.rebase_requested {
                let mut out = it.clone();
                Self::populate_blockers(&s, &mut out);
                return Ok(out);
            }
            it.rebase_requested = true;
            it.awaiting_dispatch = true;
            let mut out = it.clone();
            Self::populate_blockers(&s, &mut out);
            out
        };
        self.emit(&item);
        self.story(
            id,
            format!(
                "{}: waiting on GitHub mergeable — catch-up will retry.",
                item.title
            ),
        );
        Ok(item)
    }

    /// Review cards with a pending mergeable-check retry (`rebase_requested`).
    pub fn list_awaiting_rebase(&self) -> Vec<WorkItem> {
        let s = self.state.read();
        let mut items: Vec<_> = s
            .items
            .values()
            .filter(|i| {
                i.state == State::Review && (i.rebase_requested || i.awaiting_dispatch) && !i.parked
            })
            .cloned()
            .collect();
        items.sort_by_key(|i| i.entered_state_at);
        items
    }

    /// Record the outcome of a Review catch-up mergeable observation.
    ///
    /// Clean (MERGEABLE): stay in Review; clear retry flags (silent no-op).
    /// Conflict (CONFLICTING): bounce to Backlog with binding note, or escalate
    /// on repeated overlapping conflict files when those lists are present.
    pub fn record_rebase_outcome(
        &self,
        id: ItemId,
        outcome: RebaseOutcome,
    ) -> Result<WorkItem, String> {
        let (title, previous_files) = {
            let s = self.state.read();
            let it = s
                .items
                .get(&id)
                .ok_or_else(|| format!("no such item #{id}"))?;
            if it.state != State::Review {
                return Err(format!(
                    "only Review cards can record rebase outcome, #{id} is in {:?}",
                    it.state
                ));
            }
            (it.title.clone(), it.last_conflict_files.clone())
        };

        match outcome {
            RebaseOutcome::Clean => {
                let item = {
                    let mut s = self.state.write();
                    let it = s
                        .items
                        .get_mut(&id)
                        .ok_or_else(|| format!("no such item #{id}"))?;
                    it.rebase_requested = false;
                    it.awaiting_dispatch = false;
                    it.last_bounce_reason = None;
                    it.last_conflict_files.clear();
                    let mut out = it.clone();
                    Self::populate_blockers(&s, &mut out);
                    out
                };
                self.emit(&item);
                Ok(item)
            }
            RebaseOutcome::Conflict {
                conflicting_files,
                reason,
            } => {
                let curr_files: Vec<String> = conflicting_files
                    .iter()
                    .map(|f| f.trim().to_string())
                    .collect();
                let has_overlap = !curr_files.is_empty()
                    && !previous_files.is_empty()
                    && curr_files.iter().any(|f| previous_files.contains(f));
                let binding_note = conflict_bounce_note(&curr_files);

                if has_overlap {
                    let overlapping: Vec<String> = curr_files
                        .iter()
                        .filter(|f| previous_files.contains(f))
                        .cloned()
                        .collect();

                    let base_reason =
                        reason.unwrap_or_else(|| "GitHub PR mergeable is CONFLICTING".to_string());
                    let bounce_reason = format!(
                        "{base_reason}: decomposition failure: repeated conflict on overlapping files: {}",
                        overlapping.join(", ")
                    );

                    {
                        let mut s = self.state.write();
                        if let Some(it) = s.items.get_mut(&id) {
                            it.last_bounce_reason = Some(bounce_reason.clone());
                            it.last_conflict_files = curr_files;
                            it.notes.push(Note {
                                at: Utc::now(),
                                author: "rebase".into(),
                                text: binding_note,
                            });
                        }
                    }

                    let question = format!(
                        "Decomposition failure: repeated rebase conflict on overlapping files ({}) for card #{id}",
                        overlapping.join(", ")
                    );

                    let options = vec![
                        EscalationOption {
                            label: "Re-split tasks to isolate overlapping files".into(),
                            detail: "Return card to Shaping/Backlog or re-split the task so overlapping file boundaries are separated.".into(),
                        },
                        EscalationOption {
                            label: "Manually resolve conflict and approve".into(),
                            detail: "Manually rebase or merge the PR branch onto main and approve.".into(),
                        },
                        EscalationOption {
                            label: "Retire card".into(),
                            detail: "Cut scope and retire this card if the conflict cannot be resolved.".into(),
                        },
                    ];

                    self.escalate(id, "rebase", question, options, 0)
                } else {
                    let base_reason =
                        reason.unwrap_or_else(|| "GitHub PR mergeable is CONFLICTING".to_string());
                    let bounce_reason = if curr_files.is_empty() {
                        base_reason
                    } else {
                        format!(
                            "{base_reason}: conflicting files: {}",
                            curr_files.join(", ")
                        )
                    };

                    let item = {
                        let mut s = self.state.write();
                        if let Some(it) = s.items.get_mut(&id) {
                            it.last_bounce_reason = Some(bounce_reason.clone());
                            it.last_conflict_files = curr_files;
                            it.notes.push(Note {
                                at: Utc::now(),
                                author: "rebase".into(),
                                text: binding_note,
                            });
                        }
                        Self::transition_locked(
                            &mut s,
                            id,
                            State::Backlog,
                            "rebase",
                            Some(bounce_reason.clone()),
                        )
                        .map_err(|e| {
                            format!("failed transition to Backlog on rebase conflict: {e}")
                        })?;
                        let it_mut = s.items.get_mut(&id).unwrap();
                        it_mut.rebase_requested = false;
                        it_mut.awaiting_dispatch = false;
                        let mut out = it_mut.clone();
                        Self::populate_blockers(&s, &mut out);
                        out
                    };
                    self.emit(&item);
                    self.story(
                        id,
                        format!(
                            "{title}: GitHub CONFLICTING after main advance — returned to Backlog ({bounce_reason})."
                        ),
                    );
                    Ok(item)
                }
            }
        }
    }

    /// GitHub reports MERGEABLE — clear catch-up retry flags; stay in Review.
    pub fn complete_rebase_clean(&self, id: ItemId) -> Result<WorkItem, String> {
        self.record_rebase_outcome(id, RebaseOutcome::Clean)
    }

    /// GitHub reports CONFLICTING — bounce to Backlog for an agent to rebase.
    pub fn complete_rebase_conflict(
        &self,
        id: ItemId,
        conflicting_files: &[String],
        reason: Option<&str>,
    ) -> Result<WorkItem, String> {
        self.record_rebase_outcome(
            id,
            RebaseOutcome::Conflict {
                conflicting_files: conflicting_files.to_vec(),
                reason: reason.map(|r| r.to_string()),
            },
        )
    }

    /// Normalize a GitHub PR URL for matching board `pr_url` values.
    pub fn normalize_pr_url(url: &str) -> String {
        url.trim().trim_end_matches('/').to_ascii_lowercase()
    }

    /// When a PR merges on GitHub, mark that entry on the matching card.
    /// Done only when every listed PR is merged (gate-like Review).
    /// Returns the completed item id, or `None` if no eligible card matched
    /// or remaining PRs are still open.
    ///
    /// History `by` is `github-webhook` (webhook ingress). Polling uses
    /// [`Self::complete_for_merged_pr_by`] with `github-poll`.
    pub fn complete_for_merged_pr(&self, pr_url: &str, pr_number: Option<u64>) -> Option<ItemId> {
        self.complete_for_merged_pr_by(pr_url, pr_number, "github-webhook")
    }

    fn item_lists_pr(item: &WorkItem, needle: &str) -> bool {
        item.pull_requests.iter().any(|p| {
            p.url_str()
                .is_some_and(|u| Self::normalize_pr_url(u) == needle)
        })
    }

    fn mark_listed_pr_merged(item: &mut WorkItem, needle: &str) -> bool {
        let mut changed = false;
        for p in &mut item.pull_requests {
            if p.url_str()
                .is_some_and(|u| Self::normalize_pr_url(u) == needle)
                && !p.merged
            {
                p.merged = true;
                changed = true;
            }
        }
        changed
    }

    /// Same as [`Self::complete_for_merged_pr`] with an explicit history actor.
    pub fn complete_for_merged_pr_by(
        &self,
        pr_url: &str,
        pr_number: Option<u64>,
        by: &str,
    ) -> Option<ItemId> {
        let needle = Self::normalize_pr_url(pr_url);
        if needle.is_empty() {
            return None;
        }

        let id = {
            let s = self.state.read();
            s.items
                .values()
                .find(|i| {
                    !i.state.is_terminal() && Self::item_lists_pr(i, &needle)
                })
                .map(|i| i.id)?
        };

        let (all_merged, in_review_lane, title) = {
            let mut s = self.state.write();
            let it = s.items.get_mut(&id)?;
            Self::mark_listed_pr_merged(it, &needle);
            let all_merged = it.all_pull_requests_merged();
            let in_review_lane = matches!(it.state, State::Review | State::NeedsHuman);
            let title = it.title.clone();
            let item = it.clone();
            drop(s);
            self.emit(&item);
            (all_merged, in_review_lane, title)
        };

        if !all_merged {
            let remain = self
                .get(id)
                .map(|i| i.unmerged_prs().count())
                .unwrap_or(0);
            self.story(
                id,
                format!(
                    "{title} — PR merged; {remain} still open. Card stays in Review until all listed PRs merge."
                ),
            );
            return None;
        }

        if !in_review_lane {
            return None;
        }

        let reason = match pr_number {
            Some(n) => format!("PR merged (#{n})"),
            None => "PR merged".into(),
        };
        let by = if by.trim().is_empty() {
            "github-webhook"
        } else {
            by.trim()
        };
        match self.transition(id, State::Done, by, Some(reason)) {
            Ok(item) => {
                self.story(id, format!("{} — all listed PRs merged; card Done.", item.title));
                Some(id)
            }
            Err(e) => {
                tracing::warn!(id, error = %e, "complete_for_merged_pr transition failed");
                None
            }
        }
    }

    /// True when a submitted GitHub PR review should steer the matching card.
    /// `CHANGES_REQUESTED` and `COMMENT`/`COMMENTED` share one Board path;
    /// `APPROVED` / dismissed (and anything else) are no-ops.
    pub fn is_actionable_pr_review_state(state: &str) -> bool {
        matches!(
            state.trim().to_ascii_uppercase().as_str(),
            "CHANGES_REQUESTED" | "COMMENT" | "COMMENTED"
        )
    }

    /// Pointer steer note for forge PR review feedback — identity only, no body.
    fn pr_review_feedback_steer_note(pr_url: &str, pr_number: Option<u64>) -> String {
        let identity = match pr_number {
            Some(n) => format!("{pr_url} (#{n})"),
            None => pr_url.to_string(),
        };
        format!(
            "There is PR review feedback on {identity}. Inspect it with gh \
             (e.g. `gh pr view` / reviews); figure out the rest from the review itself."
        )
    }

    /// When GitHub submits PR review feedback (`CHANGES_REQUESTED` or `COMMENT`),
    /// steer the matching card and move it to Backlog — same treatment as human
    /// [`Self::request_changes`], without embedding the review body.
    ///
    /// Matches Review, NeedsHuman, Claimed, and Running cards by normalized PR
    /// URL (same matching as merge completion). `APPROVED` / dismissed → no-op.
    /// Idempotent: already-Backlog cards are not matched again.
    ///
    /// History `by` defaults to `github-review` (webhook). Polling should use
    /// [`Self::apply_pr_review_feedback_by`] with `github-poll`.
    pub fn apply_pr_review_feedback(
        &self,
        pr_url: &str,
        pr_number: Option<u64>,
        review_state: &str,
    ) -> Option<ItemId> {
        self.apply_pr_review_feedback_by(pr_url, pr_number, review_state, "github-review")
    }

    /// Same as [`Self::apply_pr_review_feedback`] with an explicit history actor.
    pub fn apply_pr_review_feedback_by(
        &self,
        pr_url: &str,
        pr_number: Option<u64>,
        review_state: &str,
        by: &str,
    ) -> Option<ItemId> {
        if !Self::is_actionable_pr_review_state(review_state) {
            return None;
        }

        let needle = Self::normalize_pr_url(pr_url);
        if needle.is_empty() {
            return None;
        }

        let id = {
            let s = self.state.read();
            s.items
                .values()
                .find(|i| {
                    matches!(
                        i.state,
                        State::Review | State::NeedsHuman | State::Claimed | State::Running
                    ) && Self::item_lists_pr(i, &needle)
                })
                .map(|i| i.id)?
        };

        let note = Self::pr_review_feedback_steer_note(pr_url.trim(), pr_number);
        let by = if by.trim().is_empty() {
            "github-review"
        } else {
            by.trim()
        };

        // Mirror request_changes: steer note, clear proposal, → Backlog, story.
        {
            let mut s = self.state.write();
            let it = s.items.get_mut(&id)?;
            it.notes.push(Note {
                at: Utc::now(),
                author: by.into(),
                text: note.clone(),
            });
            it.run_failures = 0;
            it.escalation = None;
            it.proposal = None;
        }

        let reason = match pr_number {
            Some(n) => format!("PR review feedback (#{n})"),
            None => "PR review feedback".into(),
        };
        match self.transition(id, State::Backlog, by, Some(reason)) {
            Ok(item) => {
                self.emit(&item);
                self.story(
                    id,
                    format!("{}: PR review feedback — Backlog ({})", item.title, note),
                );
                Some(id)
            }
            Err(e) => {
                tracing::warn!(id, error = %e, "apply_pr_review_feedback transition failed");
                None
            }
        }
    }

    // -------------------------------------------------------- derived reads

    /// Returns active (non-terminal) siblings of `id` (sharing the same parent)
    /// that were blocked by `id` and have now become unblocked (0 unresolved blockers).
    pub fn newly_unblocked_siblings(&self, id: ItemId) -> Vec<WorkItem> {
        let s = self.state.read();
        let Some(item) = s.items.get(&id) else {
            return vec![];
        };
        s.items
            .values()
            .filter(|other| {
                other.id != id
                    && other.parent == item.parent
                    && !other.state.is_terminal()
                    && other.blocked_by.contains(&id)
                    && Self::unresolved_blockers(&s, other).is_empty()
            })
            .cloned()
            .collect()
    }

    pub fn snapshot(&self) -> Snapshot {
        let s = self.state.read();
        let now = Utc::now();
        let mut items: Vec<WorkItem> = s
            .items
            .values()
            .map(|i| {
                let mut item = i.clone();
                Self::populate_blockers(&s, &mut item);
                item
            })
            .collect();

        // Project roots only (parent.is_none) — avoids goal_of over every item.
        let mut goal_ids: Vec<ItemId> = s
            .items
            .values()
            .filter(|i| i.parent.is_none())
            .map(|i| i.id)
            .collect();
        goal_ids.sort_unstable();

        let goals = goal_ids
            .into_iter()
            .filter_map(|gid| self.goal_view(&s, gid, now))
            .collect();

        // Overlay from `s` — do not call effective_agents()/agent_runtime()
        // while this read guard is held (RwLock is not reentrant; that freeze
        // made the UI show NOT LIVE after Agent runtime landed).
        let agents =
            Self::overlay_agent_runtime(&self.schema.execution.agents, s.agent_runtime.as_ref());
        drop(s);
        // Card face engine/model come from resolution, not stale WorkItem fields.
        for item in &mut items {
            item.engine = Some(self.resolve_engine_for_card(item.id));
            item.resolved_model = self.resolve_model_for_card(item.id);
        }
        Snapshot {
            items,
            levels: self.schema.levels.clone(),
            goals,
            server_time: now,
            agent_timeout_secs: agents.agent_timeout_secs,
            seq: self.seq.load(Ordering::Relaxed),
            default_engine: agents.engine,
            // Model selection moves to OpenShell-in-sandboard config; keep empty for UI.
            default_model: String::new(),
        }
    }

    fn goal_view(&self, s: &BoardState, gid: ItemId, now: DateTime<Utc>) -> Option<GoalView> {
        let goal = s.items.get(&gid)?;

        // Only Project roots are swimlanes. Nested nodes never get their own.
        if goal.parent.is_some() {
            return None;
        }
        let archived = goal.state == State::Retired;

        // Tasks under this Project — via children_by_parent (not a full scan).
        let member_ids = Self::children_of_indexed(s, gid);
        let members: Vec<&WorkItem> = member_ids.iter().filter_map(|id| s.items.get(id)).collect();

        // Active projects: retired leaves are out of scope (cut duplicates must
        // not inflate the bar). Archived projects: cut_scope retires the whole
        // subtree, so count every leaf — the scope is closed.
        let leaves: Vec<&&WorkItem> = members
            .iter()
            .filter(|i| archived || i.state != State::Retired)
            .filter(|i| !Self::has_children(s, i.id))
            .collect();
        let leaves_total = leaves.len();
        let leaves_done = if archived {
            leaves_total
        } else {
            leaves.iter().filter(|i| i.state == State::Done).count()
        };

        let agents_live = members
            .iter()
            .filter(|i| matches!(i.state, State::Claimed | State::Running | State::Splitting))
            .count();
        let needs_you = members
            .iter()
            .filter(|i| i.state == State::NeedsHuman)
            .count();

        let mut columns = Vec::new();
        for column in [
            Column::Backlog,
            Column::Running,
            Column::NeedsYou,
            Column::Review,
            Column::Done,
        ] {
            let in_col: Vec<&&WorkItem> = members
                .iter()
                .filter(|i| i.state.column() == column)
                .collect();
            columns.push(ColumnView {
                column,
                summary: Self::chunk(
                    column,
                    &in_col,
                    s,
                    now,
                    self.schema.execution.agents.agent_timeout_secs,
                ),
            });
        }

        let plan_status = Self::plan_status_label(s, gid);
        // Active Projects only — archived trees are already "scope cut" as a unit.
        let recent_retired = if archived {
            Vec::new()
        } else {
            Self::recent_retired_of(&members, 5)
        };

        Some(GoalView {
            id: gid,
            title: goal.title.clone(),
            intent: goal.intent.clone(),
            progress: if leaves_total == 0 {
                0.0
            } else {
                leaves_done as f32 / leaves_total as f32
            },
            leaves_done,
            leaves_total,
            agents_live,
            needs_you,
            auto_dispatch: goal.auto_dispatch,
            plan_status,
            archived,
            columns,
            story: s.stories.get(&gid).cloned().unwrap_or_default(),
            recent_retired,
        })
    }

    /// Compression makes less stuff; chunking makes stuff that holds. Every
    /// rollup here has to answer the column's question, not just count it.
    fn chunk(
        column: Column,
        items: &[&&WorkItem],
        s: &BoardState,
        now: DateTime<Utc>,
        agent_timeout_secs: u64,
    ) -> ChunkSummary {
        let count = items.len();
        if count == 0 {
            return ChunkSummary {
                count,
                text: "empty".into(),
            };
        }
        let oldest = items
            .iter()
            .map(|i| {
                if column == Column::Review {
                    i.oldest_unmerged_age(now)
                } else {
                    i.time_in_state(now)
                }
            })
            .max()
            .unwrap_or_else(Duration::zero);

        let text = match column {
            Column::Backlog => {
                // Waiting for operator to dispatch — not a claim queue.
                let blocked: Vec<&&&WorkItem> = items
                    .iter()
                    .filter(|i| !Self::unresolved_blockers(s, i).is_empty())
                    .collect();
                let mut parts = vec![format!("{count} in backlog")];
                if !blocked.is_empty() {
                    let mut blocker_ids: Vec<ItemId> = blocked
                        .iter()
                        .flat_map(|i| Self::unresolved_blockers(s, i))
                        .collect();
                    blocker_ids.sort_unstable();
                    blocker_ids.dedup();

                    let on: Vec<String> = blocker_ids
                        .into_iter()
                        .map(|bid| {
                            if let Some(b_item) = s.items.get(&bid) {
                                let label = format!("#{bid}");
                                let title = b_item.title.trim();
                                if title.is_empty() {
                                    label
                                } else {
                                    format!("{label}: {title}")
                                }
                            } else {
                                format!("#{bid}")
                            }
                        })
                        .collect();
                    parts.push(format!("{} blocked on {}", blocked.len(), on.join(", ")));
                }
                parts.push(format!("oldest {}", humanize(oldest)));
                parts.join(" · ")
            }
            Column::Running => {
                // Near deadline = remaining under 10% of the agent timeout.
                let warn_secs = (agent_timeout_secs as i64).max(1) / 10;
                let ending_soon = items
                    .iter()
                    .filter(|i| {
                        i.run_deadline_at
                            .or_else(|| i.lease.as_ref().map(|l| l.expires_at))
                            .map(|d| (d - now).num_seconds() <= warn_secs)
                            .unwrap_or(true)
                    })
                    .count();
                if ending_soon == 0 {
                    format!("{count} running")
                } else {
                    format!("{count} running · {ending_soon} ending soon")
                }
            }
            Column::NeedsYou => {
                // How fast must I act?
                let longest = items
                    .iter()
                    .filter_map(|i| i.escalation.as_ref())
                    .map(|e| e.blocked_secs(now))
                    .max()
                    .unwrap_or(0);
                format!(
                    "{count} blocked on you · longest {}",
                    humanize(Duration::seconds(longest))
                )
            }
            Column::Review => {
                // Can I approve this in 30 seconds? CI is on the PR, not here.
                let added: u32 = items.iter().map(|i| i.diff_added).sum();
                let removed: u32 = items.iter().map(|i| i.diff_removed).sum();
                format!(
                    "{count} awaiting review · +{added} −{removed} · oldest {}",
                    humanize(oldest)
                )
            }
            Column::Done => format!("{count} merged"),
            _ => format!("{count} items"),
        };
        ChunkSummary { count, text }
    }

    /// The primary interface for most sessions. Two taps to resolve both
    /// blockers from a phone is the actual product; the board is where you go
    /// when something has gone wrong.
    pub fn digest(&self) -> Digest {
        let s = self.state.read();
        let now = Utc::now();
        // Project roots only — avoids goal_of over every item.
        let mut goal_ids: Vec<ItemId> = s
            .items
            .values()
            .filter(|i| i.parent.is_none())
            .map(|i| i.id)
            .collect();
        goal_ids.sort_unstable();

        let goals = goal_ids
            .into_iter()
            .filter_map(|gid| {
                let goal = s.items.get(&gid)?;
                if goal.parent.is_some() {
                    return None;
                }
                if goal.state == State::Retired {
                    return None;
                }
                // Flat Tasks under Project + the Project itself (legacy goal_of set).
                let child_ids = Self::children_of_indexed(&s, gid);
                let mut members: Vec<&WorkItem> =
                    child_ids.iter().filter_map(|id| s.items.get(id)).collect();
                members.push(goal);

                let needs_you = members
                    .iter()
                    .filter(|i| i.state == State::NeedsHuman)
                    .filter_map(|i| {
                        i.escalation.as_ref().map(|e| NeedsYou {
                            id: i.id,
                            title: i.title.clone(),
                            question: e.question.clone(),
                            options: e.options.iter().map(|o| o.label.clone()).collect(),
                            recommended: e.recommended,
                            blocked_secs: e.blocked_secs(now),
                        })
                    })
                    .collect();

                let running: Vec<&&WorkItem> = members
                    .iter()
                    .filter(|i| {
                        matches!(i.state, State::Claimed | State::Running | State::Splitting)
                    })
                    .collect();
                let warn_secs =
                    (self.schema.execution.agents.agent_timeout_secs as i64).max(1) / 10;

                let mut ready_items: Vec<&&WorkItem> = members
                    .iter()
                    .filter(|i| i.state == State::Backlog)
                    .filter(|i| i.level.as_deref() != Some("Project"))
                    .filter(|i| !Self::has_children(&s, i.id))
                    .filter(|i| Self::unresolved_blockers(&s, i).is_empty())
                    .filter(|i| !i.parked)
                    .filter(|i| !i.awaiting_dispatch)
                    .collect();
                ready_items.sort_by_key(|i| i.entered_state_at);
                let ready_to_dispatch = ready_items
                    .into_iter()
                    .map(|i| ReadyCard {
                        id: i.id,
                        title: i.title.clone(),
                    })
                    .collect();

                let recently_retired = Self::recent_retired_of(&members, 5);

                Some(GoalDigest {
                    goal_id: gid,
                    goal: goal.title.clone(),
                    merged: members.iter().filter(|i| i.state == State::Done).count(),
                    needs_you,
                    running: running.len(),
                    running_stalled: running
                        .iter()
                        .filter(|i| {
                            i.run_deadline_at
                                .or_else(|| i.lease.as_ref().map(|l| l.expires_at))
                                .map(|d| (d - now).num_seconds() <= warn_secs)
                                .unwrap_or(true)
                        })
                        .count(),
                    backlog: members.iter().filter(|i| i.state == State::Backlog).count(),
                    in_review: members.iter().filter(|i| i.state == State::Review).count(),
                    ready_to_dispatch,
                    latest_story: s
                        .stories
                        .get(&gid)
                        .and_then(|v| v.last())
                        .map(|l| l.text.clone()),
                    recently_retired,
                })
            })
            .collect();

        Digest {
            since: self.started_at,
            goals,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AgentRuntimeConfig, Origin};

    /// Poll until `pred` succeeds. Prefer this over multi-second sleep loops.
    async fn eventually(mut pred: impl FnMut() -> bool) {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        while tokio::time::Instant::now() < deadline {
            if pred() {
                return;
            }
            tokio::task::yield_now().await;
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
    }

    /// Complete Task repo used by tests that need an Initial plan after create.
    fn test_task_repo() -> RepoConfig {
        RepoConfig {
            upstream: "acme/widgets".into(),
            fork: String::new(),
            base: "main".into(),
        }
    }

    /// Project + auto-seeded Initial plan.
    fn project_with_initial_plan(b: &Board, title: &str) -> (WorkItem, WorkItem) {
        let project = b
            .create(None, title, "why", None, Origin::Human, true, None)
            .expect("project");
        let seed = b
            .initial_plan_of(project.id)
            .expect("auto-seeded Initial plan");
        (project, seed)
    }

    /// A board with one leaf sitting in Backlog, claimed by `agent`.
    fn claimed_leaf() -> (Board, ItemId) {
        let b = Board::new(
            Schema::default(),
            std::env::temp_dir().join("sandboard-test-nowrite.json"),
        );
        let parent = b
            .create(None, "goal", "why", None, Origin::Human, true, None)
            .expect("project");
        let _ = b.transition(parent.id, State::Shaping, "t", None);
        let leaf = b
            .create(
                Some(parent.id),
                "leaf",
                "do a thing",
                Some("it is done".into()),
                Origin::Human,
                false,
                None,
            )
            .expect("task");
        b.set_task_repo(leaf.id, Some(test_task_repo()))
            .expect("bind leaf repo");
        let _ = b.transition(leaf.id, State::Shaping, "t", None);
        let _ = b.transition(leaf.id, State::Backlog, "t", None);
        b.claim(leaf.id, "agent", None, 45).expect("claim");
        (b, leaf.id)
    }

    #[test]
    fn claim_sets_run_deadline_from_timeout() {
        let b = Board::new(
            Schema::default(),
            std::env::temp_dir().join("sandboard-test-deadline.json"),
        );
        let parent = b
            .create(None, "goal", "why", None, Origin::Human, true, None)
            .expect("project");
        let _ = b.transition(parent.id, State::Shaping, "t", None);
        let leaf = b
            .create(
                Some(parent.id),
                "leaf",
                "do a thing",
                Some("it is done".into()),
                Origin::Human,
                false,
                None,
            )
            .expect("task");
        let _ = b.transition(leaf.id, State::Shaping, "t", None);
        let _ = b.transition(leaf.id, State::Backlog, "t", None);
        let before = Utc::now();
        let grant = b.claim(leaf.id, "agent", None, 1800).expect("claim");
        let after = Utc::now();
        let item = b.get(leaf.id).unwrap();
        let deadline = item.run_deadline_at.expect("run_deadline_at set");
        assert_eq!(grant.run_deadline_at, deadline);
        assert_eq!(grant.lease_expires_at, deadline);
        assert!(deadline >= before + Duration::seconds(1799));
        assert!(deadline <= after + Duration::seconds(1801));
    }

    #[test]
    fn heartbeat_does_not_extend_run_deadline() {
        let (b, id) = claimed_leaf();
        let original = b.get(id).unwrap().run_deadline_at.expect("deadline");
        b.heartbeat(id, "agent", 0.5, 9999).expect("heartbeat");
        let after = b.get(id).unwrap();
        assert_eq!(after.run_deadline_at, Some(original));
        assert_eq!(
            after.lease.as_ref().map(|l| l.expires_at),
            Some(original),
            "lease.expires_at must stay pinned to the claim deadline"
        );
        assert_eq!(after.progress, 0.5);
        assert_eq!(after.state, State::Running);
    }

    #[test]
    fn quiet_heartbeat_while_running_does_not_emit_upsert() {
        let (b, id) = claimed_leaf();
        let mut rx = b.subscribe();
        // Claimed → Running emits once.
        b.heartbeat(id, "agent", 0.25, 0).expect("promote");
        let first = rx.try_recv().expect("Claimed→Running upsert");
        assert!(matches!(first, BoardEvent::Upsert { .. }));
        while rx.try_recv().is_ok() {}

        // Same progress, already Running: stamp liveness only.
        b.heartbeat(id, "agent", 0.25, 0).expect("quiet");
        assert!(
            rx.try_recv().is_err(),
            "quiet heartbeat must not Upsert (drawer lock storm)"
        );
        let hb = b
            .get(id)
            .unwrap()
            .lease
            .as_ref()
            .map(|l| l.last_heartbeat)
            .expect("lease");
        assert!(hb <= Utc::now());

        // Progress change still notifies.
        b.heartbeat(id, "agent", 0.5, 0).expect("progress");
        assert!(matches!(
            rx.try_recv().expect("progress upsert"),
            BoardEvent::Upsert { .. }
        ));
    }

    #[test]
    fn agent_log_ring_lives_off_the_board_state_lock() {
        let (b, id) = claimed_leaf();
        for i in 0..10 {
            b.append_agent_log(id, format!("line {i}"));
        }
        let logs = b.get_agent_logs(id);
        assert_eq!(logs.len(), 10);
        assert_eq!(logs[0], "line 0");
        b.clear_agent_logs(id);
        assert!(b.get_agent_logs(id).is_empty());
    }

    #[test]
    fn sweep_requeues_past_run_deadline() {
        let (b, id) = claimed_leaf();
        {
            let mut s = b.state.write();
            let it = s.items.get_mut(&id).unwrap();
            it.run_deadline_at = Some(Utc::now() - Duration::seconds(1));
            if let Some(l) = it.lease.as_mut() {
                l.expires_at = Utc::now() - Duration::seconds(1);
            }
        }
        let expired = b.sweep_leases();
        assert_eq!(expired, vec![id]);
        assert_eq!(b.get(id).unwrap().state, State::Backlog);
        assert!(b.get(id).unwrap().run_deadline_at.is_none());
    }

    #[test]
    fn park_keeps_conversation_and_environment() {
        let (b, id) = claimed_leaf();
        b.set_environment(id, Some("sandboard-card-1-a1".into()));
        b.set_conversation_id(id, Some("conv-xyz".into()));
        let it = b.park(id, Some("wedged on cargo".into())).expect("park");
        assert_eq!(it.state, State::Backlog);
        assert_eq!(it.environment.as_deref(), Some("sandboard-card-1-a1"));
        assert_eq!(it.conversation_id.as_deref(), Some("conv-xyz"));
        assert!(it.parked, "park must hold the card from reclaim");
        assert!(!b.may_claim(id), "parked card must not be claimable");
        assert!(
            it.notes
                .iter()
                .any(|n| n.text.contains("Parked: wedged on cargo")),
            "park reason must become a resume note: {:?}",
            it.notes
        );
        let resumed = b.unpark(id).expect("unpark");
        assert!(!resumed.parked);
        assert!(
            resumed.awaiting_dispatch,
            "unpark should queue the supervisor (same as Start)"
        );
        assert!(b.may_claim(id), "unpark restores claimability");
    }

    #[test]
    fn cockpit_session_create_update_park_resume_stop_invariants() {
        let b = Board::new(
            Schema::default(),
            std::env::temp_dir().join("sandboard-test-cockpit-session.json"),
        );
        assert!(b.cockpit_session().is_none());

        let created = b
            .create_cockpit_session(Some("sandboard-cockpit".into()), None)
            .expect("create");
        assert_eq!(created.environment.as_deref(), Some("sandboard-cockpit"));
        assert!(created.conversation_id.is_none());
        assert_eq!(created.status, CockpitSessionStatus::Running);
        assert_eq!(created.sandbox_phase, CockpitSandboxPhase::Ready);

        b.stop_cockpit_session().expect("clear for starting-phase create");
        let starting = b.create_cockpit_session(None, None).expect("start bare");
        assert_eq!(starting.sandbox_phase, CockpitSandboxPhase::Starting);
        let phased = b
            .set_cockpit_sandbox_phase(
                CockpitSandboxPhase::WaitingForDelete,
                Some("Waiting for previous sandbox to finish deleting".into()),
            )
            .expect("phase");
        assert_eq!(phased.sandbox_phase, CockpitSandboxPhase::WaitingForDelete);
        assert!(phased
            .phase_detail
            .as_deref()
            .unwrap_or("")
            .contains("previous sandbox"));
        b.stop_cockpit_session().expect("clear again");
        let created = b
            .create_cockpit_session(Some("sandboard-cockpit".into()), None)
            .expect("create with env");
        assert_eq!(created.sandbox_phase, CockpitSandboxPhase::Ready);

        let err = b
            .create_cockpit_session(None, None)
            .expect_err("second create must fail");
        assert!(err.contains("already exists"), "{err}");

        let updated = b
            .update_cockpit_session(None, Some("conv-cockpit-1".into()))
            .expect("set conversation");
        assert_eq!(updated.environment.as_deref(), Some("sandboard-cockpit"));
        assert_eq!(updated.conversation_id.as_deref(), Some("conv-cockpit-1"));
        assert_eq!(updated.status, CockpitSessionStatus::Running);

        // Blank clears a field; omitted (None) leaves the other alone.
        let cleared_env = b
            .update_cockpit_session(Some("  ".into()), None)
            .expect("clear env");
        assert!(cleared_env.environment.is_none());
        assert_eq!(
            cleared_env.conversation_id.as_deref(),
            Some("conv-cockpit-1")
        );

        b.update_cockpit_session(Some("sandboard-cockpit".into()), None)
            .expect("restore env");

        let parked = b.park_cockpit_session().expect("park");
        assert_eq!(parked.status, CockpitSessionStatus::Parked);
        assert_eq!(parked.environment.as_deref(), Some("sandboard-cockpit"));
        assert_eq!(parked.conversation_id.as_deref(), Some("conv-cockpit-1"));
        let err = b.park_cockpit_session().expect_err("already parked");
        assert!(err.contains("already parked"), "{err}");

        let resumed = b.resume_cockpit_session().expect("resume");
        assert_eq!(resumed.status, CockpitSessionStatus::Running);
        assert_eq!(resumed.environment.as_deref(), Some("sandboard-cockpit"));
        assert_eq!(resumed.conversation_id.as_deref(), Some("conv-cockpit-1"));
        let err = b.resume_cockpit_session().expect_err("not parked");
        assert!(err.contains("not parked"), "{err}");

        b.stop_cockpit_session().expect("stop");
        assert!(b.cockpit_session().is_none());
        b.stop_cockpit_session().expect("stop is idempotent");

        // After stop, create works again — not card claim/report lifecycle.
        let again = b
            .create_cockpit_session(Some("sandboard-cockpit-2".into()), Some("conv-2".into()))
            .expect("recreate");
        assert_eq!(again.environment.as_deref(), Some("sandboard-cockpit-2"));
        assert_eq!(again.conversation_id.as_deref(), Some("conv-2"));
    }

    #[test]
    fn resolve_cockpit_sandbox_create_uses_cockpit_profile_preference() {
        let b = Board::new(
            Schema::default(),
            std::env::temp_dir().join(format!("sandboard-test-resolve-cockpit-{}", std::process::id())),
        );
        upsert_test_profile(&b, "default", "Default", "seed-image:test");
        assert!(
            b.cockpit_sandbox_profile_id().is_none(),
            "Cockpit inherits default until an override is set"
        );
        let inherited = b.resolve_cockpit_sandbox_create();
        assert_eq!(inherited.profile_id.as_deref(), Some("default"));

        upsert_test_profile(&b, "heavy", "Heavy", "heavy:1");
        b.set_cockpit_sandbox_profile("heavy")
            .expect("point Cockpit at heavy");
        let resolved = b.resolve_cockpit_sandbox_create();
        assert_eq!(resolved.profile_id.as_deref(), Some("heavy"));
        assert_eq!(resolved.image, "heavy:1");
    }

    #[test]
    fn resolve_cockpit_engine_prefers_profile_over_runtime() {
        let b = Board::new(
            Schema::default(),
            std::env::temp_dir().join(format!(
                "sandboard-test-resolve-cockpit-engine-{}",
                std::process::id()
            )),
        );
        upsert_test_profile(&b, "default", "Default", "seed-image:test");
        b.set_agent_runtime(crate::model::AgentRuntimeConfig {
            engine: "cursor".into(),
            ..Default::default()
        });
        let mut profile = b.get_sandbox_profile("default").expect("default");
        profile.engine = Some("opencode".into());
        b.upsert_sandbox_profile(profile).expect("save");
        assert_eq!(b.resolve_cockpit_engine(), "opencode");
    }

    #[test]
    fn cockpit_session_update_requires_existing_session() {
        let b = Board::new(
            Schema::default(),
            std::env::temp_dir().join("sandboard-test-cockpit-session-missing.json"),
        );
        let err = b
            .update_cockpit_session(Some("x".into()), None)
            .expect_err("no session");
        assert!(err.contains("no cockpit session"), "{err}");
        let err = b.park_cockpit_session().expect_err("no session");
        assert!(err.contains("no cockpit session"), "{err}");
    }

    #[test]
    fn cockpit_session_round_trips_json_flush_load() {
        let path = std::env::temp_dir().join(format!(
            "sandboard-test-cockpit-session-json-{}.json",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let b = Board::new(Schema::default(), path.clone());
        b.create_cockpit_session(Some("sandboard-cockpit".into()), Some("conv-a".into()))
            .expect("create");
        b.park_cockpit_session().expect("park");
        b.dirty.store(true, Ordering::Relaxed);
        b.flush();

        let raw = std::fs::read_to_string(&path).expect("read board json");
        let state: BoardState = serde_json::from_str(&raw).expect("parse");
        assert_eq!(
            state
                .cockpit_session
                .as_ref()
                .map(|s| s.environment.as_deref()),
            Some(Some("sandboard-cockpit"))
        );
        assert_eq!(
            state
                .cockpit_session
                .as_ref()
                .map(|s| s.conversation_id.as_deref()),
            Some(Some("conv-a"))
        );
        assert_eq!(
            state.cockpit_session.as_ref().map(|s| s.status),
            Some(CockpitSessionStatus::Parked)
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn enqueue_dispatch_marks_card_for_supervisor() {
        let b = Board::new(
            Schema::default(),
            std::env::temp_dir().join("sandboard-test-dispatch.json"),
        );
        let parent = b
            .create(None, "goal", "why", None, Origin::Human, true, None)
            .expect("project");
        let _ = b.transition(parent.id, State::Shaping, "t", None);
        let leaf = b
            .create(
                Some(parent.id),
                "leaf",
                "do a thing",
                Some("it is done".into()),
                Origin::Human,
                false,
                None,
            )
            .expect("task");
        let _ = b.transition(leaf.id, State::Shaping, "t", None);
        let _ = b.transition(leaf.id, State::Backlog, "t", None);
        assert!(b.list_awaiting_dispatch().is_empty());
        let it = b.enqueue_dispatch(leaf.id).expect("enqueue");
        assert!(it.awaiting_dispatch);
        assert_eq!(b.list_awaiting_dispatch().len(), 1);
        b.claim(leaf.id, "agent", None, 45).expect("claim");
        assert!(
            !b.get(leaf.id).unwrap().awaiting_dispatch,
            "claim clears the dispatch flag"
        );
        b.halt(leaf.id, None).expect("halt");
        assert!(
            !b.get(leaf.id).unwrap().awaiting_dispatch,
            "halt bounce clears dispatch"
        );
        assert!(b.list_awaiting_dispatch().is_empty());
    }

    /// Filter semantics for hot paths that use denorm / secondary indexes.
    #[test]
    fn hot_path_filters_match_legacy_semantics() {
        let b = Board::new(
            Schema::default(),
            std::env::temp_dir().join("sandboard-test-hot-filters.json"),
        );
        let project = b
            .create(None, "Proj", "why", None, Origin::Human, true, None)
            .expect("project");
        let _ = b.transition(project.id, State::Shaping, "t", None);

        let leaf = b
            .create(
                Some(project.id),
                "leaf",
                "do",
                Some("done".into()),
                Origin::Human,
                false,
                Some("rust".into()),
            )
            .expect("leaf");
        let _ = b.transition(leaf.id, State::Shaping, "t", None);
        let _ = b.transition(leaf.id, State::Backlog, "t", None);

        let blocked = b
            .create(
                Some(project.id),
                "blocked",
                "wait",
                Some("done".into()),
                Origin::Human,
                false,
                None,
            )
            .expect("blocked");
        let _ = b.transition(blocked.id, State::Shaping, "t", None);
        let _ = b.transition(blocked.id, State::Backlog, "t", None);
        b.set_blocked_by(blocked.id, vec![leaf.id]);

        let container_child = b
            .create(
                Some(project.id),
                "container-slot",
                "placeholder",
                Some("done".into()),
                Origin::Human,
                false,
                None,
            )
            .expect("slot");
        let _ = b.transition(container_child.id, State::Shaping, "t", None);
        let _ = b.transition(container_child.id, State::Backlog, "t", None);
        // Project has children → not claimable; leaf with wrong capability excluded.
        let ready_rust = b.list_ready(&["rust".into()]);
        let ready_ids: Vec<_> = ready_rust.iter().map(|i| i.id).collect();
        assert!(
            ready_ids.contains(&leaf.id),
            "rust capability matches rust leaf: {ready_ids:?}"
        );
        assert!(
            !ready_ids.contains(&blocked.id),
            "blocked leaf must be excluded"
        );
        assert!(!ready_ids.contains(&project.id), "Project must be excluded");

        let ready_python = b.list_ready(&["python".into()]);
        assert!(
            !ready_python.iter().any(|i| i.id == leaf.id),
            "capability mismatch excludes leaf"
        );

        // Parent/child helpers use children_by_parent.
        assert!(Board::has_children(&b.state.read(), project.id));
        assert!(!Board::has_children(&b.state.read(), leaf.id));
        let kids = b.children_of(project.id);
        assert!(kids.contains(&leaf.id));
        assert!(kids.contains(&blocked.id));
        assert_eq!(
            b.state.read().non_retired_child_count(project.id) as usize,
            kids.len()
        );

        // Dispatch queue: only unblocked leaf with flag.
        assert!(b.list_awaiting_dispatch().is_empty());
        b.enqueue_dispatch(leaf.id).expect("enqueue");
        assert_eq!(
            b.list_awaiting_dispatch()
                .iter()
                .map(|i| i.id)
                .collect::<Vec<_>>(),
            vec![leaf.id]
        );
        assert!(
            b.enqueue_dispatch(blocked.id).is_err(),
            "blocked card cannot dispatch"
        );

        // Digest ready_to_dispatch excludes awaiting_dispatch and blocked.
        let digest = b.digest();
        let goal = digest
            .goals
            .iter()
            .find(|g| g.goal_id == project.id)
            .expect("goal");
        assert!(
            !goal.ready_to_dispatch.iter().any(|c| c.id == leaf.id),
            "enqueued leaf not in ready_to_dispatch"
        );
        assert!(
            !goal.ready_to_dispatch.iter().any(|c| c.id == blocked.id),
            "blocked not ready"
        );
        assert!(
            goal.ready_to_dispatch
                .iter()
                .any(|c| c.id == container_child.id),
            "idle unblocked leaf is ready: {:?}",
            goal.ready_to_dispatch
        );

        // Indexes stay consistent across transition + delete.
        b.transition(leaf.id, State::Done, "t", Some("done".into()))
            .expect("done");
        assert!(
            b.list_ready(&["rust".into(), "python".into(), "any".into()])
                .iter()
                .any(|i| i.id == blocked.id),
            "completing blocker unblocks dependent"
        );
        let before = b.children_of(project.id).len();
        b.delete_item(container_child.id).expect("delete");
        assert_eq!(b.children_of(project.id).len(), before - 1);
        assert_eq!(
            b.state
                .read()
                .ids_by_state
                .get(&State::Backlog)
                .map(|s| s.len()),
            Some(
                b.state
                    .read()
                    .items
                    .values()
                    .filter(|i| i.state == State::Backlog)
                    .count()
            )
        );
    }

    #[test]
    fn snapshot_uses_project_roots_and_child_index() {
        let b = Board::new(
            Schema::default(),
            std::env::temp_dir().join("sandboard-test-snap-index.json"),
        );
        let p1 = b
            .create(None, "A", "a", None, Origin::Human, true, None)
            .unwrap();
        let p2 = b
            .create(None, "B", "b", None, Origin::Human, true, None)
            .unwrap();
        let _ = b.transition(p1.id, State::Shaping, "t", None);
        let _ = b.transition(p2.id, State::Shaping, "t", None);
        let t = b
            .create(
                Some(p1.id),
                "task",
                "x",
                Some("d".into()),
                Origin::Human,
                false,
                None,
            )
            .unwrap();
        let _ = b.transition(t.id, State::Shaping, "t", None);
        let _ = b.transition(t.id, State::Backlog, "t", None);

        let snap = b.snapshot();
        let goal_ids: Vec<_> = snap.goals.iter().map(|g| g.id).collect();
        assert!(goal_ids.contains(&p1.id));
        assert!(goal_ids.contains(&p2.id));
        let g1 = snap.goals.iter().find(|g| g.id == p1.id).unwrap();
        // Initial plan Task + our task = leaves under p1.
        assert!(g1.leaves_total >= 1);
    }

    #[test]
    fn project_auto_dispatch_queues_and_pauses() {
        let b = Board::new(
            Schema::default(),
            std::env::temp_dir().join("sandboard-test-auto-dispatch.json"),
        );
        let project = b
            .create(None, "goal", "why", None, Origin::Human, true, None)
            .expect("project");
        let _ = b.transition(project.id, State::Shaping, "t", None);
        let ready = b
            .create(
                Some(project.id),
                "ready",
                "do it",
                Some("done".into()),
                Origin::Human,
                false,
                None,
            )
            .expect("ready");
        let parked = b
            .create(
                Some(project.id),
                "parked",
                "wait",
                Some("done".into()),
                Origin::Human,
                false,
                None,
            )
            .expect("parked");
        let blocked = b
            .create(
                Some(project.id),
                "blocked",
                "later",
                Some("done".into()),
                Origin::Human,
                false,
                None,
            )
            .expect("blocked");
        for id in [ready.id, parked.id, blocked.id] {
            let _ = b.transition(id, State::Shaping, "t", None);
            let _ = b.transition(id, State::Backlog, "t", None);
        }
        b.set_blocked_by(blocked.id, vec![ready.id]);
        {
            let mut s = b.state.write();
            s.items.get_mut(&parked.id).unwrap().parked = true;
        }

        // No auto-seeded Initial plan; optional retire is a no-op when absent.
        if let Some(ip) = b.initial_plan_of(project.id) {
            let _ = b.transition(ip.id, State::Retired, "t", Some("test".into()));
        }

        assert!(b.list_awaiting_dispatch().is_empty());
        let proj = b.set_auto_dispatch(project.id, true).expect("auto on");
        assert!(proj.auto_dispatch);
        let awaiting = b.list_awaiting_dispatch();
        assert_eq!(awaiting.len(), 1, "only the unblocked unparked leaf");
        assert_eq!(awaiting[0].id, ready.id);
        assert!(!b.get(parked.id).unwrap().awaiting_dispatch);
        assert!(!b.get(blocked.id).unwrap().awaiting_dispatch);

        // Idempotent — already queued stays queued, no second phantom.
        b.auto_enqueue_all();
        assert_eq!(b.list_awaiting_dispatch().len(), 1);

        let snap = b.snapshot();
        let goal = snap
            .goals
            .iter()
            .find(|g| g.id == project.id)
            .expect("goal");
        assert!(goal.auto_dispatch);

        let proj = b.set_auto_dispatch(project.id, false).expect("auto off");
        assert!(!proj.auto_dispatch);
        assert!(
            b.list_awaiting_dispatch().is_empty(),
            "pause clears queued Backlog"
        );
        assert!(!b.get(ready.id).unwrap().awaiting_dispatch);

        let err = b.set_auto_dispatch(ready.id, true).unwrap_err();
        assert!(err.contains("Projects"), "{err}");
    }

    #[test]
    fn halt_clears_conversation_and_environment() {
        let (b, id) = claimed_leaf();
        b.set_environment(id, Some("sandboard-card-1-a1".into()));
        b.set_conversation_id(id, Some("conv-xyz".into()));
        let it = b.halt(id, Some("start over".into())).expect("halt");
        assert_eq!(it.state, State::Backlog);
        assert!(it.environment.is_none(), "halt deletes the sandbox binding");
        assert!(
            it.conversation_id.is_none(),
            "halt discards the LLM session"
        );
        assert!(!it.parked);
    }

    /// Failures under the cap requeue, so a transient problem self-heals.
    #[test]
    fn early_failures_requeue_while_budget_remains() {
        let (b, id) = claimed_leaf();
        let it = b
            .record_run_failure(id, "sandbox would not start", 3)
            .expect("recorded");
        assert_eq!(it.state, State::Backlog);
        assert_eq!(it.run_failures, 1);
    }

    /// Ctrl-C used to bounce Running → Backlog while the sandbox agent lived;
    /// restart must be able to Claim again without a full dispatch claim.
    #[test]
    fn reopen_for_adoption_from_backlog_with_environment() {
        let (b, id) = claimed_leaf();
        b.set_environment(id, Some("sandboard-card-1-a1".into()));
        b.record_run_failure(id, "agent exited -1: follower died", 3)
            .expect("failed into backlog");
        assert_eq!(b.get(id).unwrap().state, State::Backlog);

        let it = b
            .reopen_for_adoption(id, "sandbox-1", 3600)
            .expect("reopened");
        assert_eq!(it.state, State::Claimed);
        assert_eq!(it.environment.as_deref(), Some("sandboard-card-1-a1"));
        assert!(it.lease.is_some());
        assert!(it.run_deadline_at.is_some());
        assert!(!it.awaiting_dispatch);
    }

    /// The whole point: without this a card that fails early spends nothing,
    /// so no money cap stops it looping every lease period, forever.
    #[test]
    fn repeated_failures_become_a_humans_problem() {
        let (b, id) = claimed_leaf();
        for _ in 0..2 {
            b.record_run_failure(id, "clone refused", 3)
                .expect("requeued");
            b.claim(id, "agent", None, 45).expect("reclaim");
        }
        let it = b
            .record_run_failure(id, "clone refused", 3)
            .expect("escalated");
        assert_eq!(it.state, State::NeedsHuman);
        assert_eq!(it.run_failures, 3);

        // An escalation a human cannot act on in one tap is not a decision.
        let esc = it.escalation.expect("escalation present");
        assert!(
            esc.options.len() >= 2,
            "needs at least two concrete options"
        );
        assert!(
            esc.question.contains("clone refused"),
            "must say what went wrong"
        );
    }

    /// A run that dies before its first heartbeat is still Claimed, and
    /// Claimed -> NeedsHuman is not a legal edge. Escalating must still work.
    #[test]
    fn escalation_works_from_claimed_without_a_heartbeat() {
        let (b, id) = claimed_leaf();
        assert_eq!(b.get(id).unwrap().state, State::Claimed, "no heartbeat yet");
        let it = b
            .record_run_failure(id, "died instantly", 1)
            .expect("escalated");
        assert_eq!(it.state, State::NeedsHuman);
    }

    #[test]
    fn success_resets_the_retry_budget() {
        let (b, id) = claimed_leaf();
        b.record_run_failure(id, "flake", 3).expect("requeued");
        b.clear_run_failures(id);
        assert_eq!(b.get(id).unwrap().run_failures, 0);
    }

    /// Answering must buy a fresh budget, or the next single failure
    /// re-escalates immediately and the human's answer bought nothing.
    #[test]
    fn answering_an_escalation_resets_the_retry_budget() {
        let (b, id) = claimed_leaf();
        b.record_run_failure(id, "boom", 1).expect("escalated");
        assert_eq!(b.get(id).unwrap().run_failures, 1);
        b.answer_escalation(id, "Investigate the environment".into())
            .expect("answered");
        let it = b.get(id).unwrap();
        assert_eq!(it.state, State::Backlog);
        assert_eq!(it.run_failures, 0);
    }

    /// An answered escalation must not stay attached. Leaving it there made a
    /// running card keep reporting "blocked 15m" against a question that had
    /// already been resolved — the board contradicting itself about the one
    /// thing it exists to tell you.
    #[test]
    fn answering_clears_the_escalation_and_keeps_the_decision() {
        let (b, id) = claimed_leaf();
        b.record_run_failure(id, "boom", 1).expect("escalated");
        assert!(b.get(id).unwrap().escalation.is_some());

        b.answer_escalation(id, "Investigate the environment".into())
            .expect("answered");
        let it = b.get(id).unwrap();
        assert!(
            it.escalation.is_none(),
            "resolved escalation must not linger"
        );
        assert!(!it.parked, "ordinary answers must not auto-park");

        // The decision survives as standing context for whoever picks it up.
        assert!(
            it.notes
                .iter()
                .any(|n| n.text.contains("Investigate the environment")),
            "the decision must be preserved as a note: {:?}",
            it.notes
        );
    }

    /// Host-deferred answers without Proof facts must park — otherwise Project
    /// auto mode reclaims and the agent re-escalates (#174).
    #[test]
    fn answering_host_deferred_decision_parks_until_proof() {
        let (b, id) = claimed_leaf();
        b.record_run_failure(id, "boom", 1).expect("escalated");
        b.answer_escalation(
            id,
            "Host runs probe dispatch; re-claim this card to document".into(),
        )
        .expect("answered");
        let it = b.get(id).unwrap();
        assert_eq!(it.state, State::Backlog);
        assert!(it.parked, "host-deferred Decision must park");
        assert!(it.escalation.is_none());

        // Pasting pr_url= in the answer is evidence — do not park.
        let (b2, id2) = claimed_leaf();
        b2.record_run_failure(id2, "boom", 1).expect("escalated");
        b2.answer_escalation(
            id2,
            "Paste run facts now: card=#9 pr_url=https://github.com/clankrshq/sandboard-sandbox-probe/pull/2 upstream=clankrshq/sandboard-sandbox-probe"
                .into(),
        )
        .expect("answered");
        let it2 = b2.get(id2).unwrap();
        assert!(
            !it2.parked,
            "answers that already embed pr_url= must not park"
        );
    }

    #[test]
    fn answering_nothing_is_refused() {
        let (b, id) = claimed_leaf();
        assert!(b.answer_escalation(id, "whatever".into()).is_err());
    }

    #[test]
    fn delete_item_removes_item_and_descendants() {
        let b = std::sync::Arc::new(Board::new(
            crate::schema::Schema::default(),
            std::env::temp_dir().join("sandboard-test-del.json"),
        ));
        let p = b
            .create(None, "Parent", "intent", None, Origin::Human, false, None)
            .expect("project");
        let c = b
            .create(
                Some(p.id),
                "Child",
                "intent",
                None,
                Origin::Human,
                false,
                None,
            )
            .expect("task");
        assert!(b.get(p.id).is_some());
        assert!(b.get(c.id).is_some());

        b.delete_item(p.id).expect("delete parent");
        assert!(b.get(p.id).is_none());
        assert!(b.get(c.id).is_none());
    }

    #[test]
    fn propose_split_goes_to_review_approve_creates_siblings() {
        let (b, id) = claimed_leaf();
        let project_id = b.get(id).unwrap().parent.expect("task under project");
        let _ = b.transition(id, State::Running, "agent", None);
        let children = vec![
            SplitChildSpec::new("Leaf part 1", "Do leaf part 1", "Leaf part 1 done"),
            SplitChildSpec::new("Leaf part 2", "Do leaf part 2", "Leaf part 2 done"),
        ];
        let card = b
            .propose_split(id, "agent", children, 5)
            .expect("propose_split should succeed");
        assert_eq!(card.state, State::Review);
        assert_eq!(card.proposal.as_ref().unwrap().tasks.len(), 2);
        assert_eq!(
            b.children_of(project_id)
                .into_iter()
                .filter(|&cid| cid != id)
                .filter(|&cid| !b.get(cid).unwrap().is_initial_plan_task())
                .count(),
            0,
            "no siblings before Approve"
        );

        let done = b.approve_review(id).expect("approve");
        assert_eq!(done.state, State::Done);
        assert!(done.proposal.is_none());
        let siblings: Vec<_> = b
            .children_of(project_id)
            .into_iter()
            .filter_map(|cid| b.get(cid))
            .filter(|i| !i.is_initial_plan_task() && i.id != id)
            .collect();
        assert_eq!(siblings.len(), 2);
        assert!(siblings.iter().all(|s| s.state == State::Backlog));
    }

    #[test]
    fn propose_split_accepts_on_theme_children() {
        let b = Board::new(
            Schema::default(),
            std::env::temp_dir().join("sandboard-test-split-theme-accept.json"),
        );
        let project = b
            .create(
                None,
                "User Authentication System",
                "Manage user logins and tokens",
                None,
                Origin::Human,
                true,
                None,
            )
            .expect("project");
        let _ = b.transition(project.id, State::Shaping, "t", None);
        let task = b
            .create(
                Some(project.id),
                "Implement OAuth2 login flow",
                "Support Google and GitHub auth",
                Some("OAuth login working".into()),
                Origin::Human,
                false,
                None,
            )
            .expect("task");
        let _ = b.transition(task.id, State::Shaping, "t", None);
        let _ = b.transition(task.id, State::Backlog, "t", None);
        let _ = b.claim(task.id, "agent", None, 60).expect("claim");
        let _ = b.transition(task.id, State::Running, "agent", None);

        let children = vec![
            SplitChildSpec::new(
                "Google OAuth login endpoint",
                "Add endpoint for google auth callback",
                "Google auth done",
            ),
            SplitChildSpec::new(
                "GitHub OAuth token exchange",
                "Exchange code for github access token",
                "GitHub auth done",
            ),
        ];

        let card = b
            .propose_split(task.id, "agent", children, 5)
            .expect("on-theme propose_split should succeed");
        assert_eq!(card.state, State::Review);
        assert_eq!(card.proposal.as_ref().unwrap().tasks.len(), 2);
    }

    #[test]
    fn propose_split_rejects_off_theme_children() {
        let b = Board::new(
            Schema::default(),
            std::env::temp_dir().join("sandboard-test-split-theme-reject.json"),
        );
        let project = b
            .create(
                None,
                "User Authentication System",
                "Manage user logins and tokens",
                None,
                Origin::Human,
                true,
                None,
            )
            .expect("project");
        let _ = b.transition(project.id, State::Shaping, "t", None);
        let task = b
            .create(
                Some(project.id),
                "Implement OAuth2 login flow",
                "Support Google and GitHub auth",
                Some("OAuth login working".into()),
                Origin::Human,
                false,
                None,
            )
            .expect("task");
        let _ = b.transition(task.id, State::Shaping, "t", None);
        let _ = b.transition(task.id, State::Backlog, "t", None);
        let _ = b.claim(task.id, "agent", None, 60).expect("claim");
        let _ = b.transition(task.id, State::Running, "agent", None);

        let children = vec![
            SplitChildSpec::new(
                "Google OAuth login endpoint",
                "Add endpoint for google auth callback",
                "Google auth done",
            ),
            SplitChildSpec::new(
                "Database connection pool",
                "Optimize postgres max connection limit",
                "DB config done",
            ),
        ];

        let err = b.propose_split(task.id, "agent", children, 5).unwrap_err();
        assert!(
            err.contains("does not relate to parent card or project theme"),
            "got error: {err}"
        );
        assert_eq!(b.get(task.id).unwrap().state, State::Running);
    }

    #[test]
    fn propose_split_refused_below_minimum_siblings() {
        let (b, id) = claimed_leaf();
        let _ = b.transition(id, State::Running, "agent", None);
        let children = vec![SplitChildSpec::new("Single", "Only one", "Done")];
        let err = b.propose_split(id, "agent", children, 5).unwrap_err();
        assert!(err.contains("at least two siblings"), "got error: {err}");
    }

    #[test]
    fn propose_split_refused_exceeding_fanout_governor() {
        let (b, id) = claimed_leaf();
        let _ = b.transition(id, State::Running, "agent", None);
        let children: Vec<_> = (1..=6)
            .map(|i| {
                SplitChildSpec::new(
                    format!("Child {i}"),
                    format!("Intent {i}"),
                    format!("DoD {i}"),
                )
            })
            .collect();
        let err = b.propose_split(id, "agent", children, 5).unwrap_err();
        assert!(
            err.contains("exceeds max_children_per_split=5"),
            "got error: {err}"
        );
    }

    #[test]
    fn propose_split_refused_on_project_root() {
        let b = Board::new(
            Schema::default(),
            std::env::temp_dir().join("sandboard-test-split-root.json"),
        );
        let project = b
            .create(None, "proj", "why", None, Origin::Human, true, None)
            .expect("project");
        let _ = b.transition(project.id, State::Shaping, "t", None);
        let err = b
            .propose_split(
                project.id,
                "agent",
                vec![
                    SplitChildSpec::new("A", "a", "done"),
                    SplitChildSpec::new("B", "b", "done"),
                ],
                5,
            )
            .unwrap_err();
        assert!(err.contains("cannot split a Project"), "got error: {err}");
    }

    #[test]
    fn propose_split_refused_when_pr_exists() {
        let (b, id) = claimed_leaf();
        b.set_pr_url(
            id,
            Some("https://github.com/sandboard-app/sandboard/pull/42".to_string()),
        );
        let children = vec![
            SplitChildSpec::new("Part 1", "Do part 1", "Part 1 done"),
            SplitChildSpec::new("Part 2", "Do part 2", "Part 2 done"),
        ];
        let err = b.propose_split(id, "agent", children, 5).unwrap_err();
        assert!(err.contains("a PR already exists"), "got error: {err}");

        let item = b.get(id).expect("item exists");
        assert_eq!(item.state, State::NeedsHuman);
        assert!(item.escalation.is_some(), "escalation must be populated");
        let esc = item.escalation.unwrap();
        assert!(esc.question.contains("a PR already exists"));
        assert_eq!(esc.options.len(), 2);
    }

    #[test]
    fn initial_plan_refuses_propose_split() {
        let b = Board::new(
            Schema::default(),
            std::env::temp_dir().join("sandboard-test-initial-plan-no-split.json"),
        );
        let (project, seed) = project_with_initial_plan(&b, "Archive UI");
        let seed_id = seed.id;
        let _ = project;
        let _ = b
            .claim(seed_id, "agent", None, 60)
            .expect("claim initial plan");
        let _ = b.transition(seed_id, State::Running, "agent", None);

        let err = b
            .propose_split(
                seed_id,
                "agent",
                vec![
                    SplitChildSpec::new(
                        "API archive endpoint",
                        "Expose archive for board cards",
                        "Archive API works",
                    ),
                    SplitChildSpec::new(
                        "UI archive controls",
                        "Add archive actions in the board UI",
                        "Archive UI works",
                    ),
                ],
                5,
            )
            .unwrap_err();
        assert!(
            err.contains("Initial plan cannot") || err.contains("plan.json"),
            "got: {err}"
        );
        assert_eq!(b.get(seed_id).unwrap().state, State::Running);
    }

    #[test]
    fn approve_split_proposal_wires_blocked_by_keys() {
        let b = Board::new(
            Schema::default(),
            std::env::temp_dir().join("sandboard-test-split-deps.json"),
        );
        let project = b
            .create(
                None,
                "Archive UI",
                "Archive completed work from the board",
                None,
                Origin::Human,
                true,
                None,
            )
            .expect("project");
        let task = b
            .create(
                Some(project.id),
                "Archive feature",
                "Archive completed work from the board UI",
                Some("Archive works".into()),
                Origin::Human,
                false,
                None,
            )
            .expect("task");
        b.set_task_repo(task.id, Some(test_task_repo()))
            .expect("bind task repo");
        let _ = b.transition(task.id, State::Shaping, "t", None);
        let _ = b.transition(task.id, State::Backlog, "t", None);
        let _ = b.claim(task.id, "agent", None, 60).expect("claim");
        let _ = b.transition(task.id, State::Running, "agent", None);

        b.propose_split(
            task.id,
            "agent",
            vec![
                SplitChildSpec::new(
                    "API archive endpoint",
                    "Expose archive for board cards",
                    "Archive API works",
                )
                .with_deps("a", vec![]),
                SplitChildSpec::new(
                    "UI archive controls",
                    "Add archive actions in the board UI",
                    "Archive UI works",
                )
                .with_deps("b", vec!["a".into()]),
            ],
            5,
        )
        .expect("propose");
        b.approve_review(task.id).expect("approve");
        let siblings: Vec<_> = b
            .children_of(project.id)
            .into_iter()
            .filter_map(|cid| b.get(cid))
            .filter(|i| !i.is_initial_plan_task() && i.id != task.id)
            .collect();
        assert_eq!(siblings.len(), 2);
        let a = siblings.iter().find(|m| m.title.contains("API")).unwrap();
        let bb = siblings.iter().find(|m| m.title.contains("UI")).unwrap();
        assert_eq!(b.get(bb.id).unwrap().blocked_by, vec![a.id]);
    }

    #[test]
    fn approve_split_reuses_existing_siblings_by_title() {
        let b = Board::new(
            Schema::default(),
            std::env::temp_dir().join("sandboard-test-split-idempotent.json"),
        );
        let project = b
            .create(
                None,
                "Archive UI",
                "Archive completed work from the board",
                None,
                Origin::Human,
                true,
                None,
            )
            .expect("project");
        let card = b
            .create(
                Some(project.id),
                "Archive feature",
                "Archive completed work from the board",
                Some("Archive works".into()),
                Origin::Human,
                false,
                None,
            )
            .expect("card");
        b.set_task_repo(card.id, Some(test_task_repo()))
            .expect("bind card repo");
        let _ = b.transition(card.id, State::Shaping, "t", None);
        let _ = b.transition(card.id, State::Backlog, "t", None);

        let preexisting = b
            .create(
                Some(project.id),
                "API archive endpoint",
                "already shaped",
                Some("done".into()),
                Origin::Human,
                false,
                None,
            )
            .expect("preexisting");
        let _ = b.transition(preexisting.id, State::Shaping, "t", None);
        let preexisting = b
            .transition(preexisting.id, State::Backlog, "t", None)
            .expect("ready");

        let before = b.children_of(project.id).len();
        let _ = b.claim(card.id, "agent", None, 60).expect("claim");
        let _ = b.transition(card.id, State::Running, "agent", None);
        b.propose_split(
            card.id,
            "agent",
            vec![
                SplitChildSpec::new(
                    "API archive endpoint",
                    "Expose archive for board cards",
                    "Archive API works",
                ),
                SplitChildSpec::new(
                    "UI archive controls",
                    "Add archive actions in the board UI",
                    "Archive UI works",
                ),
            ],
            5,
        )
        .expect("propose");
        b.approve_review(card.id).expect("approve");
        let siblings: Vec<_> = b
            .children_of(project.id)
            .into_iter()
            .filter_map(|cid| b.get(cid))
            .filter(|i| !i.is_initial_plan_task() && i.id != card.id)
            .collect();
        assert_eq!(siblings.len(), 2);
        assert!(
            siblings.iter().any(|s| s.id == preexisting.id),
            "matching title must be reused"
        );
        assert_eq!(
            b.children_of(project.id).len(),
            before + 1,
            "only the missing sibling should be created"
        );
    }

    #[test]
    fn request_changes_clears_proposal() {
        let (b, id) = claimed_leaf();
        let _ = b.transition(id, State::Running, "agent", None);
        b.propose_split(
            id,
            "agent",
            vec![
                SplitChildSpec::new("A", "a", "a done"),
                SplitChildSpec::new("B", "b", "b done"),
            ],
            5,
        )
        .expect("propose");
        assert!(b.get(id).unwrap().proposal.is_some());
        let item = b
            .request_changes(id, "narrow the split".into())
            .expect("request_changes");
        assert_eq!(item.state, State::Backlog);
        assert!(item.proposal.is_none());
    }

    #[test]
    fn nest_under_task_is_refused() {
        let b = Board::new(
            Schema::default(),
            std::env::temp_dir().join("sandboard-test-nest.json"),
        );
        let project = b
            .create(None, "proj", "why", None, Origin::Human, true, None)
            .expect("project");
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
        let err = b
            .create(
                Some(task.id),
                "nested",
                "no",
                None,
                Origin::Human,
                false,
                None,
            )
            .unwrap_err();
        assert!(err.contains("flat under a Project"), "got error: {err}");
    }

    #[test]
    fn create_task_lands_in_backlog_stamps_clone_and_applies_blockers() {
        let b = Board::new(
            Schema::default(),
            std::env::temp_dir().join(format!(
                "sandboard-test-create-task-{}-{}.json",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            )),
        );
        let project = b
            .create_project("Proj", "Ship it", "sandboard-app/sandboard", true, None)
            .expect("project");

        let blocker = b
            .create_task(
                project.id,
                "First",
                "Clone repository: other/repo. Do first.",
                "first done",
                vec![],
                None,
                false,
            )
            .expect("blocker");
        assert_eq!(blocker.state, State::Backlog);
        assert!(matches!(blocker.origin, Origin::Human));
        assert!(
            blocker.intent.contains("Clone repository: other/repo"),
            "explicit clone must not be overwritten: {}",
            blocker.intent
        );

        let blocked = b
            .create_task(
                project.id,
                "Second",
                "Do second after first",
                "second done",
                vec![blocker.id],
                None,
                false,
            )
            .expect("blocked");
        assert_eq!(blocked.state, State::Backlog);
        assert_eq!(blocked.blocked_by, vec![blocker.id]);
        assert!(
            blocked.intent.contains("Clone repository: sandboard-app/sandboard"),
            "Project default must stamp when omitted: {}",
            blocked.intent
        );

        let nest_err = b
            .create_task(
                blocker.id,
                "Nested",
                "no",
                "done",
                vec![],
                None,
                false,
            )
            .unwrap_err();
        assert!(
            nest_err.contains("flat under a Project"),
            "got error: {nest_err}"
        );

        let dod_err = b
            .create_task(project.id, "No DoD", "intent", "  ", vec![], None, false)
            .unwrap_err();
        assert!(
            dod_err.contains("definition_of_done"),
            "got error: {dod_err}"
        );
    }

    #[test]
    fn create_task_without_project_clone_leaves_prose_unstamped() {
        let b = Board::new(
            Schema::default(),
            std::env::temp_dir().join(format!(
                "sandboard-test-create-task-no-clone-{}-{}.json",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            )),
        );
        // Bare create — no Clone repository line on the Project.
        let project = b
            .create(None, "No Repo Proj", "why", None, Origin::Human, true, None)
            .expect("project");
        let task = b
            .create_task(
                project.id,
                "Ad hoc",
                "Ship without naming a repo",
                "done",
                vec![],
                None,
                false,
            )
            .expect("task");
        assert_eq!(task.state, State::Backlog);
        assert!(
            crate::schema::clone_repo_from_prose(&task.intent).is_none(),
            "must not invent a clone target: {}",
            task.intent
        );
    }

    #[test]
    fn project_create_auto_seeds_initial_plan() {
        let b = Board::new(
            Schema::default(),
            std::env::temp_dir().join("sandboard-test-auto-seed.json"),
        );
        let project = b
            .create(None, "Phase X", "why", None, Origin::Human, true, None)
            .expect("project");
        assert!(
            project.plan.is_none(),
            "Plan lives on Initial plan, not Project"
        );
        let seed = b
            .initial_plan_of(project.id)
            .expect("create_project auto-seeds Initial plan");
        assert_eq!(seed.title, initial_plan_title("Phase X"));
        assert_eq!(seed.state, State::Backlog);
        assert!(seed.is_initial_plan_task());
        assert!(
            seed.repo.is_none(),
            "Initial plan remotes come from prose until PR"
        );
        assert!(b.resolve_card_repo(seed.id).unwrap().is_none());
        assert_ne!(b.get(project.id).unwrap().state, State::Backlog);
    }

    #[test]
    fn create_project_requires_clone_repo_and_stamps_initial_plan() {
        let b = Board::new(
            Schema::default(),
            std::env::temp_dir().join("sandboard-test-create-project-clone.json"),
        );
        let err = b
            .create_project("No Repo", "why", "", true, None)
            .expect_err("empty clone_repo");
        assert!(err.contains("clone_repo"), "{err}");

        let project = b
            .create_project(
                "OpenShell settings",
                "Rework the OpenShell settings surface",
                "sandboard-app/sandboard",
                true,
                None,
            )
            .expect("create_project");
        assert!(
            project.intent.contains("Clone repository: sandboard-app/sandboard"),
            "{}",
            project.intent
        );
        let prompt = project.project_prompt.as_deref();
        assert!(
            prompt.is_none() || prompt.is_some_and(|p| p.trim().is_empty()),
            "new Projects must not seed board essay into project_prompt: {prompt:?}"
        );
        let seed = b.initial_plan_of(project.id).expect("seeded");
        assert!(
            seed.intent.contains("Clone repository: sandboard-app/sandboard"),
            "Initial plan must stamp planning clone: {}",
            seed.intent
        );
        assert!(
            seed.definition_of_done
                .as_deref()
                .unwrap_or("")
                .contains("sandboard-app/sandboard"),
            "{:?}",
            seed.definition_of_done
        );
    }

    #[test]
    fn init_plan_is_idempotent_after_auto_seed() {
        let b = Board::new(
            Schema::default(),
            std::env::temp_dir().join("sandboard-test-init-plan.json"),
        );
        let project = b
            .create(None, "Phase X", "why", None, Origin::Human, true, None)
            .expect("project");
        let seed = b.initial_plan_of(project.id).expect("auto-seed");
        assert!(b.may_claim(seed.id));
        assert!(
            b.list_ready(&["any".into()])
                .iter()
                .any(|i| i.id == seed.id),
            "Initial plan must appear in list_ready"
        );

        let again = b.init_plan(project.id).expect("idempotent init_plan");
        assert_eq!(again.id, seed.id);
        assert_eq!(b.children_of(project.id), vec![seed.id]);
    }

    #[test]
    fn approve_creates_siblings_without_structured_repo() {
        let b = Board::new(
            Schema::default(),
            std::env::temp_dir().join("sandboard-test-approve-no-repo.json"),
        );
        let (project, seed) = project_with_initial_plan(&b, "Phase Prose Repo");
        assert!(seed.repo.is_none());

        b.propose_plan(
            project.id,
            "prose clone targets",
            vec![
                PlanTaskSpec {
                    key: "t1".into(),
                    title: "Sibling One".into(),
                    intent: "Clone acme/widgets and do one".into(),
                    definition_of_done: "one done in acme/widgets".into(),
                    blocked_by_keys: vec![],
                    capability: None,
                    repo: None,
                    item_id: None,
                },
                PlanTaskSpec {
                    key: "t2".into(),
                    title: "Sibling Two".into(),
                    intent: "Clone acme/other and do two".into(),
                    definition_of_done: "two done".into(),
                    blocked_by_keys: vec![],
                    capability: None,
                    repo: None,
                    item_id: None,
                },
            ],
            vec![],
        )
        .expect("propose");

        let published = b.approve_plan(project.id).expect("approve");
        assert_eq!(published.len(), 2);
        for id in &published {
            let task = b.get(*id).expect("sibling");
            assert!(
                task.repo.is_none(),
                "siblings resolve remotes from prose until pull_request"
            );
            assert!(b.resolve_card_repo(*id).unwrap().is_none());
        }
    }

    #[test]
    fn split_approve_creates_siblings_without_structured_repo() {
        let (b, id) = claimed_leaf();
        let project_id = b.get(id).unwrap().parent.expect("under project");
        let _ = b.transition(id, State::Running, "agent", None);
        b.propose_split(
            id,
            "agent",
            vec![
                SplitChildSpec::new("Split A", "clone acme/a — a", "a done"),
                SplitChildSpec::new("Split B", "clone acme/b — b", "b done"),
            ],
            5,
        )
        .expect("propose");
        b.approve_review(id).expect("approve");
        let siblings: Vec<_> = b
            .children_of(project_id)
            .into_iter()
            .filter_map(|cid| b.get(cid))
            .filter(|i| !i.is_initial_plan_task() && i.id != id)
            .collect();
        assert_eq!(siblings.len(), 2);
        for s in &siblings {
            assert!(s.repo.is_none());
            assert!(b.resolve_card_repo(s.id).unwrap().is_none());
        }
    }

    #[test]
    fn approve_plan_materializes_from_initial_plan_proposal() {
        let b = Board::new(
            Schema::default(),
            std::env::temp_dir().join("sandboard-test-approve-plan.json"),
        );
        let (project, _seed) = project_with_initial_plan(&b, "Phase Y");
        let _ = b.transition(project.id, State::Shaping, "t", None);

        b.propose_plan(
            project.id,
            "first cut",
            vec![
                PlanTaskSpec {
                    key: "a".into(),
                    title: "Task A".into(),
                    intent: "do a".into(),
                    definition_of_done: "a done".into(),
                    blocked_by_keys: vec![],
                    capability: None,
                    repo: None,
                    item_id: None,
                },
                PlanTaskSpec {
                    key: "b".into(),
                    title: "Task B".into(),
                    intent: "do b".into(),
                    definition_of_done: "b done".into(),
                    blocked_by_keys: vec!["a".into()],
                    capability: None,
                    repo: None,
                    item_id: None,
                },
            ],
            vec![],
        )
        .expect("propose");

        let seed = b
            .children_of(project.id)
            .into_iter()
            .find_map(|id| b.get(id).filter(|i| i.is_initial_plan_task()))
            .expect("seed");
        assert!(seed.proposal.as_ref().is_some_and(|p| p.tasks.len() == 2));
        assert!(b.get(project.id).unwrap().plan.is_none());

        let published = b.approve_plan(project.id).expect("approve");
        assert_eq!(published.len(), 2);
        assert_ne!(b.get(project.id).unwrap().state, State::Backlog);
        assert!(b.get(project.id).unwrap().plan.is_none());

        let a = b.get(published[0]).unwrap();
        let b_item = b.get(published[1]).unwrap();
        assert_eq!(a.state, State::Backlog);
        assert_eq!(b_item.state, State::Backlog);
        assert_eq!(b_item.blocked_by, vec![published[0]]);

        let seed = b.get(seed.id).unwrap();
        assert!(seed.state.is_terminal());
        // Frozen proposal with stamped item_ids.
        let prop = seed.proposal.expect("frozen proposal");
        assert_eq!(prop.tasks.len(), 2);
        assert!(prop.tasks.iter().all(|t| t.item_id.is_some()));
    }

    #[test]
    fn approve_plan_closes_initial_plan_in_review() {
        let b = Board::new(
            Schema::default(),
            std::env::temp_dir().join("sandboard-test-approve-closes-review.json"),
        );
        let (project, seed) = project_with_initial_plan(&b, "Phase Review");
        let seed_id = seed.id;
        let _ = b.transition(project.id, State::Shaping, "t", None);
        // Simulate finished Initial plan sitting in Review.
        let _ = b.claim(seed_id, "agent-1", None, 60).unwrap();
        let _ = b.transition(seed_id, State::Running, "agent-1", None);
        b.set_pr_url(seed_id, Some("https://example.com/pr/1".into()));
        let _ = b
            .report(seed_id, "agent-1", 1, 0, vec!["docs".into()])
            .expect("report");
        assert_eq!(b.get(seed_id).unwrap().state, State::Review);

        b.propose_plan(
            project.id,
            "from review",
            vec![PlanTaskSpec {
                key: "a".into(),
                title: "Task A".into(),
                intent: "do a".into(),
                definition_of_done: "a done".into(),
                blocked_by_keys: vec![],
                capability: None,
                repo: None,
                item_id: None,
            }],
            vec![],
        )
        .expect("propose");

        let published = b.approve_plan(project.id).expect("approve creates tasks");
        assert_eq!(published.len(), 1);
        assert_eq!(b.get(seed_id).unwrap().state, State::Done);
        assert_eq!(
            b.children_of(project.id)
                .into_iter()
                .filter(|&id| !b.get(id).unwrap().is_initial_plan_task())
                .count(),
            1
        );
        // Late webhook must not invent a second set of Tasks.
        assert!(
            b.complete_for_merged_pr("https://example.com/pr/1", Some(1))
                .is_none(),
            "already-Done Initial plan ignores merge webhook"
        );
        assert_eq!(
            b.children_of(project.id)
                .into_iter()
                .filter(|&id| !b.get(id).unwrap().is_initial_plan_task())
                .count(),
            1
        );
    }

    #[test]
    fn approve_review_on_initial_plan_materializes_awaiting_plan() {
        let b = Board::new(
            Schema::default(),
            std::env::temp_dir().join("sandboard-test-approve-review-initial.json"),
        );
        let (project, seed) = project_with_initial_plan(&b, "Phase AR");
        let seed_id = seed.id;
        let _ = b.transition(project.id, State::Shaping, "t", None);
        let _ = b.claim(seed_id, "agent-1", None, 60).unwrap();
        let _ = b.transition(seed_id, State::Running, "agent-1", None);
        b.set_pr_url(seed_id, Some("https://example.com/pr/2".into()));
        let _ = b
            .report(seed_id, "agent-1", 1, 0, vec!["docs".into()])
            .expect("report");

        b.propose_plan(
            project.id,
            "awaiting",
            vec![
                PlanTaskSpec {
                    key: "a".into(),
                    title: "Task A".into(),
                    intent: "do a".into(),
                    definition_of_done: "a done".into(),
                    blocked_by_keys: vec![],
                    capability: None,
                    repo: None,
                    item_id: None,
                },
                PlanTaskSpec {
                    key: "b".into(),
                    title: "Task B".into(),
                    intent: "do b".into(),
                    definition_of_done: "b done".into(),
                    blocked_by_keys: vec!["a".into()],
                    capability: None,
                    repo: None,
                    item_id: None,
                },
            ],
            vec![],
        )
        .expect("propose");

        // Approve creates Tasks even with a plan/docs PR attached.
        let done = b.approve_review(seed_id).expect("approve_review");
        assert_eq!(done.state, State::Done);
        assert!(b.get(project.id).unwrap().plan.is_none());
        assert!(done.proposal.as_ref().is_some_and(|p| p.tasks.len() == 2));
        let tasks: Vec<_> = b
            .children_of(project.id)
            .into_iter()
            .filter_map(|id| b.get(id))
            .filter(|i| !i.is_initial_plan_task())
            .collect();
        assert_eq!(tasks.len(), 2);
        assert!(tasks.iter().all(|t| t.state == State::Backlog));
        assert!(done
            .proposal
            .as_ref()
            .unwrap()
            .tasks
            .iter()
            .all(|t| t.item_id.is_some()));
    }

    #[test]
    fn propose_plan_refused_after_initial_plan_accepted() {
        let b = Board::new(
            Schema::default(),
            std::env::temp_dir().join("sandboard-test-plan-frozen.json"),
        );
        let (project, _seed) = project_with_initial_plan(&b, "Phase Freeze");
        let _ = b.transition(project.id, State::Shaping, "t", None);
        b.propose_plan(
            project.id,
            "once",
            vec![PlanTaskSpec {
                key: "a".into(),
                title: "Task A".into(),
                intent: "do a".into(),
                definition_of_done: "a done".into(),
                blocked_by_keys: vec![],
                capability: None,
                repo: None,
                item_id: None,
            }],
            vec![],
        )
        .expect("propose");
        let _ = b.approve_plan(project.id).expect("approve");
        let err = b
            .propose_plan(
                project.id,
                "again",
                vec![PlanTaskSpec {
                    key: "b".into(),
                    title: "Task B".into(),
                    intent: "do b".into(),
                    definition_of_done: "b done".into(),
                    blocked_by_keys: vec![],
                    capability: None,
                    repo: None,
                    item_id: None,
                }],
                vec![],
            )
            .expect_err("frozen");
        assert!(err.contains("frozen") || err.contains("accepted"), "{err}");
    }

    #[test]
    fn claim_briefing_reads_frozen_initial_plan_proposal() {
        let b = Board::new(
            Schema::default(),
            std::env::temp_dir().join("sandboard-test-claim-from-proposal.json"),
        );
        let (project, _seed) = project_with_initial_plan(&b, "Phase Brief");
        let _ = b.transition(project.id, State::Shaping, "t", None);
        b.propose_plan(
            project.id,
            "brief plan",
            vec![PlanTaskSpec {
                key: "a".into(),
                title: "Task A".into(),
                intent: "do a".into(),
                definition_of_done: "a done".into(),
                blocked_by_keys: vec![],
                capability: None,
                repo: None,
                item_id: None,
            }],
            vec![],
        )
        .expect("propose");
        let published = b.approve_plan(project.id).expect("approve");
        let task_id = published[0];
        let grant = b.claim(task_id, "agent", None, 60).expect("claim");
        assert_eq!(grant.plan_summary.as_deref(), Some("brief plan"));
        assert_eq!(grant.plan_tasks.len(), 1);
        assert!(grant.plan_tasks[0].current);
        assert_eq!(grant.plan_task_key.as_deref(), Some("a"));
        assert_eq!(grant.intent, "do a");
        assert_eq!(grant.title, "Task A");
    }

    #[test]
    fn approve_plan_materializes_diamond_dag() {
        let b = Board::new(
            Schema::default(),
            std::env::temp_dir().join("sandboard-test-approve-diamond-dag.json"),
        );
        let (project, _seed) = project_with_initial_plan(&b, "Phase Diamond");
        let _ = b.transition(project.id, State::Shaping, "t", None);

        b.propose_plan(
            project.id,
            "diamond plan",
            vec![
                PlanTaskSpec {
                    key: "a".into(),
                    title: "Task A".into(),
                    intent: "do a".into(),
                    definition_of_done: "a done".into(),
                    blocked_by_keys: vec![],
                    capability: None,
                    repo: None,
                    item_id: None,
                },
                PlanTaskSpec {
                    key: "b".into(),
                    title: "Task B".into(),
                    intent: "do b".into(),
                    definition_of_done: "b done".into(),
                    blocked_by_keys: vec!["a".into()],
                    capability: None,
                    repo: None,
                    item_id: None,
                },
                PlanTaskSpec {
                    key: "c".into(),
                    title: "Task C".into(),
                    intent: "do c".into(),
                    definition_of_done: "c done".into(),
                    blocked_by_keys: vec!["a".into()],
                    capability: None,
                    repo: None,
                    item_id: None,
                },
                PlanTaskSpec {
                    key: "d".into(),
                    title: "Task D".into(),
                    intent: "do d".into(),
                    definition_of_done: "d done".into(),
                    blocked_by_keys: vec!["b".into(), "c".into()],
                    capability: None,
                    repo: None,
                    item_id: None,
                },
            ],
            vec![],
        )
        .expect("propose");

        let published = b.approve_plan(project.id).expect("approve");
        assert_eq!(published.len(), 4);

        let id_a = published[0];
        let id_b = published[1];
        let id_c = published[2];
        let id_d = published[3];

        let item_a = b.get(id_a).unwrap();
        let item_b = b.get(id_b).unwrap();
        let item_c = b.get(id_c).unwrap();
        let item_d = b.get(id_d).unwrap();

        assert_eq!(item_a.blocked_by, Vec::<ItemId>::new());
        assert_eq!(item_b.blocked_by, vec![id_a]);
        assert_eq!(item_c.blocked_by, vec![id_a]);
        assert_eq!(item_d.blocked_by, vec![id_b, id_c]);

        // Verify Backlog column summary lists plain language blockers
        let s = b.state.read();
        let goal_view = b
            .goal_view(&s, project.id, chrono::Utc::now())
            .expect("goal view");
        let ready_col = goal_view
            .columns
            .iter()
            .find(|c| c.column == Column::Backlog)
            .expect("ready col");
        assert!(ready_col.summary.text.contains("blocked on"));
    }

    /// Project + Backlog task under it. Returns (board, project_id, task_id).
    fn project_with_ready_task() -> (Board, ItemId, ItemId) {
        let b = Board::new(
            Schema::default(),
            std::env::temp_dir().join(format!("sandboard-proj-task-{}.json", std::process::id())),
        );
        let project = b
            .create(None, "proj", "why", None, Origin::Human, true, None)
            .expect("project");
        let _ = b.transition(project.id, State::Shaping, "t", None);
        let task = b
            .create(
                Some(project.id),
                "task",
                "do it",
                Some("done".into()),
                Origin::Human,
                false,
                None,
            )
            .expect("task");
        let _ = b.transition(task.id, State::Shaping, "t", None);
        let _ = b.transition(task.id, State::Backlog, "t", None);
        (b, project.id, task.id)
    }

    #[test]
    fn release_with_reason_records_bounce_reason_and_history() {
        let (b, _project, task_id) = project_with_ready_task();
        let agent_id = "agent-infra-test";

        // Claim task
        let _grant = b.claim(task_id, agent_id, None, 300).expect("claim task");
        let claimed = b.get(task_id).expect("claimed item");
        assert_eq!(claimed.state, State::Claimed);
        assert_eq!(claimed.last_bounce_reason, None);

        // Release with reason
        let bounce_msg = "infra failure: podman socket connection refused";
        let released = b
            .release_with_reason(task_id, agent_id, Some(bounce_msg))
            .expect("release with reason");

        assert_eq!(released.state, State::Backlog);
        assert_eq!(released.last_bounce_reason.as_deref(), Some(bounce_msg));

        // Verify transition history
        let last_transition = released.history.last().expect("has transition history");
        assert_eq!(last_transition.from, State::Claimed);
        assert_eq!(last_transition.to, State::Backlog);
        assert_eq!(last_transition.by, agent_id);
        assert_eq!(last_transition.reason.as_deref(), Some(bounce_msg));

        // Verify state store persistence/get
        let fetched = b.get(task_id).expect("fetched item");
        assert_eq!(fetched.last_bounce_reason.as_deref(), Some(bounce_msg));
    }

    #[test]
    fn snapshot_effective_agents_does_not_reenter_rwlock() {
        // Regression: Agent runtime made snapshot call effective_agents() →
        // agent_runtime() while still holding state.read(); freeze → NOT LIVE.
        let b = Board::new(
            Schema::default(),
            std::env::temp_dir().join("sandboard-test-snapshot-agents-reenter.json"),
        );
        assert!(b.seed_agent_runtime_if_empty());
        let snap = b.snapshot();
        assert!(!snap.default_engine.is_empty());
        assert!(snap.agent_timeout_secs > 0);
    }

    #[test]
    fn snapshot_plan_status_does_not_reenter_rwlock() {
        // Regression: goal_view → plan_status_label used to call children_of while
        // snapshot still held state.read(); std RwLock is not reentrant → freeze.
        let b = Board::new(
            Schema::default(),
            std::env::temp_dir().join("sandboard-test-snapshot-reenter.json"),
        );
        let (project, seed) = project_with_initial_plan(&b, "Reenter");
        let seed_id = seed.id;
        let _ = b.transition(project.id, State::Shaping, "t", None);
        let _ = b.transition(seed_id, State::Done, "t", Some("legacy".into()));
        // Clear proposal so plan_status walks the impl-children path.
        {
            let mut s = b.state.write();
            if let Some(seed) = s.items.get_mut(&seed_id) {
                seed.proposal = None;
            }
        }
        let _ = b
            .create(
                Some(project.id),
                "Impl",
                "do",
                None,
                Origin::Human,
                true,
                None,
            )
            .expect("impl");
        let snap = b.snapshot();
        let g = snap
            .goals
            .iter()
            .find(|g| g.id == project.id)
            .expect("goal");
        assert_eq!(g.plan_status, "approved");
    }

    #[test]
    fn archived_project_marked_in_snapshot_omitted_from_digest() {
        let b = Board::new(
            Schema::default(),
            std::env::temp_dir().join("sandboard-test-archive-hide.json"),
        );
        let keep = b
            .create(None, "Keep me", "why", None, Origin::Human, true, None)
            .expect("keep");
        let archive = b
            .create(None, "Archive me", "why", None, Origin::Human, true, None)
            .expect("archive");
        let _ = b.transition(keep.id, State::Shaping, "t", None);
        let _ = b.transition(archive.id, State::Shaping, "t", None);

        assert!(b.snapshot().goals.iter().any(|g| g.id == archive.id));
        assert!(b.digest().goals.iter().any(|g| g.goal_id == archive.id));

        b.cut_scope(archive.id, Some("archived".into()))
            .expect("cut");
        assert_eq!(b.get(archive.id).unwrap().state, State::Retired);
        let snap = b.snapshot();
        let archived_goal = snap
            .goals
            .iter()
            .find(|g| g.id == archive.id)
            .expect("retired Project stays in snapshot for Show archived");
        assert!(
            archived_goal.archived,
            "retired Project must be marked archived"
        );
        assert!(
            b.digest().goals.iter().all(|g| g.goal_id != archive.id),
            "retired Project must not appear in digest"
        );
        assert!(
            b.snapshot()
                .goals
                .iter()
                .any(|g| g.id == keep.id && !g.archived),
            "active Project still listed"
        );
    }

    #[test]
    fn unarchive_scope_round_trips_archived_project() {
        let b = Board::new(
            Schema::default(),
            std::env::temp_dir().join(format!(
                "sandboard-test-unarchive-roundtrip-{}.json",
                std::process::id()
            )),
        );
        let archive = b
            .create(None, "Archive me", "why", None, Origin::Human, true, None)
            .expect("archive");
        let _ = b.transition(archive.id, State::Shaping, "t", None);
        let seed = b.initial_plan_of(archive.id).expect("seed");

        b.cut_scope(archive.id, Some("archived".into()))
            .expect("cut");
        assert!(
            b.snapshot()
                .goals
                .iter()
                .find(|g| g.id == archive.id)
                .expect("goal")
                .archived
        );
        assert!(b.digest().goals.iter().all(|g| g.goal_id != archive.id));

        let touched = b
            .unarchive_scope(archive.id, Some("restored".into()))
            .expect("unarchive");
        assert!(touched.contains(&archive.id));
        assert!(touched.contains(&seed.id));

        let restored = b.get(archive.id).unwrap();
        assert_eq!(restored.state, State::Shaping);
        assert!(restored.lease.is_none());
        assert!(!restored.awaiting_dispatch);

        let snap = b.snapshot();
        let goal = snap
            .goals
            .iter()
            .find(|g| g.id == archive.id)
            .expect("goal back in snapshot");
        assert!(
            !goal.archived,
            "unarchived Project must not be marked archived"
        );
        assert!(
            b.digest().goals.iter().any(|g| g.goal_id == archive.id),
            "unarchived Project must reappear in digest"
        );
    }

    #[test]
    fn unarchive_scope_remaps_in_flight_prior_to_backlog() {
        let b = Board::new(
            Schema::default(),
            std::env::temp_dir().join(format!(
                "sandboard-test-unarchive-inflight-{}.json",
                std::process::id()
            )),
        );
        let (project, _seed) = project_with_initial_plan(&b, "In flight");
        let _ = b.transition(project.id, State::Shaping, "t", None);
        let leaf = b
            .create(
                Some(project.id),
                "Running leaf",
                "do",
                Some("tests green".into()),
                Origin::Human,
                false,
                None,
            )
            .expect("leaf");
        let _ = b.transition(leaf.id, State::Shaping, "t", None);
        let _ = b.transition(leaf.id, State::Backlog, "t", None);
        let _ = b.transition(leaf.id, State::Claimed, "t", None);
        let _ = b.transition(leaf.id, State::Running, "t", None);

        b.cut_scope(project.id, Some("archived".into()))
            .expect("cut");
        assert_eq!(b.get(leaf.id).unwrap().state, State::Retired);

        b.unarchive_scope(project.id, None).expect("unarchive");
        let leaf_after = b.get(leaf.id).unwrap();
        assert_eq!(
            leaf_after.state,
            State::Backlog,
            "Running prior must remap to Backlog, not revive Running"
        );
        assert!(leaf_after.lease.is_none());
        assert!(!leaf_after.awaiting_dispatch);
    }

    #[test]
    fn unarchive_scope_rejects_non_retired_root() {
        let b = Board::new(
            Schema::default(),
            std::env::temp_dir().join(format!(
                "sandboard-test-unarchive-reject-{}.json",
                std::process::id()
            )),
        );
        let project = b
            .create(None, "Active", "why", None, Origin::Human, true, None)
            .expect("project");
        let err = b.unarchive_scope(project.id, None).unwrap_err();
        assert!(
            err.contains("not retired"),
            "expected non-retired rejection, got {err}"
        );
    }

    #[test]
    fn unarchive_scope_leaf_without_dod_lands_in_shaping() {
        let b = Board::new(
            Schema::default(),
            std::env::temp_dir().join(format!(
                "sandboard-test-unarchive-shaping-{}.json",
                std::process::id()
            )),
        );
        let (project, _seed) = project_with_initial_plan(&b, "No DoD");
        let _ = b.transition(project.id, State::Shaping, "t", None);
        // Claim path requires DoD to reach Backlog; force an in-flight prior
        // via history by claiming a leaf that later loses DoD before cut — or
        // cut a NeedsHuman leaf. NeedsHuman can be reached from Shaping.
        let leaf = b
            .create(
                Some(project.id),
                "Ambiguous",
                "unclear",
                None,
                Origin::Human,
                false,
                None,
            )
            .expect("leaf");
        let _ = b.transition(leaf.id, State::Shaping, "t", None);
        let _ = b.transition(leaf.id, State::NeedsHuman, "t", None);

        b.cut_scope(leaf.id, Some("paused".into())).expect("cut");
        b.unarchive_scope(leaf.id, None).expect("unarchive");
        assert_eq!(
            b.get(leaf.id).unwrap().state,
            State::Shaping,
            "in-flight leaf without DoD must land in Shaping"
        );
    }

    #[test]
    fn board_digest_lists_ready_to_dispatch_cards() {
        let b = Board::new(
            Schema::default(),
            std::env::temp_dir().join(format!(
                "sandboard-test-digest-ready-{}.json",
                std::process::id()
            )),
        );
        let (project, seed) = project_with_initial_plan(&b, "Test Project");
        let initial_id = seed.id;

        let digest_before = b.digest();
        let goal_before = digest_before
            .goals
            .iter()
            .find(|g| g.goal_id == project.id)
            .expect("goal");
        assert_eq!(goal_before.ready_to_dispatch.len(), 1);
        assert_eq!(goal_before.ready_to_dispatch[0].id, initial_id);
        assert_eq!(
            goal_before.ready_to_dispatch[0].title,
            "Initial Plan for Test Project"
        );

        b.enqueue_dispatch(initial_id).expect("dispatch");
        let digest_dispatched = b.digest();
        let goal_dispatched = digest_dispatched
            .goals
            .iter()
            .find(|g| g.goal_id == project.id)
            .expect("goal");
        assert!(goal_dispatched.ready_to_dispatch.is_empty());
    }

    #[test]
    fn retired_leaves_excluded_from_goal_progress() {
        let b = Board::new(
            Schema::default(),
            std::env::temp_dir().join(format!(
                "sandboard-test-retired-leaves-{}.json",
                std::process::id()
            )),
        );
        let (project, seed) = project_with_initial_plan(&b, "Keep sandboxes");
        // Initial plan via init_plan — park it Done so it doesn't muddy the ratio.
        let initial_id = seed.id;
        let done = b
            .create(
                Some(project.id),
                "Finished leaf",
                "why",
                Some("dod".into()),
                Origin::Human,
                false,
                None,
            )
            .expect("done leaf");
        let cut = b
            .create(
                Some(project.id),
                "Cut duplicate",
                "why",
                Some("dod".into()),
                Origin::Human,
                false,
                None,
            )
            .expect("cut leaf");

        for id in [initial_id, done.id, cut.id] {
            // Initial plan is already Backlog; others start Draft.
            let _ = b.transition(id, State::Shaping, "t", None);
            let _ = b.transition(id, State::Backlog, "t", None);
            let _ = b.transition(id, State::Claimed, "t", None);
            let _ = b.transition(id, State::Running, "t", None);
            let _ = b.transition(id, State::Review, "t", None);
            let _ = b.transition(id, State::Done, "t", None);
        }
        b.cut_scope(cut.id, Some("duplicate".into()))
            .expect("retire duplicate");

        let snap = b.snapshot();
        let goal = snap
            .goals
            .iter()
            .find(|g| g.id == project.id)
            .expect("goal");
        assert_eq!(
            (goal.leaves_done, goal.leaves_total),
            (2, 2),
            "retired leaf must not inflate denominator: got {}/{}",
            goal.leaves_done,
            goal.leaves_total
        );
        assert!((goal.progress - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn ready_column_summary_mentions_blockers_in_plain_language() {
        let b = Board::new(
            crate::schema::Schema::default(),
            std::env::temp_dir().join("sandboard-test-summary-blockers.json"),
        );
        let project = b
            .create(
                None,
                "Test Project",
                "Goal",
                None,
                Origin::Human,
                true,
                None,
            )
            .unwrap();

        // create_project auto-seeds Initial plan in Backlog — counts include it.
        let t1 = b
            .create(
                Some(project.id),
                "Task One",
                "Unblocked",
                Some("DOD".into()),
                Origin::Human,
                false,
                None,
            )
            .unwrap();
        let _ = b.transition(t1.id, State::Shaping, "human", None);
        let _ = b.transition(t1.id, State::Backlog, "human", None);

        {
            let s = b.state.read();
            let goal_view = b
                .goal_view(&s, project.id, chrono::Utc::now())
                .expect("goal view");
            let ready_col = goal_view
                .columns
                .iter()
                .find(|c| c.column == Column::Backlog)
                .expect("ready col");
            assert!(
                ready_col.summary.text.contains("2 in backlog"),
                "Initial plan + Task One: {}",
                ready_col.summary.text
            );
            assert!(!ready_col.summary.text.contains("blocked on"));
        }

        let t2 = b
            .create(
                Some(project.id),
                "Task Two",
                "Blocked",
                Some("DOD".into()),
                Origin::Human,
                false,
                None,
            )
            .unwrap();
        let _ = b.transition(t2.id, State::Shaping, "human", None);
        let _ = b.transition(t2.id, State::Backlog, "human", None);
        b.set_blocked_by(t2.id, vec![t1.id]);

        {
            let s = b.state.read();
            let goal_view = b
                .goal_view(&s, project.id, chrono::Utc::now())
                .expect("goal view");
            let ready_col = goal_view
                .columns
                .iter()
                .find(|c| c.column == Column::Backlog)
                .expect("ready col");
            assert!(
                ready_col.summary.text.contains("3 in backlog"),
                "got: {}",
                ready_col.summary.text
            );
            assert!(
                ready_col
                    .summary
                    .text
                    .contains(&format!("1 blocked on #{}: Task One", t1.id)),
                "Summary was: {}",
                ready_col.summary.text
            );
        }
    }

    #[tokio::test]
    async fn sandbox_deleted_on_done_retired_and_item_deletion() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("sandboard-sb-del-test-{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();
        let log_file = dir.join("openshell_calls.log");
        let log_path = log_file.clone();
        let os = crate::openshell::OpenShell::mock(
            move |args| {
                let line = args.join(" ");
                let _ = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&log_path)
                    .and_then(|mut f| {
                        use std::io::Write;
                        writeln!(f, "{line}")
                    });
                crate::openshell::Output {
                    code: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                }
            },
            std::time::Duration::from_secs(5),
        );

        let mut board_raw = Board::new(crate::schema::Schema::default(), dir.join("board.json"));
        board_raw.openshell = Some(os);
        let b = Arc::new(board_raw);

        // 1. Transition to Review keeps sandbox environment
        let p = b
            .create(None, "Project", "intent", None, Origin::Human, true, None)
            .unwrap();
        let t1 = b
            .create(
                Some(p.id),
                "Task 1",
                "intent",
                Some("DOD".into()),
                Origin::Human,
                false,
                None,
            )
            .unwrap();
        b.set_environment(t1.id, Some("sandboard-card-1-a1".into()));
        let _ = b.transition(t1.id, State::Shaping, "human", None);
        let _ = b.transition(t1.id, State::Backlog, "human", None);
        let _ = b.transition(t1.id, State::Claimed, "human", None);
        let _ = b.transition(t1.id, State::Running, "agent", None);
        let _ = b.transition(t1.id, State::Review, "agent", None);

        assert_eq!(
            b.get(t1.id).unwrap().environment.as_deref(),
            Some("sandboard-card-1-a1")
        );

        // 2. Transition to Done clears environment and triggers sandbox deletion
        let _ = b.transition(t1.id, State::Done, "human", None);
        assert_eq!(b.get(t1.id).unwrap().environment, None);

        eventually(|| {
            std::fs::read_to_string(&log_file)
                .unwrap_or_default()
                .contains("sandbox delete sandboard-card-1-a1")
        })
        .await;
        let log_content = std::fs::read_to_string(&log_file).unwrap_or_default();
        assert!(
            log_content.contains("sandbox delete sandboard-card-1-a1"),
            "expected 'sandbox delete sandboard-card-1-a1' in log, got: {log_content}"
        );

        // 3. Transition to Retired clears environment and triggers sandbox deletion
        let t2 = b
            .create(
                Some(p.id),
                "Task 2",
                "intent",
                Some("DOD".into()),
                Origin::Human,
                false,
                None,
            )
            .unwrap();
        b.set_environment(t2.id, Some("sandboard-card-2-a1".into()));
        let _ = b.transition(t2.id, State::Retired, "human", None);
        assert_eq!(b.get(t2.id).unwrap().environment, None);

        eventually(|| {
            std::fs::read_to_string(&log_file)
                .unwrap_or_default()
                .contains("sandbox delete sandboard-card-2-a1")
        })
        .await;
        let log_content = std::fs::read_to_string(&log_file).unwrap_or_default();
        assert!(
            log_content.contains("sandbox delete sandboard-card-2-a1"),
            "expected 'sandbox delete sandboard-card-2-a1' in log, got: {log_content}"
        );

        // 4. Item deletion triggers sandbox deletion
        let t3 = b
            .create(
                Some(p.id),
                "Task 3",
                "intent",
                Some("DOD".into()),
                Origin::Human,
                false,
                None,
            )
            .unwrap();
        b.set_environment(t3.id, Some("sandboard-card-3-a1".into()));
        b.delete_item(t3.id).expect("delete_item succeeds");
        assert!(b.get(t3.id).is_none());

        eventually(|| {
            std::fs::read_to_string(&log_file)
                .unwrap_or_default()
                .contains("sandbox delete sandboard-card-3-a1")
        })
        .await;
        let log_content = std::fs::read_to_string(&log_file).unwrap_or_default();
        assert!(
            log_content.contains("sandbox delete sandboard-card-3-a1"),
            "expected 'sandbox delete sandboard-card-3-a1' in log, got: {log_content}"
        );

        // 5. Halt clears environment and deletes the sandbox
        let t4 = b
            .create(
                Some(p.id),
                "Task 4",
                "intent",
                Some("DOD".into()),
                Origin::Human,
                false,
                None,
            )
            .unwrap();
        let _ = b.transition(t4.id, State::Shaping, "human", None);
        let _ = b.transition(t4.id, State::Backlog, "human", None);
        let _ = b.transition(t4.id, State::Claimed, "human", None);
        b.set_environment(t4.id, Some("sandboard-card-4-a1".into()));
        b.halt(t4.id, Some("start over".into())).expect("halt");
        assert_eq!(b.get(t4.id).unwrap().environment, None);

        eventually(|| {
            std::fs::read_to_string(&log_file)
                .unwrap_or_default()
                .contains("sandbox delete sandboard-card-4-a1")
        })
        .await;
        let log_content = std::fs::read_to_string(&log_file).unwrap_or_default();
        assert!(
            log_content.contains("sandbox delete sandboard-card-4-a1"),
            "expected 'sandbox delete sandboard-card-4-a1' in log, got: {log_content}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn approve_review_with_pr_moves_to_done() {
        let b = Arc::new(Board::new(
            Schema::default(),
            std::env::temp_dir().join(format!(
                "sandboard-test-approve-with-pr-{}.json",
                std::process::id()
            )),
        ));
        let p = b
            .create(
                None,
                "Approve PR",
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
        b.set_pr_url(
            t.id,
            Some("https://github.com/sandboard-app/sandboard/pull/99".into()),
        );

        let item = b.approve_review(t.id).expect("approve");
        assert_eq!(
            item.state,
            State::Done,
            "Approve & Move to Done must complete PR cards"
        );
        // Webhook after Approve is a no-op (idempotent).
        assert!(b
            .complete_for_merged_pr("https://github.com/sandboard-app/sandboard/pull/99", Some(99))
            .is_none());
        assert_eq!(b.get(t.id).unwrap().state, State::Done);
    }

    #[test]
    fn complete_for_merged_pr_matches_normalized_url_and_is_idempotent() {
        let b = Arc::new(Board::new(
            Schema::default(),
            std::env::temp_dir().join(format!(
                "sandboard-test-complete-merged-pr-{}.json",
                std::process::id()
            )),
        ));
        let p = b
            .create(
                None,
                "Merge Proj",
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
                "Feature",
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
        b.set_pr_url(
            t.id,
            Some("https://github.com/sandboard-app/sandboard/pull/55/".into()),
        );

        assert_eq!(
            Board::normalize_pr_url("https://GitHub.com/sandboard-app/sandboard/pull/55/"),
            "https://github.com/sandboard-app/sandboard/pull/55"
        );

        let done_id = b
            .complete_for_merged_pr("https://GitHub.com/sandboard-app/sandboard/pull/55", Some(55))
            .expect("should complete Review card");
        assert_eq!(done_id, t.id);
        assert_eq!(b.get(t.id).unwrap().state, State::Done);

        assert!(
            b.complete_for_merged_pr("https://github.com/sandboard-app/sandboard/pull/55", Some(55))
                .is_none(),
            "idempotent: already Done"
        );
        assert!(
            b.complete_for_merged_pr("https://github.com/sandboard-app/sandboard/pull/56", Some(56))
                .is_none(),
            "no match"
        );
    }

    fn review_leaf(b: &Board, title: &str) -> WorkItem {
        let p = b
            .create(None, "Multi PR Proj", "intent", None, Origin::Human, true, None)
            .unwrap();
        let t = b
            .create(
                Some(p.id),
                title,
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
        t
    }

    #[test]
    fn report_pull_request_appends_without_replacing() {
        let b = Board::new(
            Schema::default(),
            std::env::temp_dir().join(format!(
                "sandboard-test-report-pr-append-{}.json",
                std::process::id()
            )),
        );
        let t = review_leaf(&b, "Two PRs");
        b.report_pull_request(
            t.id,
            crate::model::PullRequest::from_url("https://github.com/sandboard-app/sandboard/pull/1"),
        )
        .unwrap();
        b.report_pull_request(
            t.id,
            crate::model::PullRequest::from_url("https://github.com/other/widgets/pull/2"),
        )
        .unwrap();
        let item = b.get(t.id).unwrap();
        assert_eq!(item.pull_requests.len(), 2);
        assert_eq!(
            item.pr_urls(),
            vec![
                "https://github.com/sandboard-app/sandboard/pull/1",
                "https://github.com/other/widgets/pull/2"
            ]
        );
        // Same URL updates rather than duplicating.
        let mut again = crate::model::PullRequest::from_url("https://github.com/sandboard-app/sandboard/pull/1");
        again.base = Some(crate::model::PullRequestEnd::new("sandboard-app/sandboard", "main"));
        again.head = Some(crate::model::PullRequestEnd::new("sandboard-app/sandboard", "sandboard/card-1"));
        b.report_pull_request(t.id, again).unwrap();
        let item = b.get(t.id).unwrap();
        assert_eq!(item.pull_requests.len(), 2);
        assert!(item.pull_requests[0].has_forge_ends());
        assert_eq!(item.state, State::Review);
    }

    #[test]
    fn complete_for_merged_pr_partial_stays_review_all_merged_done() {
        let b = Board::new(
            Schema::default(),
            std::env::temp_dir().join(format!(
                "sandboard-test-multi-pr-merge-{}.json",
                std::process::id()
            )),
        );
        let t = review_leaf(&b, "Wait for both");
        b.report_pull_request(
            t.id,
            crate::model::PullRequest::from_url("https://github.com/sandboard-app/sandboard/pull/10"),
        )
        .unwrap();
        b.report_pull_request(
            t.id,
            crate::model::PullRequest::from_url("https://github.com/other/widgets/pull/11"),
        )
        .unwrap();

        assert!(
            b.complete_for_merged_pr("https://github.com/sandboard-app/sandboard/pull/10", Some(10))
                .is_none(),
            "one merge must not Done the card"
        );
        let item = b.get(t.id).unwrap();
        assert_eq!(item.state, State::Review);
        assert!(item.pull_requests[0].merged);
        assert!(!item.pull_requests[1].merged);
        assert_eq!(item.pr_url(), Some("https://github.com/other/widgets/pull/11"));

        let done = b
            .complete_for_merged_pr("https://github.com/other/widgets/pull/11", Some(11))
            .expect("all merged");
        assert_eq!(done, t.id);
        assert_eq!(b.get(t.id).unwrap().state, State::Done);
        assert!(b.get(t.id).unwrap().all_pull_requests_merged());
    }

    #[test]
    fn apply_pr_review_feedback_keeps_sibling_prs() {
        let b = Board::new(
            Schema::default(),
            std::env::temp_dir().join(format!(
                "sandboard-test-multi-pr-bounce-{}.json",
                std::process::id()
            )),
        );
        let t = review_leaf(&b, "Bounce one");
        b.report_pull_request(
            t.id,
            crate::model::PullRequest::from_url("https://github.com/sandboard-app/sandboard/pull/20"),
        )
        .unwrap();
        b.report_pull_request(
            t.id,
            crate::model::PullRequest::from_url("https://github.com/other/widgets/pull/21"),
        )
        .unwrap();
        b.complete_for_merged_pr("https://github.com/sandboard-app/sandboard/pull/20", Some(20));

        let id = b
            .apply_pr_review_feedback(
                "https://github.com/other/widgets/pull/21",
                Some(21),
                "CHANGES_REQUESTED",
            )
            .expect("bounce");
        assert_eq!(id, t.id);
        let item = b.get(t.id).unwrap();
        assert_eq!(item.state, State::Backlog);
        assert_eq!(item.pull_requests.len(), 2);
        assert!(item.pull_requests[0].merged);
        assert!(!item.pull_requests[1].merged);
        assert_eq!(item.pull_requests[1].url, "https://github.com/other/widgets/pull/21");
    }

    #[test]
    fn migrate_legacy_singular_pull_request_into_vec() {
        let json = r#"{
            "id": 1,
            "parent": null,
            "title": "legacy",
            "intent": "i",
            "state": "review",
            "origin": {"kind": "human"},
            "created_at": "2026-01-01T00:00:00Z",
            "entered_state_at": "2026-01-01T00:00:00Z",
            "pull_request": {"url": "https://github.com/sandboard-app/sandboard/pull/3"}
        }"#;
        let mut item: WorkItem = serde_json::from_str(json).unwrap();
        item.migrate_legacy_pr_url();
        assert_eq!(item.pull_requests.len(), 1);
        assert_eq!(item.pr_url(), Some("https://github.com/sandboard-app/sandboard/pull/3"));
    }

    #[test]
    fn review_chunk_oldest_uses_unmerged_pr_reported_at() {
        let b = Board::new(
            Schema::default(),
            std::env::temp_dir().join(format!(
                "sandboard-test-oldest-unmerged-{}.json",
                std::process::id()
            )),
        );
        let t = review_leaf(&b, "Stale list");
        let old = Utc::now() - Duration::hours(2);
        let young = Utc::now() - Duration::minutes(5);
        b.set_pull_requests(
            t.id,
            vec![
                crate::model::PullRequest {
                    url: "https://github.com/sandboard-app/sandboard/pull/30".into(),
                    reported_at: Some(old),
                    ..Default::default()
                },
                crate::model::PullRequest {
                    url: "https://github.com/other/widgets/pull/31".into(),
                    reported_at: Some(young),
                    ..Default::default()
                },
            ],
        );
        let item = b.get(t.id).unwrap();
        let age = item.oldest_unmerged_age(Utc::now());
        assert!(
            age >= Duration::minutes(110),
            "oldest unmerged should be ~2h, got {age:?}"
        );
        b.complete_for_merged_pr("https://github.com/sandboard-app/sandboard/pull/30", Some(30));
        let item = b.get(t.id).unwrap();
        let age = item.oldest_unmerged_age(Utc::now());
        assert!(
            age < Duration::minutes(15),
            "after first merge, oldest unmerged is the young PR, got {age:?}"
        );
    }

    /// Review card + CHANGES_REQUESTED → Backlog with a pointer-style steer note
    /// (no review-body dump). COMMENT shares the same path. APPROVED / unknown
    /// URL are no-ops; duplicate apply is safe.
    #[test]
    fn apply_pr_review_feedback_steers_review_to_backlog() {
        let b = Arc::new(Board::new(
            Schema::default(),
            std::env::temp_dir().join(format!(
                "sandboard-test-pr-review-feedback-{}.json",
                std::process::id()
            )),
        ));
        let p = b
            .create(
                None,
                "Review Feedback Proj",
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
                "Feature",
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
        let pr = "https://github.com/sandboard-app/sandboard/pull/261/";
        b.set_pr_url(t.id, Some(pr.into()));
        // Proposal must be cleared like human request_changes — set via propose
        // is blocked when a PR exists, so plant one directly then apply feedback.
        b.set_proposal(
            t.id,
            crate::model::TaskProposal {
                summary: "would dump review body here".into(),
                tasks: vec![crate::model::PlanTaskSpec {
                    key: "a".into(),
                    title: "A".into(),
                    intent: "a".into(),
                    definition_of_done: "a done".into(),
                    blocked_by_keys: vec![],
                    capability: None,
                    item_id: None,
                    repo: None,
                }],
            },
        )
        .expect("plant proposal");
        assert!(b.get(t.id).unwrap().proposal.is_some());

        let id = b
            .apply_pr_review_feedback(
                "https://GitHub.com/sandboard-app/sandboard/pull/261",
                Some(261),
                "CHANGES_REQUESTED",
            )
            .expect("CHANGES_REQUESTED should steer Review → Backlog");
        assert_eq!(id, t.id);
        let item = b.get(t.id).unwrap();
        assert_eq!(item.state, State::Backlog);
        assert!(item.proposal.is_none(), "proposal cleared");
        let note = item
            .notes
            .last()
            .expect("pointer steer note")
            .text
            .clone();
        assert!(
            note.contains("PR review feedback") && note.contains("gh"),
            "pointer-style note expected, got: {note}"
        );
        assert!(
            !note.to_ascii_lowercase().contains("would dump")
                && !note.contains("CHANGES_REQUESTED"),
            "must not embed review body or dump state jargon as the body: {note}"
        );
        assert!(
            note.contains("261") || note.contains("pull/261"),
            "note should identify the PR: {note}"
        );

        // Idempotent: already Backlog — not matched again.
        assert!(
            b.apply_pr_review_feedback(
                "https://github.com/sandboard-app/sandboard/pull/261",
                Some(261),
                "CHANGES_REQUESTED",
            )
            .is_none(),
            "duplicate apply is safe"
        );
        assert_eq!(b.get(t.id).unwrap().notes.len(), item.notes.len());
    }

    #[test]
    fn apply_pr_review_feedback_comment_same_path_as_changes_requested() {
        let b = Arc::new(Board::new(
            Schema::default(),
            std::env::temp_dir().join(format!(
                "sandboard-test-pr-review-comment-{}.json",
                std::process::id()
            )),
        ));
        let p = b
            .create(
                None,
                "Comment Proj",
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
                "Feature",
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
        b.set_pr_url(
            t.id,
            Some("https://github.com/sandboard-app/sandboard/pull/262".into()),
        );

        let id = b
            .apply_pr_review_feedback(
                "https://github.com/sandboard-app/sandboard/pull/262",
                Some(262),
                "COMMENT",
            )
            .expect("COMMENT should steer like CHANGES_REQUESTED");
        assert_eq!(id, t.id);
        let item = b.get(t.id).unwrap();
        assert_eq!(item.state, State::Backlog);
        let note = &item.notes.last().unwrap().text;
        assert!(note.contains("PR review feedback") && note.contains("gh"));
        assert!(!note.contains("please fix the typo in line 12"));
    }

    #[test]
    fn apply_pr_review_feedback_approved_and_unknown_are_noop() {
        let b = Arc::new(Board::new(
            Schema::default(),
            std::env::temp_dir().join(format!(
                "sandboard-test-pr-review-noop-{}.json",
                std::process::id()
            )),
        ));
        let p = b
            .create(
                None,
                "Noop Proj",
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
                "Feature",
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
        b.set_pr_url(
            t.id,
            Some("https://github.com/sandboard-app/sandboard/pull/263".into()),
        );
        let notes_before = b.get(t.id).unwrap().notes.len();

        assert!(
            b.apply_pr_review_feedback(
                "https://github.com/sandboard-app/sandboard/pull/263",
                Some(263),
                "APPROVED",
            )
            .is_none(),
            "APPROVED is a no-op"
        );
        assert!(
            b.apply_pr_review_feedback(
                "https://github.com/sandboard-app/sandboard/pull/263",
                Some(263),
                "dismissed",
            )
            .is_none(),
            "dismissed is a no-op"
        );
        assert_eq!(b.get(t.id).unwrap().state, State::Review);
        assert_eq!(b.get(t.id).unwrap().notes.len(), notes_before);

        assert!(
            b.apply_pr_review_feedback(
                "https://github.com/sandboard-app/sandboard/pull/9999",
                Some(9999),
                "CHANGES_REQUESTED",
            )
            .is_none(),
            "unknown PR URL is a no-op"
        );
        assert_eq!(b.get(t.id).unwrap().state, State::Review);
    }

    #[test]
    fn test_event_sequence_ordering_and_buffer_catchup() {
        let b = Board::new(
            Schema::default(),
            std::env::temp_dir().join(format!("sandboard-test-seq-catchup-{}.json", std::process::id())),
        )
        .with_buffer_capacity(10);

        assert_eq!(b.current_seq(), 0);

        // create_project auto-seeds Initial plan (several Upserts + story).
        let p = b
            .create(None, "Test Seq", "intent", None, Origin::Human, true, None)
            .unwrap();

        let after_create = b.current_seq();
        assert!(after_create >= 1);

        b.story(p.id, "Story line 1".to_string());
        let after_story = b.current_seq();
        assert_eq!(after_story, after_create + 1);

        match b.catch_up(0) {
            CatchUpResult::Events(events) => {
                assert_eq!(events.len() as u64, after_story);
                for (i, ev) in events.iter().enumerate() {
                    assert_eq!(ev.seq(), (i + 1) as u64);
                }
            }
            CatchUpResult::Reset { .. } => panic!("expected events for last_seq 0"),
        }

        match b.catch_up(after_create) {
            CatchUpResult::Events(events) => {
                assert_eq!(events.len(), 1);
                assert_eq!(events[0].seq(), after_story);
            }
            CatchUpResult::Reset { .. } => panic!("expected events for last_seq after_create"),
        }

        match b.catch_up(after_story) {
            CatchUpResult::Events(events) => {
                assert!(events.is_empty());
            }
            CatchUpResult::Reset { .. } => panic!("expected empty events for last_seq after_story"),
        }

        // Overflow buffer capacity 10: emit until current_seq is after_story + 9.
        for i in 0..9 {
            b.story(p.id, format!("Story line extra {i}"));
        }
        let final_seq = after_story + 9;
        assert_eq!(b.current_seq(), final_seq);

        // Catchup from 0 needs seq 1, which was popped once we exceed capacity.
        match b.catch_up(0) {
            CatchUpResult::Reset { seq } => {
                assert_eq!(seq, final_seq);
            }
            CatchUpResult::Events(_) => panic!("expected Reset frame for lagged last_seq 0"),
        }

        // Oldest retained seq is final_seq - 9 (capacity 10 holds final_seq-9 ..= final_seq).
        let oldest_kept = final_seq - 9;
        match b.catch_up(oldest_kept - 1) {
            CatchUpResult::Events(events) => {
                assert_eq!(events.len(), 10);
                assert_eq!(events[0].seq(), oldest_kept);
                assert_eq!(events.last().unwrap().seq(), final_seq);
            }
            CatchUpResult::Reset { .. } => panic!("expected events for last_seq oldest_kept-1"),
        }

        match b.catch_up(final_seq + 9) {
            CatchUpResult::Reset { seq } => {
                assert_eq!(seq, final_seq);
            }
            CatchUpResult::Events(_) => panic!("expected Reset for future last_seq"),
        }
    }

    #[test]
    fn approve_next_linear_chain_handoff_surfaces_sibling() {
        let b = Arc::new(Board::new(
            Schema::default(),
            std::env::temp_dir().join(format!(
                "sandboard-test-approve-next-{}.json",
                std::process::id()
            )),
        ));
        let p = b
            .create(
                None,
                "Linear Chain Project",
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
                "Task 1",
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
                "Task 2",
                "intent 2",
                Some("dod 2".into()),
                Origin::Human,
                false,
                None,
            )
            .unwrap();
        let t3 = b
            .create(
                Some(p.id),
                "Task 3",
                "intent 3",
                Some("dod 3".into()),
                Origin::Human,
                false,
                None,
            )
            .unwrap();

        b.set_blocked_by(t2.id, vec![t1.id]);
        b.set_blocked_by(t3.id, vec![t2.id]);

        let _ = b.transition(t1.id, State::Shaping, "test", None);
        let _ = b.transition(t1.id, State::Backlog, "test", None);
        let _ = b.transition(t1.id, State::Claimed, "agent", None);
        let _ = b.transition(t1.id, State::Running, "agent", None);
        let _ = b.transition(t1.id, State::Review, "agent", None);

        let done1 = b.approve_review(t1.id).expect("approve t1");
        assert_eq!(done1.state, State::Done);

        let unblocked1 = b.newly_unblocked_siblings(t1.id);
        assert_eq!(unblocked1.len(), 1);
        assert_eq!(unblocked1[0].id, t2.id);

        let stories = b.stories_for(p.id);
        assert!(
            stories.iter().any(|s| s
                .text
                .contains(&format!("Unblocked next sibling #{}", t2.id))),
            "Expected story line referencing unblocked sibling #{}",
            t2.id
        );

        let _ = b.transition(t2.id, State::Shaping, "test", None);
        let _ = b.transition(t2.id, State::Backlog, "test", None);
        let _ = b.transition(t2.id, State::Claimed, "agent", None);
        let _ = b.transition(t2.id, State::Running, "agent", None);
        let _ = b.transition(t2.id, State::Review, "agent", None);

        let done2 = b.approve_review(t2.id).expect("approve t2");
        assert_eq!(done2.state, State::Done);

        let unblocked2 = b.newly_unblocked_siblings(t2.id);
        assert_eq!(unblocked2.len(), 1);
        assert_eq!(unblocked2[0].id, t3.id);

        let stories2 = b.stories_for(p.id);
        assert!(
            stories2.iter().any(|s| s
                .text
                .contains(&format!("Unblocked next sibling #{}", t3.id))),
            "Expected story line referencing unblocked sibling #{}",
            t3.id
        );
    }

    #[tokio::test]
    async fn test_heal_completed_epics_auto_closes_project_when_child_tasks_done() {
        let path = std::env::temp_dir().join(format!(
            "sandboard-test-epic-hygiene-{}.json",
            std::process::id()
        ));
        let b = Arc::new(Board::new(Schema::default(), path));
        let p = b
            .create(
                None,
                "Epic Hygiene Project",
                "intent",
                None,
                Origin::Human,
                true,
                None,
            )
            .unwrap();
        let _t1 = b
            .create(
                Some(p.id),
                "Child Task 1",
                "intent 1",
                Some("dod 1".into()),
                Origin::Human,
                false,
                None,
            )
            .unwrap();
        let _t2 = b
            .create(
                Some(p.id),
                "Child Task 2",
                "intent 2",
                Some("dod 2".into()),
                Origin::Human,
                false,
                None,
            )
            .unwrap();

        // Project should be open
        assert_ne!(b.get(p.id).unwrap().state, State::Done);

        // Mark all child tasks (including seeded Initial plan task) as Done
        let children: Vec<ItemId> = {
            let s = b.state.read();
            s.items
                .values()
                .filter(|i| i.parent == Some(p.id))
                .map(|i| i.id)
                .collect()
        };

        for cid in children {
            let _ = b.transition(cid, State::Shaping, "test", None);
            let _ = b.transition(cid, State::Done, "test", None);
        }

        // Run heal_completed_epics
        let healed = b.heal_completed_epics().await;
        assert!(healed > 0, "expected at least 1 project healed");

        assert_eq!(b.get(p.id).unwrap().state, State::Done);
    }

    #[test]
    fn identify_behind_sibling_prs_after_merge_done_without_blind_rebase_queue() {
        let b = Board::new(
            Schema::default(),
            std::env::temp_dir().join(format!("sandboard-test-rebase-{}.json", std::process::id())),
        );
        let project = b
            .create(
                None,
                "Rebase Proj",
                "intent",
                None,
                Origin::Human,
                true,
                None,
            )
            .unwrap();

        let t1 = b
            .create(
                Some(project.id),
                "Task 1",
                "intent 1",
                Some("dod 1".into()),
                Origin::Human,
                false,
                None,
            )
            .unwrap();
        let t2 = b
            .create(
                Some(project.id),
                "Task 2",
                "intent 2",
                Some("dod 2".into()),
                Origin::Human,
                false,
                None,
            )
            .unwrap();

        b.transition(t1.id, State::Shaping, "test", None).unwrap();
        b.transition(t1.id, State::Backlog, "test", None).unwrap();
        b.transition(t1.id, State::Claimed, "agent", None).unwrap();
        b.transition(t1.id, State::Review, "agent", None).unwrap();
        b.set_pr_url(
            t1.id,
            Some("https://github.com/sandboard-app/sandboard/pull/101".into()),
        );

        b.transition(t2.id, State::Shaping, "test", None).unwrap();
        b.transition(t2.id, State::Backlog, "test", None).unwrap();
        b.transition(t2.id, State::Claimed, "agent", None).unwrap();
        b.transition(t2.id, State::Review, "agent", None).unwrap();
        b.set_pr_url(
            t2.id,
            Some("https://github.com/sandboard-app/sandboard/pull/102".into()),
        );

        let behind = b.identify_behind_sibling_prs(t1.id);
        assert_eq!(behind.len(), 1);
        assert_eq!(behind[0].id, t2.id);

        let completed_id = b
            .complete_for_merged_pr("https://github.com/sandboard-app/sandboard/pull/101", Some(101))
            .expect("t1 completed");
        assert_eq!(completed_id, t1.id);

        let t2_updated = b.get(t2.id).unwrap();
        assert_eq!(t2_updated.state, State::Review);
        assert!(
            !t2_updated.rebase_requested,
            "merge→Done must not set rebase_requested before mergeable is observed"
        );
        assert!(!t2_updated.awaiting_dispatch);
        assert!(
            b.list_awaiting_rebase().is_empty(),
            "no catch-up work signal until mergeable observation"
        );
        assert_eq!(
            b.identify_behind_sibling_prs(t1.id)
                .iter()
                .map(|i| i.id)
                .collect::<Vec<_>>(),
            vec![t2.id],
            "sibling remains a catch-up candidate for mergeable observation"
        );
    }

    /// MainAdvanced steers Running only; Review stays Review with no catch-up
    /// signal until mergeable is observed (MERGEABLE = no-op).
    #[test]
    fn notify_main_advanced_leaves_review_as_noop_until_mergeable_observed() {
        let b = Board::new(
            Schema::default(),
            std::env::temp_dir().join(format!("sandboard-test-rebase-main-{}.json", std::process::id())),
        );
        let project = b
            .create(
                None,
                "Rebase Main Proj",
                "intent",
                None,
                Origin::Human,
                true,
                None,
            )
            .unwrap();

        let t1 = b
            .create(
                Some(project.id),
                "Merged Task",
                "intent 1",
                Some("dod 1".into()),
                Origin::Human,
                false,
                None,
            )
            .unwrap();
        let t2 = b
            .create(
                Some(project.id),
                "Behind Task",
                "intent 2",
                Some("dod 2".into()),
                Origin::Human,
                false,
                None,
            )
            .unwrap();

        b.transition(t1.id, State::Shaping, "test", None).unwrap();
        b.transition(t1.id, State::Done, "test", None).unwrap();

        b.transition(t2.id, State::Shaping, "test", None).unwrap();
        b.transition(t2.id, State::Backlog, "test", None).unwrap();
        b.transition(t2.id, State::Claimed, "agent", None).unwrap();
        b.transition(t2.id, State::Review, "agent", None).unwrap();
        b.set_pr_url(
            t2.id,
            Some("https://github.com/sandboard-app/sandboard/pull/202".into()),
        );

        b.notify_main_advanced("sandboard-app/sandboard", "refs/heads/main", Some("sha123".into()));

        let t2_updated = b.get(t2.id).unwrap();
        assert_eq!(t2_updated.state, State::Review);
        assert!(
            !t2_updated.rebase_requested,
            "MainAdvanced must not treat every Review PR as rebase work up front"
        );
        assert!(!t2_updated.awaiting_dispatch);
        assert!(
            b.identify_all_behind_sibling_prs()
                .iter()
                .any(|i| i.id == t2.id),
            "Review PR remains a tip catch-up candidate"
        );
    }

    /// Tip advance identifies Review PRs even without a Done sibling — observation
    /// happens later; Board alone must not set rebase_requested.
    #[test]
    fn notify_main_advanced_identifies_review_without_done_sibling_as_noop() {
        let b = Board::new(
            Schema::default(),
            std::env::temp_dir().join(format!(
                "sandboard-test-rebase-tip-no-done-{}.json",
                std::process::id()
            )),
        );
        let project = b
            .create(
                None,
                "Tip No Done Proj",
                "intent",
                None,
                Origin::Human,
                true,
                None,
            )
            .unwrap();
        let review = b
            .create(
                Some(project.id),
                "Open Review PR",
                "intent",
                Some("dod".into()),
                Origin::Human,
                false,
                None,
            )
            .unwrap();

        b.transition(review.id, State::Shaping, "test", None)
            .unwrap();
        b.transition(review.id, State::Backlog, "test", None)
            .unwrap();
        b.transition(review.id, State::Claimed, "agent", None)
            .unwrap();
        b.transition(review.id, State::Review, "agent", None)
            .unwrap();
        b.set_pr_url(
            review.id,
            Some("https://github.com/sandboard-app/sandboard/pull/404".into()),
        );

        assert!(
            b.identify_all_behind_sibling_prs()
                .iter()
                .any(|i| i.id == review.id),
            "tip-driven identify must include Review PRs without a Done sibling"
        );

        b.notify_main_advanced("sandboard-app/sandboard", "refs/heads/main", Some("tipdeadbeef".into()));

        let after = b.get(review.id).unwrap();
        assert_eq!(after.state, State::Review);
        assert!(
            !after.rebase_requested,
            "MERGEABLE-default: MainAdvanced alone is a Review no-op"
        );
        assert!(!after.awaiting_dispatch);
    }

    /// MainAdvanced steers Running on the advancing upstream; Review waits for
    /// mergeable observation (Review is not parked onto a live steer path).
    #[test]
    fn notify_main_advanced_steers_running_without_queuing_review_rebase() {
        let b = Board::new(
            schema_with_upstream("sandboard-app/sandboard"),
            std::env::temp_dir().join(format!(
                "sandboard-test-rebase-review-and-running-{}.json",
                std::process::id()
            )),
        );
        let project = b
            .create(
                None,
                "Review And Running Proj",
                "intent",
                None,
                Origin::Human,
                true,
                None,
            )
            .unwrap();
        let done = b
            .create(
                Some(project.id),
                "Already Merged",
                "intent",
                Some("dod".into()),
                Origin::Human,
                false,
                None,
            )
            .unwrap();
        let review = b
            .create(
                Some(project.id),
                "Still In Review",
                "intent",
                Some("dod".into()),
                Origin::Human,
                false,
                None,
            )
            .unwrap();
        let running = b
            .create(
                Some(project.id),
                "Live Run",
                "intent",
                Some("dod".into()),
                Origin::Human,
                false,
                None,
            )
            .unwrap();

        b.transition(done.id, State::Shaping, "test", None).unwrap();
        b.transition(done.id, State::Done, "test", None).unwrap();

        b.transition(review.id, State::Shaping, "test", None)
            .unwrap();
        b.transition(review.id, State::Backlog, "test", None)
            .unwrap();
        b.transition(review.id, State::Claimed, "agent", None)
            .unwrap();
        b.transition(review.id, State::Review, "agent", None)
            .unwrap();
        b.set_pr_url(
            review.id,
            Some("https://github.com/sandboard-app/sandboard/pull/505".into()),
        );

        b.transition(running.id, State::Shaping, "test", None)
            .unwrap();
        b.transition(running.id, State::Backlog, "test", None)
            .unwrap();
        b.transition(running.id, State::Claimed, "agent", None)
            .unwrap();
        b.transition(running.id, State::Running, "agent", None)
            .unwrap();

        let steered = b.notify_main_advanced(
            "sandboard-app/sandboard",
            "refs/heads/main",
            Some("bothpaths".into()),
        );
        assert_eq!(steered, vec![running.id]);

        let review_after = b.get(review.id).unwrap();
        assert_eq!(review_after.state, State::Review);
        assert!(
            !review_after.rebase_requested,
            "Review catch-up waits for mergeable observation; MainAdvanced alone is a no-op"
        );
        assert!(
            !review_after
                .notes
                .iter()
                .any(|n| n.text.contains("Main advanced")),
            "Review must not be steered on notify alone: {:?}",
            review_after.notes
        );

        let running_after = b.get(running.id).unwrap();
        assert_eq!(running_after.state, State::Backlog);
        assert!(running_after.awaiting_dispatch);
        assert!(
            running_after.notes.iter().any(|n| {
                n.text.contains("bothpaths")
                    && n.text.contains("fetch")
                    && n.text.contains("upstream/main")
                    && n.text.to_lowercase().contains("rebase")
            }),
            "Running on matching upstream must be steered: {:?}",
            running_after.notes
        );
        let story = b.stories_for(running.id);
        assert!(
            story.iter().any(|l| {
                l.text.contains("bothpaths")
                    && l.text.contains("auto-steered")
                    && l.text.contains("interrupted for rebase")
            }),
            "goal story must describe main-advanced auto-steer: {:?}",
            story
        );
    }

    #[test]
    fn complete_for_merged_pr_by_leaves_sibling_review_for_mergeable_observation() {
        // Shared Board path: webhook and poll actors complete the same way;
        // sibling Review catch-up observes mergeable later (not rebase_requested up front).
        for by in ["github-webhook", "github-poll"] {
            let b = Board::new(
                Schema::default(),
                std::env::temp_dir().join(format!(
                    "sandboard-test-merge-done-sibling-{by}-{}.json",
                    std::process::id()
                )),
            );
            let project = b
                .create(
                    None,
                    "Merge Done Sibling Proj",
                    "intent",
                    None,
                    Origin::Human,
                    true,
                    None,
                )
                .unwrap();
            let merged = b
                .create(
                    Some(project.id),
                    "Merging Now",
                    "intent",
                    Some("dod".into()),
                    Origin::Human,
                    false,
                    None,
                )
                .unwrap();
            let sibling = b
                .create(
                    Some(project.id),
                    "Sibling Review",
                    "intent",
                    Some("dod".into()),
                    Origin::Human,
                    false,
                    None,
                )
                .unwrap();

            for (id, url) in [
                (merged.id, "https://github.com/sandboard-app/sandboard/pull/601"),
                (sibling.id, "https://github.com/sandboard-app/sandboard/pull/602"),
            ] {
                b.transition(id, State::Shaping, "test", None).unwrap();
                b.transition(id, State::Backlog, "test", None).unwrap();
                b.transition(id, State::Claimed, "agent", None).unwrap();
                b.transition(id, State::Review, "agent", None).unwrap();
                b.set_pr_url(id, Some(url.into()));
            }

            let completed = b
                .complete_for_merged_pr_by(
                    "https://github.com/sandboard-app/sandboard/pull/601",
                    Some(601),
                    by,
                )
                .unwrap_or_else(|| panic!("{by}: merged card Done"));
            assert_eq!(completed, merged.id, "{by}");
            assert_eq!(b.get(merged.id).unwrap().state, State::Done, "{by}");

            let sibling_after = b.get(sibling.id).unwrap();
            assert_eq!(sibling_after.state, State::Review, "{by}");
            assert!(
                !sibling_after.rebase_requested,
                "{by}: merge→Done must not set rebase_requested before mergeable observation"
            );
            assert!(!sibling_after.awaiting_dispatch, "{by}");
            assert!(
                b.identify_behind_sibling_prs(merged.id)
                    .iter()
                    .any(|i| i.id == sibling.id),
                "{by}: sibling remains a catch-up candidate"
            );
            let hist_by = b
                .get(merged.id)
                .unwrap()
                .history
                .last()
                .map(|h| h.by.clone())
                .unwrap_or_default();
            assert_eq!(hist_by, by, "{by}: history actor must match ingress");
        }
    }

    #[test]
    fn notify_main_advanced_steers_running_cards_with_fetch_rebase_note() {
        let b = Board::new(
            schema_with_upstream("sandboard-app/sandboard"),
            std::env::temp_dir().join(format!("sandboard-test-main-steer-{}.json", std::process::id())),
        );
        let project = b
            .create(
                None,
                "Steer Main Proj",
                "intent",
                None,
                Origin::Human,
                true,
                None,
            )
            .unwrap();
        let running = b
            .create(
                Some(project.id),
                "Live Task",
                "intent",
                Some("dod".into()),
                Origin::Human,
                false,
                None,
            )
            .unwrap();
        let claimed = b
            .create(
                Some(project.id),
                "Claimed Task",
                "intent",
                Some("dod".into()),
                Origin::Human,
                false,
                None,
            )
            .unwrap();
        let backlog = b
            .create(
                Some(project.id),
                "Idle Task",
                "intent",
                Some("dod".into()),
                Origin::Human,
                false,
                None,
            )
            .unwrap();

        for id in [running.id, claimed.id, backlog.id] {
            b.transition(id, State::Shaping, "test", None).unwrap();
            b.transition(id, State::Backlog, "test", None).unwrap();
        }
        b.transition(running.id, State::Claimed, "agent", None)
            .unwrap();
        b.transition(running.id, State::Running, "agent", None)
            .unwrap();
        b.transition(claimed.id, State::Claimed, "agent", None)
            .unwrap();

        let steered = b.notify_main_advanced(
            "sandboard-app/sandboard",
            "refs/heads/main",
            Some("abcdeadbeef".into()),
        );
        assert_eq!(steered.len(), 2);
        assert!(steered.contains(&running.id));
        assert!(steered.contains(&claimed.id));

        let running_after = b.get(running.id).unwrap();
        assert_eq!(
            running_after.state,
            State::Backlog,
            "park+unpark must bounce the card to Backlog for resume"
        );
        assert!(
            running_after.awaiting_dispatch,
            "unpark must queue the supervisor"
        );
        assert!(!running_after.parked, "unpark clears the park hold");
        assert!(
            running_after.notes.iter().any(|n| {
                n.text.contains("abcdeadbeef")
                    && n.text.contains("fetch")
                    && n.text.contains("upstream/main")
                    && n.text.to_lowercase().contains("rebase")
            }),
            "Running card should have a fetch/rebase steer note with sha: {:?}",
            running_after.notes
        );
        let story = b.stories_for(running.id);
        assert!(
            story.iter().any(|l| {
                l.text.contains("abcdeadbeef")
                    && l.text.contains("auto-steered")
                    && l.text.contains("interrupted for rebase")
            }),
            "goal story must describe main-advanced auto-steer: {:?}",
            story
        );

        let claimed_after = b.get(claimed.id).unwrap();
        assert!(claimed_after.awaiting_dispatch);
        assert!(
            claimed_after
                .notes
                .iter()
                .any(|n| n.text.contains("abcdeadbeef") && n.text.contains("upstream/main")),
            "Claimed cards should also be steered: {:?}",
            claimed_after.notes
        );

        let backlog_after = b.get(backlog.id).unwrap();
        assert!(
            backlog_after.notes.is_empty(),
            "Backlog cards must not get a main-advanced steer: {:?}",
            backlog_after.notes
        );
    }

    #[test]
    fn notify_main_advanced_parks_and_unparks_running_so_steer_takes_effect() {
        let b = Board::new(
            schema_with_upstream("sandboard-app/sandboard"),
            std::env::temp_dir().join(format!(
                "sandboard-test-main-park-unpark-{}.json",
                std::process::id()
            )),
        );
        let project = b
            .create(
                None,
                "Park Unpark Main Proj",
                "intent",
                None,
                Origin::Human,
                true,
                None,
            )
            .unwrap();
        let running = b
            .create(
                Some(project.id),
                "Live Task",
                "intent",
                Some("dod".into()),
                Origin::Human,
                false,
                None,
            )
            .unwrap();
        b.transition(running.id, State::Shaping, "test", None)
            .unwrap();
        b.transition(running.id, State::Backlog, "test", None)
            .unwrap();
        b.transition(running.id, State::Claimed, "agent", None)
            .unwrap();
        b.transition(running.id, State::Running, "agent", None)
            .unwrap();
        b.set_environment(running.id, Some("sandboard-card-150-sandbox".into()));
        b.set_conversation_id(running.id, Some("conv-main-adv".into()));

        b.notify_main_advanced("sandboard-app/sandboard", "refs/heads/main", Some("def456abc".into()));

        let after = b.get(running.id).unwrap();
        assert_eq!(after.state, State::Backlog);
        assert!(
            after.awaiting_dispatch,
            "unpark must leave the card queued for the supervisor"
        );
        assert!(!after.parked);
        assert_eq!(
            after.environment.as_deref(),
            Some("sandboard-card-150-sandbox"),
            "sandbox environment must survive park+unpark"
        );
        assert_eq!(
            after.conversation_id.as_deref(),
            Some("conv-main-adv"),
            "conversation_id must survive park+unpark"
        );
        assert!(
            after.notes.iter().any(|n| {
                n.text.contains("def456abc")
                    && n.text.contains("upstream/main")
                    && n.text.to_lowercase().contains("rebase")
            }),
            "notes must include the main-advanced steer: {:?}",
            after.notes
        );
        let story = b.stories_for(running.id);
        assert!(
            story.iter().any(|l| {
                l.text.contains("def456abc")
                    && l.text.contains("auto-steered")
                    && l.text.contains("interrupted for rebase")
            }),
            "goal story must describe main-advanced auto-steer: {:?}",
            story
        );
    }

    #[test]
    fn notify_main_advanced_coalesce_skips_double_bounce_same_sha() {
        let b = Board::new(
            schema_with_upstream("sandboard-app/sandboard"),
            std::env::temp_dir().join(format!(
                "sandboard-test-main-coalesce-same-{}.json",
                std::process::id()
            )),
        );
        let project = b
            .create(
                None,
                "Coalesce Same Sha Proj",
                "intent",
                None,
                Origin::Human,
                true,
                None,
            )
            .unwrap();
        let running = b
            .create(
                Some(project.id),
                "Live Task",
                "intent",
                Some("dod".into()),
                Origin::Human,
                false,
                None,
            )
            .unwrap();
        b.transition(running.id, State::Shaping, "test", None)
            .unwrap();
        b.transition(running.id, State::Backlog, "test", None)
            .unwrap();
        b.transition(running.id, State::Claimed, "agent", None)
            .unwrap();
        b.transition(running.id, State::Running, "agent", None)
            .unwrap();
        b.set_environment(running.id, Some("sandboard-card-coalesce-sandbox".into()));
        b.set_conversation_id(running.id, Some("conv-coalesce".into()));

        let sha = "cccccccccccccccccccccccccccccccccccccccc";
        let steered = b.notify_main_advanced("sandboard-app/sandboard", "refs/heads/main", Some(sha.into()));
        assert_eq!(steered, vec![running.id]);

        let after_first = b.get(running.id).unwrap();
        let steer_notes_after_first = after_first
            .notes
            .iter()
            .filter(|n| n.text.starts_with("Main advanced ("))
            .count();
        assert_eq!(steer_notes_after_first, 1);

        let steered_again =
            b.notify_main_advanced("sandboard-app/sandboard", "refs/heads/main", Some(sha.into()));
        assert!(
            steered_again.is_empty(),
            "same sha within dispatch window must not re-steer"
        );

        let after_second = b.get(running.id).unwrap();
        assert_eq!(after_second.state, State::Backlog);
        assert!(after_second.awaiting_dispatch);
        assert!(!after_second.parked);
        assert_eq!(
            after_second
                .notes
                .iter()
                .filter(|n| n.text.starts_with("Main advanced ("))
                .count(),
            1,
            "must not append a duplicate steer note: {:?}",
            after_second.notes
        );
        assert_eq!(
            after_second.environment.as_deref(),
            Some("sandboard-card-coalesce-sandbox")
        );
        assert_eq!(
            after_second.conversation_id.as_deref(),
            Some("conv-coalesce")
        );
    }

    #[test]
    fn notify_main_advanced_coalesce_refreshes_note_on_new_sha_without_second_bounce() {
        let b = Board::new(
            schema_with_upstream("sandboard-app/sandboard"),
            std::env::temp_dir().join(format!(
                "sandboard-test-main-coalesce-new-{}.json",
                std::process::id()
            )),
        );
        let project = b
            .create(
                None,
                "Coalesce New Sha Proj",
                "intent",
                None,
                Origin::Human,
                true,
                None,
            )
            .unwrap();
        let running = b
            .create(
                Some(project.id),
                "Live Task",
                "intent",
                Some("dod".into()),
                Origin::Human,
                false,
                None,
            )
            .unwrap();
        b.transition(running.id, State::Shaping, "test", None)
            .unwrap();
        b.transition(running.id, State::Backlog, "test", None)
            .unwrap();
        b.transition(running.id, State::Claimed, "agent", None)
            .unwrap();
        b.transition(running.id, State::Running, "agent", None)
            .unwrap();
        b.set_environment(running.id, Some("sandboard-card-coalesce2-sandbox".into()));
        b.set_conversation_id(running.id, Some("conv-coalesce2".into()));

        let sha1 = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let sha2 = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        b.notify_main_advanced("sandboard-app/sandboard", "refs/heads/main", Some(sha1.into()));

        let steered_again =
            b.notify_main_advanced("sandboard-app/sandboard", "refs/heads/main", Some(sha2.into()));
        assert_eq!(steered_again, vec![running.id]);

        let after = b.get(running.id).unwrap();
        assert_eq!(after.state, State::Backlog);
        assert!(after.awaiting_dispatch, "dispatch must stay queued");
        assert!(!after.parked);
        assert!(
            after
                .notes
                .iter()
                .any(|n| n.text.contains(sha2) && n.text.starts_with("Main advanced (")),
            "latest steer note must reflect the new sha: {:?}",
            after.notes
        );
        assert_eq!(
            after
                .notes
                .iter()
                .filter(|n| n.text.contains("Parked: main advanced"))
                .count(),
            1,
            "coalesce must not park again: {:?}",
            after.notes
        );
        assert_eq!(
            after.environment.as_deref(),
            Some("sandboard-card-coalesce2-sandbox")
        );
        assert_eq!(
            after.conversation_id.as_deref(),
            Some("conv-coalesce2")
        );
        let story = b.stories_for(running.id);
        assert!(
            story.iter().any(|l| l.text.contains(sha2) && l.text.contains("auto-steered")),
            "coalesce refresh on new sha must append goal story: {:?}",
            story
        );
    }

    #[test]
    fn notify_main_advanced_skips_cross_repo_live_cards() {
        let b = Board::new(
            Schema::default(),
            std::env::temp_dir().join(format!(
                "sandboard-test-main-skip-cross-{}.json",
                std::process::id()
            )),
        );
        let project = b
            .create(
                None,
                "Cross Repo Proj",
                "intent",
                None,
                Origin::Human,
                true,
                None,
            )
            .unwrap();
        let other = b
            .create(
                Some(project.id),
                "Other Repo Live",
                "intent",
                Some("dod".into()),
                Origin::Human,
                false,
                None,
            )
            .unwrap();
        b.transition(other.id, State::Shaping, "test", None)
            .unwrap();
        b.transition(other.id, State::Backlog, "test", None)
            .unwrap();
        b.transition(other.id, State::Claimed, "agent", None)
            .unwrap();
        b.transition(other.id, State::Running, "agent", None)
            .unwrap();
        b.set_pr_url(
            other.id,
            Some("https://github.com/other/widgets/pull/42".into()),
        );

        let steered = b.notify_main_advanced(
            "sandboard-app/sandboard",
            "refs/heads/main",
            Some("crossskipsha".into()),
        );
        assert!(steered.is_empty());

        let after = b.get(other.id).unwrap();
        assert_eq!(after.state, State::Running, "card must stay Running");
        assert!(!after.awaiting_dispatch);
        assert!(
            after.notes.is_empty(),
            "no steer note for unrelated upstream: {:?}",
            after.notes
        );
    }

    #[test]
    fn identify_review_prs_for_main_advanced_scopes_by_upstream_repo() {
        let b = Board::new(
            Schema::default(),
            std::env::temp_dir().join(format!(
                "sandboard-test-main-review-scope-{}.json",
                std::process::id()
            )),
        );
        let project = b
            .create(
                None,
                "Review Scope Proj",
                "intent",
                None,
                Origin::Human,
                true,
                None,
            )
            .unwrap();
        let sandboard_review = b
            .create(
                Some(project.id),
                "Sandboard Review",
                "intent",
                Some("dod".into()),
                Origin::Human,
                false,
                None,
            )
            .unwrap();
        let other_review = b
            .create(
                Some(project.id),
                "Other Review",
                "intent",
                Some("dod".into()),
                Origin::Human,
                false,
                None,
            )
            .unwrap();
        for (id, url) in [
            (sandboard_review.id, "https://github.com/sandboard-app/sandboard/pull/701"),
            (other_review.id, "https://github.com/other/widgets/pull/702"),
        ] {
            b.transition(id, State::Shaping, "test", None).unwrap();
            b.transition(id, State::Backlog, "test", None).unwrap();
            b.transition(id, State::Claimed, "agent", None).unwrap();
            b.transition(id, State::Review, "agent", None).unwrap();
            b.set_pr_url(id, Some(url.into()));
        }

        let scoped = b.identify_review_prs_for_main_advanced("sandboard-app/sandboard");
        assert_eq!(scoped.len(), 1);
        assert_eq!(scoped[0].id, sandboard_review.id);

        let other_scope = b.identify_review_prs_for_main_advanced("other/widgets");
        assert_eq!(other_scope.len(), 1);
        assert_eq!(other_scope[0].id, other_review.id);
    }

    #[test]
    fn rebase_clean_keeps_card_in_review() {
        let b = Board::new(
            Schema::default(),
            std::env::temp_dir().join(format!(
                "sandboard-test-rebase-clean-{}.json",
                std::process::id()
            )),
        );
        let project = b
            .create(
                None,
                "Rebase Clean Proj",
                "intent",
                None,
                Origin::Human,
                true,
                None,
            )
            .unwrap();
        let t1 = b
            .create(
                Some(project.id),
                "Task Clean",
                "intent 1",
                Some("dod 1".into()),
                Origin::Human,
                false,
                None,
            )
            .unwrap();

        b.transition(t1.id, State::Shaping, "test", None).unwrap();
        b.transition(t1.id, State::Backlog, "test", None).unwrap();
        b.transition(t1.id, State::Claimed, "agent", None).unwrap();
        b.transition(t1.id, State::Review, "agent", None).unwrap();
        b.set_pr_url(
            t1.id,
            Some("https://github.com/sandboard-app/sandboard/pull/301".into()),
        );

        b.dispatch_rebase(t1.id).unwrap();
        let item = b.get(t1.id).unwrap();
        assert!(item.rebase_requested);

        {
            let mut s = b.state.write();
            let it = s.items.get_mut(&t1.id).unwrap();
            it.last_bounce_reason = Some("prior conflict bounce".into());
            it.last_conflict_files = vec!["src/stale.rs".into()];
        }

        let updated = b.complete_rebase_clean(t1.id).unwrap();
        assert_eq!(updated.state, State::Review);
        assert!(!updated.rebase_requested);
        assert!(!updated.awaiting_dispatch);
        assert_eq!(updated.last_bounce_reason, None);
        assert!(updated.last_conflict_files.is_empty());
    }

    #[test]
    fn rebase_conflict_moves_card_to_backlog_with_bounce_reason_and_conflict_details() {
        let b = Board::new(
            Schema::default(),
            std::env::temp_dir().join(format!(
                "sandboard-test-rebase-conflict-{}.json",
                std::process::id()
            )),
        );
        let project = b
            .create(
                None,
                "Rebase Conflict Proj",
                "intent",
                None,
                Origin::Human,
                true,
                None,
            )
            .unwrap();
        let t1 = b
            .create(
                Some(project.id),
                "Task Conflict",
                "intent 1",
                Some("dod 1".into()),
                Origin::Human,
                false,
                None,
            )
            .unwrap();

        b.transition(t1.id, State::Shaping, "test", None).unwrap();
        b.transition(t1.id, State::Backlog, "test", None).unwrap();
        b.transition(t1.id, State::Claimed, "agent", None).unwrap();
        b.transition(t1.id, State::Review, "agent", None).unwrap();
        b.set_pr_url(
            t1.id,
            Some("https://github.com/sandboard-app/sandboard/pull/302".into()),
        );

        b.dispatch_rebase(t1.id).unwrap();

        let conflicting_files = vec!["src/main.rs".to_string(), "src/store.rs".to_string()];
        let updated = b
            .complete_rebase_conflict(t1.id, &conflicting_files, Some("git rebase conflict"))
            .unwrap();

        assert_eq!(updated.state, State::Backlog);
        assert!(!updated.rebase_requested);
        assert!(!updated.awaiting_dispatch);

        let bounce_reason = updated.last_bounce_reason.expect("bounce reason set");
        assert!(bounce_reason.contains("git rebase conflict"));
        assert!(bounce_reason.contains("src/main.rs"));
        assert!(bounce_reason.contains("src/store.rs"));

        let note = updated
            .notes
            .iter()
            .find(|n| n.text.contains("do-not-re-report-while-CONFLICTING"))
            .expect("binding conflict note");
        assert!(note.text.contains("src/main.rs"));
        assert!(note.text.contains("src/store.rs"));
        assert!(note.text.contains("BINDING"));
    }

    #[test]
    fn report_clears_stale_bounce_fields() {
        let b = Board::new(
            Schema::default(),
            std::env::temp_dir().join(format!(
                "sandboard-test-report-clear-bounce-{}.json",
                std::process::id()
            )),
        );
        let project = b
            .create(
                None,
                "Clear Bounce Proj",
                "intent",
                None,
                Origin::Human,
                true,
                None,
            )
            .unwrap();
        let t1 = b
            .create(
                Some(project.id),
                "Task Clear",
                "intent 1",
                Some("dod 1".into()),
                Origin::Human,
                false,
                None,
            )
            .unwrap();

        b.transition(t1.id, State::Shaping, "test", None).unwrap();
        b.transition(t1.id, State::Backlog, "test", None).unwrap();
        b.transition(t1.id, State::Claimed, "agent", None).unwrap();
        b.transition(t1.id, State::Running, "agent", None).unwrap();

        {
            let mut s = b.state.write();
            let it = s.items.get_mut(&t1.id).unwrap();
            it.last_bounce_reason = Some("stale bounce".into());
            it.last_conflict_files = vec!["src/old.rs".into()];
        }

        let updated = b
            .report(t1.id, "agent", 3, 1, vec!["ci-on-pr".into()])
            .unwrap();
        assert_eq!(updated.state, State::Review);
        assert_eq!(updated.last_bounce_reason, None);
        assert!(updated.last_conflict_files.is_empty());
    }

    #[test]
    fn conflict_bounce_note_names_files_and_forbids_hollow_report() {
        let note = conflict_bounce_note(&["a.rs".into(), "b.rs".into()]);
        assert!(note.contains("BINDING"));
        assert!(note.contains("a.rs"));
        assert!(note.contains("b.rs"));
        assert!(note.contains("do-not-re-report-while-CONFLICTING"));
    }

    #[test]
    fn repeated_rebase_conflict_on_overlapping_files_escalates_to_needs_human() {
        let b = Board::new(
            Schema::default(),
            std::env::temp_dir().join(format!(
                "sandboard-test-rebase-conflict-repeat-{}.json",
                std::process::id()
            )),
        );
        let project = b
            .create(
                None,
                "Repeated Conflict Proj",
                "intent",
                None,
                Origin::Human,
                true,
                None,
            )
            .unwrap();
        let t1 = b
            .create(
                Some(project.id),
                "Task Repeated Conflict",
                "intent 1",
                Some("dod 1".into()),
                Origin::Human,
                false,
                None,
            )
            .unwrap();

        b.transition(t1.id, State::Shaping, "test", None).unwrap();
        b.transition(t1.id, State::Backlog, "test", None).unwrap();
        b.transition(t1.id, State::Claimed, "agent", None).unwrap();
        b.transition(t1.id, State::Review, "agent", None).unwrap();
        b.set_pr_url(
            t1.id,
            Some("https://github.com/sandboard-app/sandboard/pull/303".into()),
        );

        // First conflict -> Backlog
        b.dispatch_rebase(t1.id).unwrap();
        let first_conflict_files = vec!["src/main.rs".to_string(), "src/store.rs".to_string()];
        let updated1 = b
            .complete_rebase_conflict(t1.id, &first_conflict_files, Some("git rebase conflict"))
            .unwrap();
        assert_eq!(updated1.state, State::Backlog);
        assert_eq!(
            updated1.last_conflict_files,
            vec!["src/main.rs", "src/store.rs"]
        );

        // Card is claimed again and moves back to Review
        b.transition(t1.id, State::Claimed, "agent", None).unwrap();
        b.transition(t1.id, State::Review, "agent", None).unwrap();

        // Second conflict on overlapping file "src/main.rs" -> NeedsHuman
        b.dispatch_rebase(t1.id).unwrap();
        let second_conflict_files = vec!["src/main.rs".to_string(), "src/api.rs".to_string()];
        let updated2 = b
            .complete_rebase_conflict(t1.id, &second_conflict_files, Some("git rebase conflict"))
            .unwrap();

        assert_eq!(updated2.state, State::NeedsHuman);
        assert!(!updated2.rebase_requested);
        assert!(!updated2.awaiting_dispatch);

        let esc = updated2.escalation.expect("escalation present");
        assert!(esc.question.contains("Decomposition failure"));
        assert!(esc.question.contains("src/main.rs"));
        assert!(esc.options.len() >= 2);
    }

    #[test]
    fn second_rebase_conflict_on_disjoint_files_returns_to_backlog() {
        let b = Board::new(
            Schema::default(),
            std::env::temp_dir().join(format!(
                "sandboard-test-rebase-conflict-disjoint-{}.json",
                std::process::id()
            )),
        );
        let project = b
            .create(
                None,
                "Disjoint Conflict Proj",
                "intent",
                None,
                Origin::Human,
                true,
                None,
            )
            .unwrap();
        let t1 = b
            .create(
                Some(project.id),
                "Task Disjoint Conflict",
                "intent 1",
                Some("dod 1".into()),
                Origin::Human,
                false,
                None,
            )
            .unwrap();

        b.transition(t1.id, State::Shaping, "test", None).unwrap();
        b.transition(t1.id, State::Backlog, "test", None).unwrap();
        b.transition(t1.id, State::Claimed, "agent", None).unwrap();
        b.transition(t1.id, State::Review, "agent", None).unwrap();
        b.set_pr_url(
            t1.id,
            Some("https://github.com/sandboard-app/sandboard/pull/304".into()),
        );

        // First conflict -> Backlog
        b.dispatch_rebase(t1.id).unwrap();
        let first_conflict_files = vec!["src/main.rs".to_string()];
        let updated1 = b
            .complete_rebase_conflict(t1.id, &first_conflict_files, Some("git rebase conflict"))
            .unwrap();
        assert_eq!(updated1.state, State::Backlog);

        // Card is claimed again and moves back to Review
        b.transition(t1.id, State::Claimed, "agent", None).unwrap();
        b.transition(t1.id, State::Review, "agent", None).unwrap();

        // Second conflict on disjoint file "src/store.rs" -> Backlog (not NeedsHuman)
        b.dispatch_rebase(t1.id).unwrap();
        let second_conflict_files = vec!["src/store.rs".to_string()];
        let updated2 = b
            .complete_rebase_conflict(t1.id, &second_conflict_files, Some("git rebase conflict"))
            .unwrap();

        assert_eq!(updated2.state, State::Backlog);
        assert!(updated2.escalation.is_none());
        assert_eq!(updated2.last_conflict_files, vec!["src/store.rs"]);
    }

    const SEED_POLICY_YAML: &str =
        "version: 1\n# seed-policy\nfilesystem_policy:\n  include_workdir: true\n";
    const SEED_POLICY_ID: &str = "seed-policy";

    fn ensure_seed_policy(b: &Board) {
        b.upsert_openshell_policy(OpenShellPolicy {
            id: SEED_POLICY_ID.into(),
            name: "Seed".into(),
            yaml: SEED_POLICY_YAML.into(),
        })
        .expect("seed policy");
    }

    fn upsert_test_profile(b: &Board, id: &str, name: &str, image: &str) -> SandboxProfile {
        ensure_seed_policy(b);
        b.upsert_sandbox_profile(SandboxProfile {
            id: id.into(),
            name: name.into(),
            image: image.into(),
            policy_id: SEED_POLICY_ID.into(),
            policy_inline_legacy: None,
            cpu: Some("2".into()),
            memory: Some("4Gi".into()),
            engine: Some("cursor".into()),
            model: None,
            provider_names: Vec::new(),
            mcp_server_ids: Vec::new(),
            env: Default::default(),
            prompt: None,
            shipped: false,
        })
        .expect("upsert profile")
    }

    #[test]
    fn first_sandbox_profile_becomes_default_and_cockpit_inherits() {
        let b = Board::new(
            Schema::default(),
            std::env::temp_dir().join("sandboard-test-sbx-first.json"),
        );
        assert!(b.list_sandbox_profiles().is_empty());
        let p = upsert_test_profile(&b, "default", "Default", "seed-image:test");
        assert_eq!(b.list_sandbox_profiles().len(), 1);
        assert_eq!(p.image, "seed-image:test");
        assert_eq!(b.default_sandbox_profile_id().as_deref(), Some("default"));
        assert!(b.cockpit_sandbox_profile_id().is_none());
        let resolved = b.resolve_cockpit_sandbox_create();
        assert_eq!(resolved.profile_id.as_deref(), Some("default"));
        assert_eq!(resolved.image, "seed-image:test");
    }

    #[test]
    fn explicit_cockpit_profile_overrides_default() {
        let b = Board::new(
            Schema::default(),
            std::env::temp_dir().join(format!(
                "sandboard-test-sbx-cockpit-override-{}",
                std::process::id()
            )),
        );
        upsert_test_profile(&b, "default", "Default", "worker:1");
        upsert_test_profile(&b, "heavy", "Heavy", "heavy:1");
        b.set_cockpit_sandbox_profile("heavy").unwrap();
        assert_eq!(b.cockpit_sandbox_profile_id().as_deref(), Some("heavy"));
        assert_eq!(
            b.resolve_cockpit_sandbox_create().profile_id.as_deref(),
            Some("heavy")
        );
        b.clear_cockpit_sandbox_profile();
        assert!(b.cockpit_sandbox_profile_id().is_none());
        assert_eq!(
            b.resolve_cockpit_sandbox_create().profile_id.as_deref(),
            Some("default")
        );
    }

    #[test]
    fn migrate_github_app_provider_name_rewrites_row_and_attaches() {
        let b = Board::new(
            Schema::default(),
            std::env::temp_dir().join(format!(
                "sandboard-test-ghapp-rename-{}",
                std::process::id()
            )),
        );
        upsert_test_profile(&b, "default", "Default", AgentConfig::default().image.as_str());
        b.upsert_openshell_provider(OpenShellProviderDesired {
            name: "github".into(),
            provider_type: "github".into(),
            config: BTreeMap::new(),
            credentials_sealed: None,
            credential_keys: vec!["GH_TOKEN".into()],
            refresh: None,
        });
        {
            let mut p = b.get_sandbox_profile("default").expect("default");
            p.provider_names = vec!["github".into(), "vertex".into()];
            b.upsert_sandbox_profile(p).expect("attach");
        }
        assert!(b.migrate_github_app_provider_name());
        let providers = b.openshell_providers();
        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0].name, "github-app");
        assert_eq!(providers[0].provider_type, "github-app");
        assert_eq!(
            b.get_sandbox_profile("default")
                .expect("default")
                .provider_names,
            vec!["github-app".to_string(), "vertex".to_string()]
        );
        assert!(!b.migrate_github_app_provider_name());
    }

    #[test]
    fn migrate_github_app_sealed_into_provider_moves_bundle() {
        use crate::github_app::{CONFIG_APP_ID, CONFIG_INSTALLATION_ID, CRED_PRIVATE_KEY};
        use crate::secrets::{open_string_map, seal_github_app, GitHubAppBundle};

        let dir = std::env::temp_dir().join(format!(
            "sandboard-test-ghapp-seal-migrate-{}",
            std::process::id()
        ));
        let _ = std::fs::create_dir_all(&dir);
        let key_path = dir.join("master.key");
        let _env = crate::secrets::master_key_env::Guard::with_key_path(&key_path);
        let b = Board::new(Schema::default(), dir.join("board.json"));
        let pem = include_str!("testdata/github_app_test_rsa.pem");
        let sealed = seal_github_app(&GitHubAppBundle {
            app_id: "4242".into(),
            private_key_pem: pem.into(),
            webhook_secret: "whsec".into(),
            ..Default::default()
        })
        .expect("seal");
        {
            let mut s = b.state.write();
            s.github_app_sealed = Some(sealed);
            s.github_app_installation_id = Some(99);
        }
        assert!(b.migrate_github_app_sealed_into_provider());
        assert!(b.github_app_sealed().is_none());
        assert!(b.state.read().github_app_installation_id.is_none());
        let p = b
            .openshell_providers()
            .into_iter()
            .find(|p| p.name == "github-app")
            .expect("provider");
        assert_eq!(p.provider_type, "github-app");
        assert_eq!(p.config.get(CONFIG_APP_ID).map(String::as_str), Some("4242"));
        assert_eq!(
            p.config.get(CONFIG_INSTALLATION_ID).map(String::as_str),
            Some("99")
        );
        let map = open_string_map(p.credentials_sealed.as_deref().unwrap()).expect("open");
        assert!(map.get(CRED_PRIVATE_KEY).is_some_and(|s| s.contains("BEGIN")));
        assert_eq!(b.github_app_installation_id(), Some(99));
        assert!(b.github_app_status().complete);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn prune_sandbox_attach_names_drops_unknown_providers() {
        let b = Board::new(
            Schema::default(),
            std::env::temp_dir().join(format!(
                "sandboard-test-sbx-prune-attach-{}",
                std::process::id()
            )),
        );
        upsert_test_profile(&b, "default", "Default", AgentConfig::default().image.as_str());
        b.upsert_openshell_provider(OpenShellProviderDesired {
            name: "github".into(),
            provider_type: "github".into(),
            config: BTreeMap::new(),
            credentials_sealed: None,
            credential_keys: vec![],
            refresh: None,
        });
        {
            let mut p = b.get_sandbox_profile("default").expect("default");
            p.provider_names = vec![
                "github".into(),
                "vertex".into(),
                "antigravity".into(),
            ];
            b.upsert_sandbox_profile(p).expect("wishlist");
        }
        assert!(b.prune_sandbox_attach_names());
        assert_eq!(
            b.get_sandbox_profile("default")
                .expect("default")
                .provider_names,
            vec!["github".to_string()]
        );
        assert!(!b.prune_sandbox_attach_names());
    }

    fn agents_with_repo() -> AgentConfig {
        AgentConfig {
            repo: crate::schema::RepoConfig {
                upstream: "acme/widgets".into(),
                fork: "bot/widgets".into(),
                base: "main".into(),
            },
            ..Default::default()
        }
    }

    fn schema_with_upstream(upstream: &str) -> Schema {
        let mut schema = Schema::default();
        schema.execution.agents.repo.upstream = upstream.into();
        schema
    }

    #[test]
    fn workspace_binding_seeds_forge_when_unbound() {
        let b = Board::new(
            Schema::default(),
            std::env::temp_dir().join(format!("sandboard-test-ws-seed-{}.json", std::process::id())),
        );
        assert!(b.workspace_binding().is_none());
        assert!(b.seed_workspace_binding_from(&agents_with_repo()));
        let ws = b.workspace_binding().expect("seeded");
        assert_eq!(ws.forge, "github");
        // Second seed is a no-op once forge is set.
        assert!(!b.seed_workspace_binding_from(&agents_with_repo()));
        // Work remotes still come from yaml, not Settings.
        let mut schema = Schema::default();
        schema.execution.agents = agents_with_repo();
        let b2 = Board::new(
            schema,
            std::env::temp_dir().join(format!(
                "sandboard-test-ws-yaml-repo-{}.json",
                std::process::id()
            )),
        );
        let repo = b2.yaml_work_repo().expect("yaml work repo");
        assert_eq!(repo.upstream, "acme/widgets");
        assert_eq!(repo.fork, "bot/widgets");
    }

    #[test]
    fn agents_overlay_is_yaml_passthrough_without_workspace_remotes() {
        let b = Board::new(
            Schema::default(),
            std::env::temp_dir().join(format!("sandboard-test-ws-fail-{}.json", std::process::id())),
        );
        assert!(
            b.yaml_work_repo().is_none(),
            "empty yaml has no work remotes"
        );

        b.set_workspace_binding(WorkspaceBinding {
            forge: "github".into(),
        })
        .expect("forge binding");
        // Forge binding does not invent work remotes.
        assert!(b.yaml_work_repo().is_none());

        let agents = AgentConfig {
            ..Default::default()
        };
        let overlaid = b.agents_with_workspace(&agents);
        assert!(overlaid.repo.upstream.is_empty());
    }

    #[test]
    fn agent_runtime_seeds_from_defaults_and_overlays_effective_agents() {
        let mut schema = Schema::default();
        schema.execution.agents = AgentConfig {
            // Yaml create/runtime knobs must not win over compiled seed defaults.
            engine: "agy".into(),
            max_concurrent: 7,
            agent_timeout_secs: 60,
            max_attempts: 9,
            image: "yaml-only:1".into(),
            ..Default::default()
        };
        let b = Board::new(
            schema,
            std::env::temp_dir().join(format!(
                "sandboard-test-agent-rt-seed-{}.json",
                std::process::id()
            )),
        );
        assert!(b.agent_runtime().is_none());
        assert!(b.seed_agent_runtime_if_empty());
        let seeded = b.agent_runtime().expect("seeded");
        assert_eq!(seeded.engine, "cursor");
        let compiled_rt = AgentRuntimeConfig::default();
        assert_eq!(seeded.engine, compiled_rt.engine);
        assert_eq!(seeded.max_concurrent, compiled_rt.max_concurrent);
        assert_eq!(seeded.agent_timeout_secs, compiled_rt.agent_timeout_secs);
        assert_eq!(seeded.max_attempts, compiled_rt.max_attempts);
        assert!(!b.seed_agent_runtime_if_empty(), "second seed is a no-op");

        // Explicit from() helper still maps AgentConfig knobs (tests / callers).
        let other = Board::new(
            Schema::default(),
            std::env::temp_dir().join(format!(
                "sandboard-test-agent-rt-from-{}.json",
                std::process::id()
            )),
        );
        assert!(other.seed_agent_runtime_from(&AgentConfig {
            engine: "claude".into(),
            max_concurrent: 3,
            ..Default::default()
        }));
        assert_eq!(
            other.agent_runtime().expect("from").engine,
            "claude"
        );

        b.set_agent_runtime(AgentRuntimeConfig {
            engine: "agy".into(),
            max_concurrent: 1,
            agent_timeout_secs: 900,
            max_attempts: 2,
            ..Default::default()
        });
        let eff = b.effective_agents();
        assert_eq!(eff.engine, "agy");
        assert_eq!(eff.max_concurrent, 1);
        assert_eq!(eff.agent_timeout_secs, 900);
        // Create knobs stay compiled defaults (profiles own live create-spec).
        assert_eq!(eff.image, AgentConfig::default().image);
        assert_ne!(eff.image, "yaml-only:1");
    }

    #[test]
    fn resolve_card_repo_url_only_is_same_repo_stub() {
        let mut schema = Schema::default();
        schema.execution.agents = agents_with_repo();
        // yaml default upstream differs from PR target — PR wins.
        schema.execution.agents.repo.upstream = "acme/default".into();
        schema.execution.agents.repo.fork = "bot/default".into();
        schema.execution.agents.repo.base = "develop".into();
        let b = Board::new(
            schema,
            std::env::temp_dir().join(format!("sandboard-test-resolve-pr-{}.json", std::process::id())),
        );
        b.set_workspace_binding(WorkspaceBinding {
            forge: "github".into(),
        })
        .unwrap();

        let p = b
            .create(
                None,
                "Other Repo Proj",
                "why",
                None,
                Origin::Human,
                true,
                None,
            )
            .unwrap();
        let t = b
            .create(
                Some(p.id),
                "Feature",
                "intent",
                Some("dod".into()),
                Origin::Human,
                false,
                None,
            )
            .unwrap();
        b.set_pr_url(
            t.id,
            Some("https://github.com/other/widgets/pull/99".into()),
        );

        let repo = b.resolve_card_repo(t.id).expect("resolve").expect("bound");
        // URL alone → same-repo stub until base/head reported.
        assert_eq!(repo.upstream, "other/widgets");
        assert_eq!(repo.fork, "other/widgets");
        assert!(!repo.uses_cross_fork());
        assert_eq!(repo.base, "main");
    }

    #[test]
    fn resolve_card_repo_uses_pull_request_base_head() {
        let b = Board::new(
            Schema::default(),
            std::env::temp_dir().join(format!(
                "sandboard-test-resolve-prbind-{}.json",
                std::process::id()
            )),
        );
        let p = b
            .create(None, "P", "why", None, Origin::Human, true, None)
            .unwrap();
        let t = b
            .create(
                Some(p.id),
                "T",
                "intent",
                Some("dod".into()),
                Origin::Human,
                false,
                None,
            )
            .unwrap();
        b.set_pull_request(
            t.id,
            Some(crate::model::PullRequest {
                url: "https://github.com/other/widgets/pull/3".into(),
                base: Some(crate::model::PullRequestEnd::new(
                    "other/widgets",
                    "develop",
                )),
                head: Some(crate::model::PullRequestEnd::new(
                    "bot/widgets",
                    "sandboard/card-1",
                )),
                ..Default::default()
            }),
        );
        let repo = b.resolve_card_repo(t.id).unwrap().unwrap();
        assert_eq!(repo.upstream, "other/widgets");
        assert_eq!(repo.fork, "bot/widgets");
        assert_eq!(repo.base, "develop");
        assert!(repo.uses_cross_fork());
    }

    #[test]
    fn resolve_card_repo_first_run_is_unbound() {
        let b = Board::new(
            Schema::default(),
            std::env::temp_dir().join(format!(
                "sandboard-test-resolve-unbound-{}.json",
                std::process::id()
            )),
        );
        let p = b
            .create(None, "P", "why", None, Origin::Human, true, None)
            .unwrap();
        let t = b
            .create(
                Some(p.id),
                "T",
                "intent",
                Some("dod".into()),
                Origin::Human,
                false,
                None,
            )
            .unwrap();
        assert!(b.resolve_card_repo(t.id).unwrap().is_none());
    }

    #[test]
    fn resolve_card_repo_ignores_legacy_task_repo_field() {
        let b = Board::new(
            Schema::default(),
            std::env::temp_dir().join(format!(
                "sandboard-test-resolve-task-repo-{}.json",
                std::process::id()
            )),
        );
        let p = b
            .create(None, "P", "why", None, Origin::Human, true, None)
            .unwrap();
        let t = b
            .create(
                Some(p.id),
                "T",
                "intent",
                Some("dod".into()),
                Origin::Human,
                false,
                None,
            )
            .unwrap();
        b.set_task_repo(
            t.id,
            Some(RepoConfig {
                upstream: "acme/widgets".into(),
                fork: "bot/widgets".into(),
                base: "develop".into(),
            }),
        )
        .expect("set_task_repo writes unused field");
        assert!(
            b.resolve_card_repo(t.id).unwrap().is_none(),
            "WorkItem.repo alone does not bind remotes"
        );
    }

    #[test]
    fn resolve_card_repo_uses_pull_request_only() {
        let b = Board::new(
            Schema::default(),
            std::env::temp_dir().join(format!(
                "sandboard-test-resolve-pr-wins-{}.json",
                std::process::id()
            )),
        );
        let p = b
            .create(None, "P", "why", None, Origin::Human, true, None)
            .unwrap();
        let t = b
            .create(
                Some(p.id),
                "T",
                "intent",
                Some("dod".into()),
                Origin::Human,
                false,
                None,
            )
            .unwrap();
        b.set_task_repo(
            t.id,
            Some(RepoConfig {
                upstream: "acme/from-task".into(),
                fork: String::new(),
                base: "main".into(),
            }),
        )
        .unwrap();
        b.set_pull_request(
            t.id,
            Some(crate::model::PullRequest {
                url: "https://github.com/other/widgets/pull/3".into(),
                base: Some(crate::model::PullRequestEnd::new(
                    "other/widgets",
                    "develop",
                )),
                head: Some(crate::model::PullRequestEnd::new(
                    "bot/widgets",
                    "sandboard/card-1",
                )),
                ..Default::default()
            }),
        );

        let repo = b.resolve_card_repo(t.id).unwrap().unwrap();
        assert_eq!(repo.upstream, "other/widgets");
        assert_eq!(repo.fork, "bot/widgets");
        assert_eq!(repo.base, "develop");
        assert_ne!(repo.upstream, "acme/from-task");
    }

    #[test]
    fn resolve_card_repo_sibling_prs_resolve_independently() {
        let b = Board::new(
            Schema::default(),
            std::env::temp_dir().join(format!(
                "sandboard-test-resolve-siblings-{}.json",
                std::process::id()
            )),
        );
        let p = b
            .create(
                None,
                "Multi-repo Proj",
                "why",
                None,
                Origin::Human,
                true,
                None,
            )
            .unwrap();
        let a = b
            .create(
                Some(p.id),
                "Task A",
                "intent-a",
                Some("dod".into()),
                Origin::Human,
                false,
                None,
            )
            .unwrap();
        let c = b
            .create(
                Some(p.id),
                "Task C",
                "intent-c",
                Some("dod".into()),
                Origin::Human,
                false,
                None,
            )
            .unwrap();
        b.set_pull_request(
            a.id,
            Some(crate::model::PullRequest {
                url: "https://github.com/acme/frontend/pull/1".into(),
                base: Some(crate::model::PullRequestEnd::new("acme/frontend", "main")),
                head: Some(crate::model::PullRequestEnd::new("acme/frontend", "sandboard/a")),
                ..Default::default()
            }),
        );
        b.set_pull_request(
            c.id,
            Some(crate::model::PullRequest {
                url: "https://github.com/acme/backend/pull/2".into(),
                base: Some(crate::model::PullRequestEnd::new("acme/backend", "develop")),
                head: Some(crate::model::PullRequestEnd::new("bot/backend", "sandboard/c")),
                ..Default::default()
            }),
        );

        let ra = b.resolve_card_repo(a.id).unwrap().unwrap();
        let rc = b.resolve_card_repo(c.id).unwrap().unwrap();
        assert_eq!(ra.upstream, "acme/frontend");
        assert_eq!(ra.base, "main");
        assert_eq!(rc.upstream, "acme/backend");
        assert_eq!(rc.fork, "bot/backend");
        assert_eq!(rc.base, "develop");
        assert!(b.resolve_card_repo(p.id).unwrap().is_none());
    }

    /// After report, `pull_request` must yield complete remotes for resume briefings.
    #[test]
    fn resolve_card_repo_pull_request_is_complete_for_resume() {
        let b = Board::new(
            Schema::default(),
            std::env::temp_dir().join(format!(
                "sandboard-test-resolve-preclone-{}.json",
                std::process::id()
            )),
        );
        let p = b
            .create(None, "P", "why", None, Origin::Human, true, None)
            .unwrap();
        let t = b
            .create(
                Some(p.id),
                "T",
                "intent",
                Some("dod".into()),
                Origin::Human,
                false,
                None,
            )
            .unwrap();
        b.set_pull_request(
            t.id,
            Some(crate::model::PullRequest {
                url: "https://github.com/acme/widgets/pull/1".into(),
                base: Some(crate::model::PullRequestEnd::new("acme/widgets", "main")),
                head: Some(crate::model::PullRequestEnd::new("acme/widgets", "sandboard/t")),
                ..Default::default()
            }),
        );

        let mut agents = b.effective_agents();
        match b.resolve_card_repo(t.id) {
            Ok(Some(repo)) => agents.repo = repo,
            Ok(None) => agents.repo = Default::default(),
            Err(e) => panic!("resolve: {e}"),
        }
        assert!(
            agents.repo.is_complete(),
            "resume remotes gate (is_complete) must pass for pull_request"
        );
        assert_eq!(agents.repo.clone_target(), "acme/widgets");

        let u = b
            .create(
                Some(p.id),
                "Unbound",
                "intent",
                Some("dod".into()),
                Origin::Human,
                false,
                None,
            )
            .unwrap();
        match b.resolve_card_repo(u.id) {
            Ok(Some(repo)) => agents.repo = repo,
            Ok(None) => agents.repo = Default::default(),
            Err(e) => panic!("resolve unbound: {e}"),
        }
        assert!(!agents.repo.is_complete());
    }

    #[test]
    fn parse_github_pr_url_ok() {
        assert_eq!(
            parse_github_pr_url("https://github.com/Acme/Widgets/pull/42"),
            Some(("Acme/Widgets".into(), 42))
        );
        assert_eq!(
            parse_github_pr_url("https://GitHub.com/acme/widgets/pull/7/"),
            Some(("acme/widgets".into(), 7))
        );
        assert_eq!(parse_github_pr_url("not-a-url"), None);
    }

    #[test]
    fn complete_for_merged_pr_two_owners_independent_of_workspace_upstream() {
        let b = Board::new(
            Schema::default(),
            std::env::temp_dir().join(format!(
                "sandboard-test-multi-pr-complete-{}.json",
                std::process::id()
            )),
        );
        b.set_workspace_binding(WorkspaceBinding {
            forge: "github".into(),
        })
        .unwrap();

        let p = b
            .create(None, "Multi", "why", None, Origin::Human, true, None)
            .unwrap();
        let make_review = |title: &str, pr: &str| {
            let t = b
                .create(
                    Some(p.id),
                    title,
                    "intent",
                    Some("dod".into()),
                    Origin::Human,
                    false,
                    None,
                )
                .unwrap();
            b.transition(t.id, State::Shaping, "test", None).unwrap();
            b.transition(t.id, State::Backlog, "test", None).unwrap();
            b.transition(t.id, State::Claimed, "agent", None).unwrap();
            b.transition(t.id, State::Running, "agent", None).unwrap();
            b.transition(t.id, State::Review, "agent", None).unwrap();
            b.set_pr_url(t.id, Some(pr.into()));
            t.id
        };
        let a = make_review("A", "https://github.com/alpha/one/pull/1");
        let c = make_review("C", "https://github.com/charlie/two/pull/2");

        assert_eq!(
            b.complete_for_merged_pr("https://github.com/alpha/one/pull/1", Some(1)),
            Some(a)
        );
        assert_eq!(b.get(a).unwrap().state, State::Done);
        assert_eq!(b.get(c).unwrap().state, State::Review);

        assert_eq!(
            b.complete_for_merged_pr("https://github.com/charlie/two/pull/2", Some(2)),
            Some(c)
        );
        assert_eq!(b.get(c).unwrap().state, State::Done);
        // Forge binding is independent of PR completion.
        assert_eq!(b.workspace_binding().unwrap().forge, "github");
    }

    #[test]
    fn workspace_binding_persists_in_json_roundtrip() {
        let dir = std::env::temp_dir().join(format!("sandboard-test-ws-persist-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sandboard.json");
        {
            let mut schema = Schema::default();
            schema.execution.agents = agents_with_repo();
            let b = Board::new(schema, path.clone());
            assert!(b.seed_workspace_binding_if_empty());
            b.dirty.store(true, Ordering::Relaxed);
            b.flush();
        }
        let restored = Board::load_or_new(Schema::default(), path);
        let ws = restored.workspace_binding().expect("restored workspace");
        assert_eq!(ws.forge, "github");
        assert!(!restored.seed_workspace_binding_if_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn workspace_binding_legacy_json_ignores_upstream_and_beads_keys() {
        let ws: WorkspaceBinding = serde_json::from_str(
            r#"{"forge":"github","upstream":"old/work","beads_sync_repo":"x/y"}"#,
        )
        .unwrap();
        assert_eq!(ws.forge, "github");
        let wire = serde_json::to_value(&ws).unwrap();
        assert!(wire.get("beads_sync_repo").is_none());
        assert!(wire.get("upstream").is_none());
    }

    #[test]
    fn task_repo_persists_in_json_roundtrip_and_snapshot() {
        let dir = std::env::temp_dir().join(format!(
            "sandboard-test-task-repo-persist-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sandboard.json");
        let task_id = {
            let b = Board::new(Schema::default(), path.clone());
            let project = b
                .create(None, "Bound Proj", "why", None, Origin::Human, true, None)
                .expect("project");
            assert!(
                project.repo.is_none(),
                "Projects are containers; remotes live on Tasks"
            );
            let task = b
                .create(
                    Some(project.id),
                    "Impl",
                    "do it",
                    Some("done".into()),
                    Origin::Human,
                    false,
                    None,
                )
                .expect("task");
            let bound = b
                .set_task_repo(
                    task.id,
                    Some(RepoConfig {
                        upstream: "acme/widgets".into(),
                        fork: "bot/widgets".into(),
                        base: "develop".into(),
                    }),
                )
                .expect("set_task_repo");
            assert_eq!(bound.repo.as_ref().unwrap().upstream, "acme/widgets");
            assert_eq!(bound.repo.as_ref().unwrap().fork, "bot/widgets");
            assert_eq!(bound.repo.as_ref().unwrap().base, "develop");

            let snap = b.snapshot();
            let snap_task = snap
                .items
                .iter()
                .find(|i| i.id == task.id)
                .expect("in snap");
            assert_eq!(
                snap_task.repo.as_ref().map(|r| r.upstream.as_str()),
                Some("acme/widgets")
            );
            let snap_proj = snap
                .items
                .iter()
                .find(|i| i.id == project.id)
                .expect("project in snap");
            assert!(snap_proj.repo.is_none());

            b.dirty.store(true, Ordering::Relaxed);
            b.flush();
            task.id
        };
        let restored = Board::load_or_new(Schema::default(), path);
        let task = restored.get(task_id).expect("restored task");
        let repo = task.repo.expect("task repo survived flush");
        assert_eq!(repo.upstream, "acme/widgets");
        assert_eq!(repo.fork, "bot/widgets");
        assert_eq!(repo.base, "develop");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn project_has_no_repo_field_and_set_task_repo_refuses() {
        let b = Board::new(
            Schema::default(),
            std::env::temp_dir().join(format!(
                "sandboard-test-project-no-repo-{}.json",
                std::process::id()
            )),
        );
        let project = b
            .create(None, "No Repo Proj", "why", None, Origin::Human, true, None)
            .expect("project");
        assert!(project.repo.is_none());
        let wire = serde_json::to_value(&project).expect("serialize");
        assert!(
            wire.get("repo").is_none(),
            "Projects omit repo when unset (no product_repo)"
        );
        assert!(
            wire.get("product_repo").is_none(),
            "Projects must not grow a product_repo field"
        );
        let err = b
            .set_task_repo(
                project.id,
                Some(RepoConfig {
                    upstream: "acme/widgets".into(),
                    fork: String::new(),
                    base: "main".into(),
                }),
            )
            .expect_err("Projects refuse task repo");
        assert!(
            err.contains("Project"),
            "error should name Project refusal: {err}"
        );
        assert!(b.get(project.id).unwrap().repo.is_none());

        // WorkspaceBinding carries forge only.
        let ws = WorkspaceBinding {
            forge: "github".into(),
        };
        let ws_wire = serde_json::to_value(&ws).unwrap();
        assert!(ws_wire.get("upstream").is_none());
        assert!(ws_wire.get("fork").is_none());
        assert!(ws_wire.get("base").is_none());
        assert!(ws_wire.get("beads_sync_repo").is_none());
    }

    #[test]
    fn set_task_repo_refuses_empty_upstream() {
        let b = Board::new(
            Schema::default(),
            std::env::temp_dir().join(format!(
                "sandboard-test-task-repo-empty-{}.json",
                std::process::id()
            )),
        );
        let project = b
            .create(None, "P", "why", None, Origin::Human, true, None)
            .unwrap();
        let task = b
            .create(
                Some(project.id),
                "T",
                "i",
                Some("d".into()),
                Origin::Human,
                false,
                None,
            )
            .unwrap();
        assert!(b
            .set_task_repo(
                task.id,
                Some(RepoConfig {
                    upstream: "  ".into(),
                    fork: String::new(),
                    base: "main".into(),
                }),
            )
            .is_err());
        // Default base when omitted / empty.
        let bound = b
            .set_task_repo(
                task.id,
                Some(RepoConfig {
                    upstream: "acme/widgets".into(),
                    fork: String::new(),
                    base: String::new(),
                }),
            )
            .expect("complete upstream");
        assert_eq!(bound.repo.as_ref().unwrap().base, "main");
    }

    #[test]
    fn resolve_policy_yaml_ignores_host_paths_for_create_defaults() {
        let dir = std::env::temp_dir().join(format!(
            "sandboard-test-sbx-no-host-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("policy.yaml");
        std::fs::write(&path, "version: 1\n# from-file\n").unwrap();
        assert_eq!(
            resolve_policy_yaml(path.to_str().unwrap()),
            crate::seed_policies::MINIMAL_SANDBOX_POLICY
        );
        let defaults = crate::model::sandbox_profile_create_defaults();
        assert_eq!(
            defaults.policy_id,
            crate::seed_policies::MINIMAL_POLICY_ID
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sandbox_profiles_set_default_and_project_override() {
        let b = Board::new(
            Schema::default(),
            std::env::temp_dir().join("sandboard-test-sbx-override.json"),
        );
        upsert_test_profile(&b, "default", "Default", "seed-image:test");
        let heavy = b
            .upsert_sandbox_profile(SandboxProfile {
                id: "heavy".into(),
                name: "Heavy".into(),
                image: "heavy:latest".into(),
                policy_id: SEED_POLICY_ID.into(),
                policy_inline_legacy: None,
                cpu: Some("8".into()),
                memory: Some("16Gi".into()),
                engine: None,
                model: None,
                provider_names: Vec::new(),
                mcp_server_ids: Vec::new(),
            env: Default::default(),
            prompt: None,
            shipped: false,
            })
            .expect("upsert heavy");
        b.set_default_sandbox_profile(&heavy.id)
            .expect("set default");
        assert_eq!(b.default_sandbox_profile_id().as_deref(), Some("heavy"));

        let project = b
            .create(None, "Sbx Proj", "why", None, Origin::Human, true, None)
            .expect("project");
        assert!(project.sandbox_profile_id.is_none());

        let updated = b
            .set_project_sandbox_profile(project.id, Some("default".into()))
            .expect("set override");
        assert_eq!(updated.sandbox_profile_id.as_deref(), Some("default"));

        let cleared = b
            .set_project_sandbox_profile(project.id, None)
            .expect("clear override");
        assert!(cleared.sandbox_profile_id.is_none());

        assert!(
            b.set_project_sandbox_profile(project.id, Some("missing".into()))
                .is_err(),
            "unknown profile must be refused"
        );
        assert!(
            b.set_default_sandbox_profile("missing").is_err(),
            "unknown default must be refused"
        );
    }

    #[test]
    fn attach_providers_come_from_profile_list_only() {
        let b = Board::new(
            Schema::default(),
            std::env::temp_dir().join(format!("sandboard-test-attach-profile-{}", std::process::id())),
        );
        upsert_test_profile(&b, "default", "Default", "seed-image:test");
        b.upsert_openshell_provider(OpenShellProviderDesired {
            name: "vertex".into(),
            provider_type: "google-vertex-ai".into(),
            config: Default::default(),
            credentials_sealed: None,
            credential_keys: Vec::new(),
            refresh: None,
        });
        b.upsert_openshell_provider(OpenShellProviderDesired {
            name: "github".into(),
            provider_type: "github".into(),
            config: Default::default(),
            credentials_sealed: None,
            credential_keys: Vec::new(),
            refresh: None,
        });

        let project = b
            .create(None, "P", "why", None, Origin::Human, true, None)
            .unwrap();
        // Seeded default has empty provider_names → attach none.
        assert!(b
            .attach_providers_for_resolved(&b.resolve_sandbox_create(project.id))
            .is_empty());

        b.upsert_sandbox_profile(SandboxProfile {
            id: "default".into(),
            name: "Default".into(),
            image: "img:1".into(),
            policy_id: SEED_POLICY_ID.into(),
            policy_inline_legacy: None,
            cpu: None,
            memory: None,
            engine: Some("cursor".into()),
            model: None,
            provider_names: vec!["vertex".into(), "missing".into()],
            mcp_server_ids: Vec::new(),
            env: Default::default(),
            prompt: None,
            shipped: false,
        })
        .unwrap();
        assert_eq!(
            b.attach_providers_for_resolved(&b.resolve_sandbox_create(project.id)),
            vec!["vertex".to_string()],
            "unknown names dropped; only profile list attaches"
        );
    }

    #[test]
    fn sandbox_profiles_refuse_delete_when_in_use_by_project() {
        let b = Board::new(
            Schema::default(),
            std::env::temp_dir().join("sandboard-test-sbx-delete.json"),
        );
        upsert_test_profile(&b, "default", "Default", "seed-image:test");
        b.upsert_sandbox_profile(SandboxProfile {
            id: "alt".into(),
            name: "Alt".into(),
            image: "alt:latest".into(),
            policy_id: SEED_POLICY_ID.into(),
            policy_inline_legacy: None,
            cpu: None,
            memory: None,
            engine: None,
            model: None,
            provider_names: Vec::new(),
            mcp_server_ids: Vec::new(),
            env: Default::default(),
            prompt: None,
            shipped: false,
        })
        .unwrap();

        // Default may be deleted (clears the preference); Project override still blocks.
        b.delete_sandbox_profile("default").expect("delete default");
        assert!(b.default_sandbox_profile_id().is_none());
        upsert_test_profile(&b, "default", "Default", "seed-image:test");
        b.set_default_sandbox_profile("alt").unwrap();
        let project = b
            .create(None, "Uses Default", "why", None, Origin::Human, true, None)
            .unwrap();
        b.set_project_sandbox_profile(project.id, Some("default".into()))
            .unwrap();

        let err = b.delete_sandbox_profile("default").unwrap_err();
        assert!(err.contains("in use"), "expected in-use refusal, got {err}");

        b.set_project_sandbox_profile(project.id, None).unwrap();
        b.delete_sandbox_profile("default")
            .expect("delete after reassignment");
        assert!(b.get_sandbox_profile("default").is_none());
    }

    #[test]
    fn sandbox_profiles_round_trip_json_flush_load() {
        let path = std::env::temp_dir().join(format!(
            "sandboard-test-sbx-json-{}.json",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_file(&path);

        let b = Board::new(Schema::default(), path.clone());
        upsert_test_profile(&b, "default", "Default", "seed-image:test");
        b.upsert_sandbox_profile(SandboxProfile {
            id: "ci".into(),
            name: "CI".into(),
            image: "ci:1".into(),
            policy_id: SEED_POLICY_ID.into(),
            policy_inline_legacy: None,
            cpu: Some("1".into()),
            memory: None,
            engine: None,
            model: None,
            provider_names: Vec::new(),
            mcp_server_ids: Vec::new(),
            env: Default::default(),
            prompt: None,
            shipped: false,
        })
        .unwrap();
        b.set_default_sandbox_profile("ci").unwrap();
        let project = b
            .create(None, "Persist Proj", "why", None, Origin::Human, true, None)
            .unwrap();
        b.set_project_sandbox_profile(project.id, Some("default".into()))
            .unwrap();
        b.flush();

        let restored = Board::load_or_new(Schema::default(), path.clone());
        assert_eq!(restored.default_sandbox_profile_id().as_deref(), Some("ci"));
        assert_eq!(restored.list_sandbox_profiles().len(), 8);
        let p = restored.get(project.id).expect("project");
        assert_eq!(p.sandbox_profile_id.as_deref(), Some("default"));
        assert_eq!(
            restored.get_sandbox_profile("ci").unwrap().policy_id,
            SEED_POLICY_ID
        );
        assert_eq!(
            restored
                .get_openshell_policy(SEED_POLICY_ID)
                .unwrap()
                .yaml,
            SEED_POLICY_YAML
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn resolve_sandbox_create_project_override_then_default_then_compiled() {
        let yaml_policy = "version: 1\n# yaml-must-not-win\n";
        let def_policy = "version: 1\n# default-profile\n";
        let alt_policy = "version: 1\n# alt-profile\n";
        let mut schema = Schema::default();
        schema.execution.agents = AgentConfig {
            image: "yaml-img".into(),
            policy: yaml_policy.into(),
            cpu: Some("1".into()),
            memory: Some("1Gi".into()),
            ..Default::default()
        };
        let b = Board::new(
            schema,
            std::env::temp_dir().join(format!(
                "sandboard-test-sbx-resolve-store-{}.json",
                std::process::id()
            )),
        );
        // Empty catalog → compiled defaults (yaml create knobs ignored).
        let project = b
            .create(None, "P", "why", None, Origin::Human, true, None)
            .unwrap();
        let task = b
            .create(
                Some(project.id),
                "T",
                "do",
                Some("done".into()),
                Origin::Human,
                false,
                None,
            )
            .unwrap();
        let compiled = AgentConfig::default();
        let fallback = b.resolve_sandbox_create(task.id);
        assert!(fallback.profile_id.is_none());
        assert_eq!(fallback.image, compiled.image);
        assert_eq!(fallback.policy, resolve_policy_yaml(&compiled.policy));
        assert_ne!(fallback.image, "yaml-img");

        b.upsert_openshell_policy(OpenShellPolicy {
            id: "def-pol".into(),
            name: "Default policy".into(),
            yaml: def_policy.into(),
        })
        .unwrap();
        b.upsert_openshell_policy(OpenShellPolicy {
            id: "alt-pol".into(),
            name: "Alt policy".into(),
            yaml: alt_policy.into(),
        })
        .unwrap();
        b.upsert_sandbox_profile(SandboxProfile {
            id: "default".into(),
            name: "Default".into(),
            image: "def-img".into(),
            policy_id: "def-pol".into(),
            policy_inline_legacy: None,
            cpu: Some("2".into()),
            memory: None,
            engine: None,
            model: None,
            provider_names: Vec::new(),
            mcp_server_ids: Vec::new(),
            env: Default::default(),
            prompt: None,
            shipped: false,
        })
        .unwrap();
        b.set_default_sandbox_profile("default").unwrap();
        let def = b.resolve_sandbox_create(task.id);
        assert_eq!(def.profile_id.as_deref(), Some("default"));
        assert_eq!(def.image, "def-img");
        assert_eq!(def.policy, def_policy);

        b.upsert_sandbox_profile(SandboxProfile {
            id: "alt".into(),
            name: "Alt".into(),
            image: "alt-img".into(),
            policy_id: "alt-pol".into(),
            policy_inline_legacy: None,
            cpu: None,
            memory: Some("8Gi".into()),
            engine: None,
            model: Some("claude-opus-4".into()),
            provider_names: Vec::new(),
            mcp_server_ids: Vec::new(),
            env: Default::default(),
            prompt: None,
            shipped: false,
        })
        .unwrap();
        b.set_project_sandbox_profile(project.id, Some("alt".into()))
            .unwrap();
        let over = b.resolve_sandbox_create(task.id);
        assert_eq!(over.profile_id.as_deref(), Some("alt"));
        assert_eq!(over.image, "alt-img");
        assert_eq!(over.policy, alt_policy);
        assert_eq!(over.memory.as_deref(), Some("8Gi"));
        assert_eq!(over.model.as_deref(), Some("claude-opus-4"));
    }

    #[test]
    fn resolve_engine_for_card_profile_wins_over_runtime_and_ignores_item_engine() {
        let mut schema = Schema::default();
        schema.execution.agents.engine = "cursor".into();
        let b = Board::new(
            schema,
            std::env::temp_dir().join(format!(
                "sandboard-test-engine-resolve-{}.json",
                std::process::id()
            )),
        );
        b.set_agent_runtime(AgentRuntimeConfig {
            engine: "cursor".into(),
            ..Default::default()
        });
        ensure_seed_policy(&b);
        b.upsert_sandbox_profile(SandboxProfile {
            id: "default".into(),
            name: "Default".into(),
            image: "img:1".into(),
            policy_id: SEED_POLICY_ID.into(),
            policy_inline_legacy: None,
            cpu: None,
            memory: None,
            engine: Some("agy".into()),
            model: None,
            provider_names: Vec::new(),
            mcp_server_ids: Vec::new(),
            env: Default::default(),
            prompt: None,
            shipped: false,
        })
        .unwrap();
        b.set_default_sandbox_profile("default").unwrap();

        let project = b
            .create(None, "P", "why", None, Origin::Human, true, None)
            .unwrap();
        let task = b
            .create(
                Some(project.id),
                "T",
                "do",
                Some("done".into()),
                Origin::Human,
                false,
                None,
            )
            .unwrap();
        // Stale card-level engine must not win.
        b.update_item(task.id, None, None, None, Some("claude".into()), None)
            .unwrap();
        assert_eq!(b.resolve_engine_for_card(task.id), "agy");

        b.transition(task.id, State::Shaping, "test", None).unwrap();
        b.transition(task.id, State::Backlog, "test", None).unwrap();
        let grant = b.claim(task.id, "agent-1", None, 60).unwrap();
        assert_eq!(grant.engine.as_deref(), Some("agy"));
    }

    #[test]
    fn resolve_engine_for_card_falls_back_to_agent_runtime() {
        let mut schema = Schema::default();
        schema.execution.agents.engine = "cursor".into();
        let b = Board::new(
            schema,
            std::env::temp_dir().join(format!(
                "sandboard-test-engine-fallback-{}.json",
                std::process::id()
            )),
        );
        b.set_agent_runtime(AgentRuntimeConfig {
            engine: "claude".into(),
            ..Default::default()
        });
        ensure_seed_policy(&b);
        b.upsert_sandbox_profile(SandboxProfile {
            id: "default".into(),
            name: "Default".into(),
            image: "img:1".into(),
            policy_id: SEED_POLICY_ID.into(),
            policy_inline_legacy: None,
            cpu: None,
            memory: None,
            engine: None,
            model: None,
            provider_names: Vec::new(),
            mcp_server_ids: Vec::new(),
            env: Default::default(),
            prompt: None,
            shipped: false,
        })
        .unwrap();
        b.set_default_sandbox_profile("default").unwrap();
        let project = b
            .create(None, "P", "why", None, Origin::Human, true, None)
            .unwrap();
        let task = b
            .create(
                Some(project.id),
                "T",
                "do",
                Some("done".into()),
                Origin::Human,
                false,
                None,
            )
            .unwrap();
        assert_eq!(b.resolve_engine_for_card(task.id), "claude");
    }

    #[test]
    fn resolve_model_for_card_card_over_spec_over_default() {
        let b = Board::new(
            Schema::default(),
            std::env::temp_dir().join(format!(
                "sandboard-test-model-resolve-{}",
                std::process::id()
            )),
        );
        ensure_seed_policy(&b);
        b.upsert_sandbox_profile(SandboxProfile {
            id: "default".into(),
            name: "Default".into(),
            image: "img:1".into(),
            policy_id: SEED_POLICY_ID.into(),
            policy_inline_legacy: None,
            cpu: None,
            memory: None,
            engine: Some("agy".into()),
            model: Some("spec-model".into()),
            provider_names: Vec::new(),
            mcp_server_ids: Vec::new(),
            env: Default::default(),
            prompt: None,
            shipped: false,
        })
        .unwrap();
        b.set_default_sandbox_profile("default").unwrap();
        let project = b
            .create(None, "P", "why", None, Origin::Human, true, None)
            .unwrap();
        let task = b
            .create(
                Some(project.id),
                "T",
                "do",
                Some("done".into()),
                Origin::Human,
                false,
                None,
            )
            .unwrap();
        assert_eq!(
            b.resolve_model_for_card(task.id).as_deref(),
            Some("spec-model")
        );

        b.transition(task.id, State::Shaping, "test", None).unwrap();
        b.transition(task.id, State::Backlog, "test", None).unwrap();
        let grant = b.claim(task.id, "agent-1", Some("claim-model".into()), 60).unwrap();
        assert_eq!(grant.model.as_deref(), Some("claim-model"));
        assert_eq!(
            b.resolve_model_for_card(task.id).as_deref(),
            Some("claim-model")
        );
    }

    #[test]
    fn resolve_model_for_card_agy_defaults_without_spec_or_card() {
        let b = Board::new(
            Schema::default(),
            std::env::temp_dir().join(format!(
                "sandboard-test-model-default-{}",
                std::process::id()
            )),
        );
        ensure_seed_policy(&b);
        b.upsert_sandbox_profile(SandboxProfile {
            id: "default".into(),
            name: "Default".into(),
            image: "img:1".into(),
            policy_id: SEED_POLICY_ID.into(),
            policy_inline_legacy: None,
            cpu: None,
            memory: None,
            engine: Some("agy".into()),
            model: None,
            provider_names: Vec::new(),
            mcp_server_ids: Vec::new(),
            env: Default::default(),
            prompt: None,
            shipped: false,
        })
        .unwrap();
        b.set_default_sandbox_profile("default").unwrap();
        let project = b
            .create(None, "P", "why", None, Origin::Human, true, None)
            .unwrap();
        let task = b
            .create(
                Some(project.id),
                "T",
                "do",
                Some("done".into()),
                Origin::Human,
                false,
                None,
            )
            .unwrap();
        assert_eq!(
            b.resolve_model_for_card(task.id).as_deref(),
            Some(crate::antigravity::DEFAULT_SEAT_MODEL)
        );
    }

    #[test]
    fn resolve_cockpit_model_spec_then_default() {
        let b = Board::new(
            Schema::default(),
            std::env::temp_dir().join(format!(
                "sandboard-test-cockpit-model-{}",
                std::process::id()
            )),
        );
        ensure_seed_policy(&b);
        b.upsert_sandbox_profile(SandboxProfile {
            id: "cockpit".into(),
            name: "Cockpit".into(),
            image: "img:1".into(),
            policy_id: SEED_POLICY_ID.into(),
            policy_inline_legacy: None,
            cpu: None,
            memory: None,
            engine: Some("agy".into()),
            model: Some("cockpit-spec".into()),
            provider_names: Vec::new(),
            mcp_server_ids: Vec::new(),
            env: Default::default(),
            prompt: None,
            shipped: false,
        })
        .unwrap();
        b.set_cockpit_sandbox_profile("cockpit").unwrap();
        assert_eq!(
            b.resolve_cockpit_model().as_deref(),
            Some("cockpit-spec")
        );

        b.upsert_sandbox_profile(SandboxProfile {
            id: "cockpit2".into(),
            name: "Cockpit2".into(),
            image: "img:2".into(),
            policy_id: SEED_POLICY_ID.into(),
            policy_inline_legacy: None,
            cpu: None,
            memory: None,
            engine: Some("agy".into()),
            model: None,
            provider_names: Vec::new(),
            mcp_server_ids: Vec::new(),
            env: Default::default(),
            prompt: None,
            shipped: false,
        })
        .unwrap();
        b.set_cockpit_sandbox_profile("cockpit2").unwrap();
        assert_eq!(
            b.resolve_cockpit_model().as_deref(),
            Some(crate::antigravity::DEFAULT_SEAT_MODEL)
        );
    }

    #[test]
    fn sandbox_profiles_create_without_id_slugs_from_name() {
        let b = Board::new(
            Schema::default(),
            std::env::temp_dir().join("sandboard-test-sbx-slug.json"),
        );
        upsert_test_profile(&b, "default", "Default", "seed-image:test");

        let created = b
            .upsert_sandbox_profile(SandboxProfile {
                id: String::new(),
                name: "Heavy CI".into(),
                image: "img:ci".into(),
                policy_id: SEED_POLICY_ID.into(),
                policy_inline_legacy: None,
                cpu: None,
                memory: None,
                engine: None,
                model: None,
                provider_names: Vec::new(),
                mcp_server_ids: Vec::new(),
            env: Default::default(),
            prompt: None,
            shipped: false,
            })
            .expect("create from name");
        assert_eq!(created.id, "heavy-ci");

        // Collision with seeded `default` slug from name "Default" would be
        // "default" — creating another "Default" must suffix.
        let again = b
            .upsert_sandbox_profile(SandboxProfile {
                id: String::new(),
                name: "Default".into(),
                image: "img:2".into(),
                policy_id: SEED_POLICY_ID.into(),
                policy_inline_legacy: None,
                cpu: None,
                memory: None,
                engine: None,
                model: None,
                provider_names: Vec::new(),
                mcp_server_ids: Vec::new(),
            env: Default::default(),
            prompt: None,
            shipped: false,
            })
            .expect("create colliding slug");
        assert_eq!(again.id, "default-2");

        // Explicit empty-ish punctuation falls back to `profile`.
        let punct = b
            .upsert_sandbox_profile(SandboxProfile {
                id: "".into(),
                name: "!!!".into(),
                image: "img:x".into(),
                policy_id: SEED_POLICY_ID.into(),
                policy_inline_legacy: None,
                cpu: None,
                memory: None,
                engine: None,
                model: None,
                provider_names: Vec::new(),
                mcp_server_ids: Vec::new(),
            env: Default::default(),
            prompt: None,
            shipped: false,
            })
            .expect("create punctuation name");
        assert_eq!(punct.id, "profile");

        // Seeded default still present and untouched.
        assert!(b.get_sandbox_profile("default").is_some());
        assert_eq!(b.default_sandbox_profile_id().as_deref(), Some("default"));
    }

    #[test]
    fn migrate_sandbox_policies_path_to_inline_then_catalog() {
        let dir = std::env::temp_dir().join(format!(
            "sandboard-test-sbx-migrate-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let policy_path = dir.join("legacy.yaml");
        let yaml = "version: 1\n# legacy-migrated\n";
        std::fs::write(&policy_path, yaml).unwrap();

        let b = Board::new(Schema::default(), dir.join("board.json"));
        // Simulate a pre-catalog board row: host path in legacy `policy` field.
        {
            let mut s = b.state.write();
            s.sandbox_profiles.insert(
                "legacy".into(),
                SandboxProfile {
                    id: "legacy".into(),
                    name: "Legacy".into(),
                    image: "img:1".into(),
                    policy_id: String::new(),
                    policy_inline_legacy: Some(policy_path.to_string_lossy().into()),
                    cpu: None,
                    memory: None,
                    engine: None,
                    model: None,
                    provider_names: Vec::new(),
                    mcp_server_ids: Vec::new(),
            env: Default::default(),
            prompt: None,
            shipped: false,
                },
            );
        }
        assert_eq!(b.migrate_sandbox_policies_to_inline(), 1);
        assert_eq!(
            b.get_sandbox_profile("legacy")
                .unwrap()
                .policy_inline_legacy
                .as_deref(),
            Some(yaml)
        );
        assert_eq!(b.migrate_sandbox_policies_to_inline(), 0);
        assert_eq!(b.migrate_inline_policies_to_catalog(), 1);
        let profile = b.get_sandbox_profile("legacy").unwrap();
        assert_eq!(profile.policy_id, "legacy-policy");
        assert!(profile.policy_inline_legacy.is_none());
        assert_eq!(
            b.get_openshell_policy("legacy-policy").unwrap().yaml,
            yaml,
            "migration must preserve YAML bytes exactly"
        );
        assert_eq!(b.migrate_inline_policies_to_catalog(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn openshell_policies_crud_and_refuse_in_use_delete() {
        let b = Board::new(
            Schema::default(),
            std::env::temp_dir().join(format!(
                "sandboard-test-policies-crud-{}",
                std::process::id()
            )),
        );
        let created = b
            .upsert_openshell_policy(OpenShellPolicy {
                id: "".into(),
                name: "Allow crates".into(),
                yaml: "version: 1\n# crates\n".into(),
            })
            .expect("create policy");
        assert_eq!(created.id, "allow-crates");
        assert_eq!(b.list_openshell_policies().len(), 1);

        b.upsert_sandbox_profile(SandboxProfile {
            id: "default".into(),
            name: "Default".into(),
            image: "img:1".into(),
            policy_id: created.id.clone(),
            policy_inline_legacy: None,
            cpu: None,
            memory: None,
            engine: None,
            model: None,
            provider_names: Vec::new(),
            mcp_server_ids: Vec::new(),
            env: Default::default(),
            prompt: None,
            shipped: false,
        })
        .expect("profile");

        let err = b.delete_openshell_policy(&created.id).unwrap_err();
        assert!(
            err.contains("in use"),
            "in-use delete must fail, got {err}"
        );

        b.delete_sandbox_profile("default").expect("drop profile");
        b.delete_openshell_policy(&created.id).expect("delete free");
        assert!(b.get_openshell_policy(&created.id).is_none());
    }

    #[test]
    fn mcp_server_catalog_merges_policy_fragment_and_providers() {
        let b = Board::new(
            Schema::default(),
            std::env::temp_dir().join(format!(
                "sandboard-test-mcp-catalog-{}.json",
                std::process::id()
            )),
        );
        ensure_seed_policy(&b);
        assert!(b.ensure_shipped_mcp_servers());
        assert!(!b.ensure_shipped_mcp_servers());
        // Legacy shipped HTTP + CockpitBearer must migrate to stdio placeholder.
        {
            let mut s = b.state.write();
            s.mcp_servers.insert(
                SANDBOARD_MCP_SERVER_ID.into(),
                McpServerDesired {
                    id: SANDBOARD_MCP_SERVER_ID.into(),
                    name: "sandboard".into(),
                    transport: McpTransport::Http {
                        url: String::new(),
                        auth: McpHttpAuth::CockpitBearer,
                    },
                    policy_fragment_yaml: None,
                    provider_names: Vec::new(),
                    env: Default::default(),
                    audience: McpAudience::Cockpit,
                    shipped: true,
                },
            );
        }
        assert!(b.ensure_shipped_mcp_servers());
        let migrated = b.get_mcp_server(SANDBOARD_MCP_SERVER_ID).expect("sandboard");
        assert!(matches!(
            migrated.transport,
            McpTransport::Stdio { ref command, ref args, cwd: None }
                if command.is_empty() && args.is_empty()
        ));
        assert!(!b.ensure_shipped_mcp_servers());
        b.upsert_openshell_provider(OpenShellProviderDesired {
            name: "gcp-adc".into(),
            provider_type: "google-cloud".into(),
            config: Default::default(),
            credentials_sealed: None,
            credential_keys: Vec::new(),
            refresh: None,
        });
        let frag = r#"
network_policies:
  hf:
    name: hf
    endpoints:
      - { host: huggingface.co, port: 443, access: full, tls: skip }
"#;
        b.upsert_mcp_server(McpServerDesired {
            id: "cnv".into(),
            name: "CNV".into(),
            transport: McpTransport::Stdio {
                command: "uv".into(),
                args: vec!["tool".into(), "run".into()],
                cwd: None,
            },
            policy_fragment_yaml: Some(frag.into()),
            provider_names: vec!["gcp-adc".into()],
            env: Default::default(),
            audience: McpAudience::Both,
            shipped: false,
        })
        .expect("upsert mcp");
        b.upsert_sandbox_profile(SandboxProfile {
            id: "default".into(),
            name: "Default".into(),
            image: "img:1".into(),
            policy_id: SEED_POLICY_ID.into(),
            policy_inline_legacy: None,
            cpu: None,
            memory: None,
            engine: Some("cursor".into()),
            model: None,
            provider_names: Vec::new(),
            mcp_server_ids: vec!["cnv".into()],
            env: Default::default(),
            prompt: None,
            shipped: false,
        })
        .expect("profile");
        let resolved = b.resolve_cockpit_sandbox_create();
        assert!(resolved.policy.contains("huggingface.co"));
        assert_eq!(resolved.providers, vec!["gcp-adc".to_string()]);
        // Cockpit always gets shipped sandboard even when the profile only listed cnv.
        assert_eq!(
            resolved.mcp_server_ids,
            vec!["sandboard".to_string(), "cnv".to_string()]
        );
        let worker = {
            let project = b
                .create(None, "P", "why", None, Origin::Human, true, None)
                .unwrap();
            b.resolve_sandbox_create(project.id)
        };
        assert_eq!(worker.mcp_server_ids, vec!["cnv".to_string()]);
        // Cockpit Bearer sandboard must not attach to workers even if listed.
        b.upsert_sandbox_profile(SandboxProfile {
            id: "default".into(),
            name: "Default".into(),
            image: "img:1".into(),
            policy_id: SEED_POLICY_ID.into(),
            policy_inline_legacy: None,
            cpu: None,
            memory: None,
            engine: Some("cursor".into()),
            model: None,
            provider_names: Vec::new(),
            mcp_server_ids: vec!["sandboard".into(), "cnv".into()],
            env: Default::default(),
            prompt: None,
            shipped: false,
        })
        .expect("profile");
        let worker2 = b.resolve_sandbox_create(
            b.create(None, "P2", "why", None, Origin::Human, true, None)
                .unwrap()
                .id,
        );
        assert_eq!(worker2.mcp_server_ids, vec!["cnv".to_string()]);
    }

    #[test]
    fn ensure_cockpit_sandboard_mcp_attach_seeds_profile_and_resolve() {
        let b = Board::new(
            Schema::default(),
            std::env::temp_dir().join(format!(
                "sandboard-test-cockpit-sandboard-attach-{}",
                std::process::id()
            )),
        );
        ensure_seed_policy(&b);
        assert!(b.ensure_shipped_mcp_servers());
        b.upsert_sandbox_profile(SandboxProfile {
            id: "cockpit".into(),
            name: "Cockpit".into(),
            image: "img:1".into(),
            policy_id: SEED_POLICY_ID.into(),
            policy_inline_legacy: None,
            cpu: None,
            memory: None,
            engine: Some("cursor".into()),
            model: None,
            provider_names: Vec::new(),
            mcp_server_ids: Vec::new(),
            env: Default::default(),
            prompt: None,
            shipped: false,
        })
        .expect("profile");
        b.set_cockpit_sandbox_profile("cockpit").unwrap();
        let profile = b
            .list_sandbox_profiles()
            .into_iter()
            .find(|p| p.id == "cockpit")
            .expect("cockpit profile");
        assert_eq!(profile.mcp_server_ids, vec!["sandboard".to_string()]);
        assert!(!b.ensure_cockpit_sandboard_mcp_attach());
        let resolved = b.resolve_cockpit_sandbox_create();
        assert_eq!(resolved.mcp_server_ids, vec!["sandboard".to_string()]);
    }

    #[test]
    fn upsert_cockpit_profile_refuses_to_drop_shipped_sandboard() {
        let b = Board::new(
            Schema::default(),
            std::env::temp_dir().join(format!(
                "sandboard-test-cockpit-sandboard-locked-{}",
                std::process::id()
            )),
        );
        ensure_seed_policy(&b);
        b.upsert_sandbox_profile(SandboxProfile {
            id: "cockpit".into(),
            name: "Cockpit".into(),
            image: "img:1".into(),
            policy_id: SEED_POLICY_ID.into(),
            policy_inline_legacy: None,
            cpu: None,
            memory: None,
            engine: Some("cursor".into()),
            model: None,
            provider_names: Vec::new(),
            mcp_server_ids: vec!["sandboard".into()],
            env: Default::default(),
            prompt: None,
            shipped: false,
        })
        .expect("profile");
        b.set_cockpit_sandbox_profile("cockpit").unwrap();
        let stored = b
            .upsert_sandbox_profile(SandboxProfile {
                id: "cockpit".into(),
                name: "Cockpit".into(),
                image: "img:1".into(),
                policy_id: SEED_POLICY_ID.into(),
                policy_inline_legacy: None,
                cpu: None,
                memory: None,
                engine: Some("cursor".into()),
                model: None,
                provider_names: Vec::new(),
                mcp_server_ids: Vec::new(),
            env: Default::default(),
            prompt: None,
            shipped: false,
            })
            .expect("upsert without sandboard");
        assert_eq!(
            stored.mcp_server_ids,
            vec!["sandboard".to_string()],
            "cockpit upsert must keep shipped sandboard"
        );
    }

    #[test]
    fn ensure_minimal_policy_seeds_once_without_overwrite() {
        let b = Board::new(
            Schema::default(),
            std::env::temp_dir().join(format!(
                "sandboard-test-minimal-seed-{}",
                std::process::id()
            )),
        );
        assert!(b.ensure_minimal_policy());
        assert!(!b.ensure_minimal_policy());
        let id = crate::seed_policies::MINIMAL_POLICY_ID;
        let edited = "version: 1\n# operator-edited\n";
        b.upsert_openshell_policy(OpenShellPolicy {
            id: id.into(),
            name: "Minimal+".into(),
            yaml: edited.into(),
        })
        .unwrap();
        assert!(!b.ensure_minimal_policy());
        assert_eq!(b.get_openshell_policy(id).unwrap().yaml, edited);
        let defaults = crate::model::sandbox_profile_create_defaults();
        assert_eq!(defaults.policy_id, id);
    }

    #[test]
    fn ensure_shipped_sandbox_profiles_seeds_once_without_overwrite() {
        let b = Board::new(
            Schema::default(),
            std::env::temp_dir().join(format!(
                "sandboard-test-shipped-sandbox-profiles-{}",
                std::process::id()
            )),
        );
        assert!(b.ensure_shipped_sandbox_profiles());
        assert!(!b.ensure_shipped_sandbox_profiles());

        for (id, engine, policy_id) in [
            ("sandbox-cursor", "cursor", crate::seed_policies::COCKPIT_CURSOR_POLICY_ID),
            ("sandbox-agy", "agy", crate::seed_policies::COCKPIT_AGY_POLICY_ID),
            ("sandbox-claude", "claude", crate::seed_policies::COCKPIT_CLAUDE_POLICY_ID),
            ("sandbox-opencode", "opencode", crate::seed_policies::COCKPIT_OPENCODE_POLICY_ID),
        ] {
            let p = b.get_sandbox_profile(id).unwrap_or_else(|| panic!("missing {id}"));
            assert_eq!(p.engine.as_deref(), Some(engine));
            assert_eq!(p.policy_id, policy_id);
            assert!(p.model.is_none(), "seeded profiles must not set model: {p:?}");
            assert!(p.image.contains(&format!("sandbox-{engine}")), "{p:?}");
            assert!(
                p.mcp_server_ids.iter().any(|m| m == "sandboard"),
                "cockpit MCP must be attached: {p:?}"
            );
            assert!(p.shipped, "{p:?}");
            assert!(b.get_openshell_policy(policy_id).is_some(), "policy {policy_id}");
        }
        // Seeding never picks a default — that's an explicit operator choice.
        assert!(b.default_sandbox_profile_id().is_none());

        // Operator edit survives a re-seed: still keyed by id, ensure only inserts.
        let mut edited = b.get_sandbox_profile("sandbox-cursor").unwrap();
        edited.name = "My Cursor Box".into();
        b.upsert_sandbox_profile(edited).unwrap();
        assert!(!b.ensure_shipped_sandbox_profiles());
        assert_eq!(
            b.get_sandbox_profile("sandbox-cursor").unwrap().name,
            "My Cursor Box"
        );
    }

    #[test]
    fn migrate_inline_policy_boards_losslessly_on_load() {
        let path = std::env::temp_dir().join(format!(
            "sandboard-test-inline-migrate-load-{}.json",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_file(&path);
        let yaml = "version: 1\n# keep-exact-bytes\nfilesystem_policy:\n  include_workdir: true\n";
        // Pre-catalog board JSON (inline policy on the profile).
        let legacy = serde_json::json!({
            "next_id": 1,
            "items": {},
            "sandbox_profiles": {
                "default": {
                    "id": "default",
                    "name": "Default",
                    "image": "img:1",
                    "policy": yaml,
                    "cpu": null,
                    "memory": null,
                    "engine": null,
                    "provider_names": []
                }
            },
            "default_sandbox_profile_id": "default"
        });
        std::fs::write(&path, serde_json::to_string_pretty(&legacy).unwrap()).unwrap();

        let restored = Board::load_or_new(Schema::default(), path.clone());
        let profile = restored.get_sandbox_profile("default").unwrap();
        assert_eq!(profile.policy_id, "default-policy");
        assert!(profile.policy_inline_legacy.is_none());
        assert_eq!(
            restored.get_openshell_policy("default-policy").unwrap().yaml,
            yaml
        );
        let resolved = restored.resolve_cockpit_sandbox_create();
        assert_eq!(resolved.policy, yaml);
        // Minimal seed also present for create defaults.
        assert!(
            restored
                .get_openshell_policy(crate::seed_policies::MINIMAL_POLICY_ID)
                .is_some()
        );
        let _ = std::fs::remove_file(&path);
    }
}
