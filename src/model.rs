//! One node type.
//!
//! Project and Task are the only levels: Project is a container; Tasks are
//! flat siblings under it, linked by dependency edges. Only two facts about a
//! node are structural: whether it has children (container vs claimable leaf)
//! and where it sits relative to the commitment line. Everything else — the
//! badge, the colour, which gates apply — comes from the level schema.

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub type ItemId = u64;

/// The lifecycle contract. The UI renders it; the agent API mutates it. Same
/// object — see `machine.rs` for the legal edges.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Draft,
    Shaping,
    /// Claimable pool — operator must explicitly dispatch; nothing auto-starts.
    ///
    /// `alias = "ready"` loads legacy boards/history that used Ready.
    #[serde(alias = "ready")]
    Backlog,
    Claimed,
    Running,
    Splitting,
    NeedsHuman,
    /// Human review of the PR. Mechanical checks belong in CI, not a board column.
    ///
    /// `alias = "verifying"` loads legacy history/boards that used the removed
    /// Verifying state (sandboard never ran real gates there).
    #[serde(alias = "verifying")]
    Review,
    Done,
    /// Cut scope. Retired, not deleted — "we chose not to" is a fact you will
    /// need later.
    Retired,
}

impl State {
    /// Which board column this state renders in. Several states collapse into
    /// one column because the question you're asking of them is the same.
    pub fn column(self) -> Column {
        match self {
            State::Draft => Column::Intake,
            State::Shaping => Column::Shaping,
            State::Backlog => Column::Backlog,
            State::Claimed | State::Running | State::Splitting => Column::Running,
            State::NeedsHuman => Column::NeedsYou,
            State::Review => Column::Review,
            State::Done => Column::Done,
            State::Retired => Column::Retired,
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, State::Done | State::Retired)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Column {
    Intake,
    Shaping,
    /// Formerly `ready` — serde alias keeps old snapshots loading.
    #[serde(alias = "ready")]
    Backlog,
    Running,
    NeedsYou,
    Review,
    Done,
    Retired,
}

/// Provenance — "why does this exist?" must be instantly answerable, so the
/// tree stays honest about what a person actually asked for versus what the
/// system decided on its own.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Origin {
    Human,
    Planner,
    /// Machine-born: an agent discovered the work was bigger than its card.
    Split {
        from: ItemId,
    },
    Reflection,
}

/// Who holds the card while a run is in flight. The hard stop is
/// [`WorkItem::run_deadline_at`] (agent timeout), not lease renewal.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Lease {
    pub agent_id: String,
    pub granted_at: DateTime<Utc>,
    /// Retained for older clients; not used for liveness or sweep.
    #[serde(default)]
    pub last_heartbeat: DateTime<Utc>,
    /// Mirrors `run_deadline_at` at claim time; not extended by heartbeats.
    pub expires_at: DateTime<Utc>,
}

impl Lease {
    pub fn is_expired(&self, now: DateTime<Utc>) -> bool {
        now > self.expires_at
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct EscalationOption {
    pub label: String,
    pub detail: String,
}

/// An open-ended "what should I do?" transfers the whole problem back to the
/// human. Forcing concrete options with a recommendation turns a five-minute
/// think into a one-tap decision.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Escalation {
    pub question: String,
    pub options: Vec<EscalationOption>,
    pub recommended: usize,
    pub blocked_since: DateTime<Utc>,
    #[serde(default)]
    pub answer: Option<String>,
}

impl Escalation {
    pub fn blocked_secs(&self, now: DateTime<Utc>) -> i64 {
        (now - self.blocked_since).num_seconds().max(0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GateStatus {
    Pending,
    Running,
    Passed,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct GateRun {
    pub name: String,
    pub status: GateStatus,
    #[serde(default)]
    pub detail: Option<String>,
}

/// A Steer note: injected into a running agent's next turn. Free — no restart,
/// no context loss.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Note {
    pub at: DateTime<Utc>,
    pub author: String,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Transition {
    pub at: DateTime<Utc>,
    pub from: State,
    pub to: State,
    pub by: String,
    #[serde(default)]
    pub reason: Option<String>,
}

/// Summary of a resolved blocker item (id, title, state).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct BlockerSummary {
    pub id: ItemId,
    pub title: String,
    pub state: State,
}

/// Lifecycle of a Plan artifact attached to a Project.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus {
    /// Empty shell — waiting for a Plan Task (or propose_breakdown) to fill it.
    Empty,
    /// Proposed revision awaiting human Approve Plan.
    AwaitingApproval,
    /// Last revision has been materialized; Tasks are on the Board.
    Approved,
}

/// One proposed Task inside a Plan artifact. Keys are stable within the plan
/// so deps and replans can refer to work before board ids exist.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PlanTaskSpec {
    pub key: String,
    pub title: String,
    pub intent: String,
    pub definition_of_done: String,
    #[serde(default)]
    pub blocked_by_keys: Vec<String>,
    #[serde(default)]
    pub capability: Option<String>,
    /// Optional wire field; materialize uses intent/DoD for clone targets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<crate::schema::RepoConfig>,
    /// Set when Approve Plan materializes (or updates) a board Task.
    #[serde(default)]
    pub item_id: Option<ItemId>,
}

/// Legacy Project-level plan blob (ignored for new boards). Live plans are
/// `TaskProposal` on the Initial plan card.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PlanArtifact {
    pub revision: u32,
    #[serde(default)]
    pub summary: String,
    pub status: PlanStatus,
    #[serde(default)]
    pub tasks: Vec<PlanTaskSpec>,
    /// Keys to retire on the next approve (replan cancels).
    #[serde(default)]
    pub cancel_keys: Vec<String>,
    /// Board ids resolved from `cancel_keys` at propose time (keys drop out of `tasks`).
    #[serde(default)]
    pub cancel_item_ids: Vec<ItemId>,
    #[serde(default)]
    pub approved_revision: Option<u32>,
}

impl PlanArtifact {
    #[allow(dead_code)] // kept for legacy board JSON / future replan tooling
    pub fn empty() -> Self {
        Self {
            revision: 0,
            summary: String::new(),
            status: PlanStatus::Empty,
            tasks: Vec::new(),
            cancel_keys: Vec::new(),
            cancel_item_ids: Vec::new(),
            approved_revision: None,
        }
    }

    /// Compact status for Home / GoalView: `no_plan`, `awaiting_approval`, `approved_vN`.
    #[allow(dead_code)] // GoalView now derives status from Initial plan proposal
    pub fn status_label(&self) -> String {
        match self.status {
            PlanStatus::Empty => "no_plan".into(),
            PlanStatus::AwaitingApproval => "awaiting_approval".into(),
            PlanStatus::Approved => {
                format!(
                    "approved_v{}",
                    self.approved_revision.unwrap_or(self.revision)
                )
            }
        }
    }
}

/// Legacy exact title (pre–project-name seed). Still recognized by
/// [`title_is_initial_plan`].
pub const INITIAL_PLAN_TITLE_LEGACY: &str = "Initial plan";

/// Prefix for seed Task titles: `Initial Plan for <Project name>`.
pub const INITIAL_PLAN_TITLE_PREFIX: &str = "Initial Plan for ";

/// Title for a Project's Initial plan seed Task.
pub fn initial_plan_title(project_title: &str) -> String {
    format!("{INITIAL_PLAN_TITLE_PREFIX}{project_title}")
}

/// Whether a card title identifies an Initial plan Task.
pub fn title_is_initial_plan(title: &str) -> bool {
    title == INITIAL_PLAN_TITLE_LEGACY || title.starts_with(INITIAL_PLAN_TITLE_PREFIX)
}

#[cfg(test)]
mod initial_plan_title_tests {
    use super::*;

    #[test]
    fn title_includes_project_name() {
        assert_eq!(
            initial_plan_title("Webhook rebase"),
            "Initial Plan for Webhook rebase"
        );
        assert!(title_is_initial_plan("Initial Plan for Webhook rebase"));
        assert!(title_is_initial_plan(INITIAL_PLAN_TITLE_LEGACY));
        assert!(!title_is_initial_plan("Implement webhook handler"));
    }
}

/// Hardwired protocol always injected into card briefings — works even when
/// the board standing prompt is cleared. Shared agent policy belongs in
/// Settings → Agent runtime (`standing_prompt`), not here.
pub const PROTOCOL_MINIMUM: &str = "\
Merging is a human action — approving in sandboard surfaces the PR; it never merges.\n\
Sandbox stack failures present as hangs — treat silence as failure and escalate rather than looping.\n\
Name the repository to clone in each Task's intent and/or definition of done \
(`owner/name`, and push remote when it differs). Do not invent an owner/name from context; \
if the card text is silent or ambiguous, escalate.\n\
Initial plan: write /sandbox/.sandboard/plan.json; each proposed task names its \
clone target in intent/DoD; human Approve creates Tasks.\n\
If impl work is bigger than one card, write /sandbox/.sandboard/split.json (same task shape; name \
clone targets in each child's intent/DoD); card goes to Review — Approve creates siblings. \
Never nest under a Task.\n\
When the work is done, write /sandbox/.sandboard/report.json (url/base/head per report.schema.json) \
and publish the PR on this card's branch.\n\
";

/// Default board standing prompt (Settings → Agent runtime). Seeded into
/// [`AgentRuntimeConfig::standing_prompt`] when Agent runtime is first created.
/// Empty by default — board-wide policy is opt-in; hardwired protocol lives in
/// [`PROTOCOL_MINIMUM`]. Project `project_prompt` remains Project-only extras.
pub const DEFAULT_BOARD_STANDING_PROMPT: &str = "";

#[cfg(test)]
mod standing_prompt_tests {
    use super::{DEFAULT_BOARD_STANDING_PROMPT, PROTOCOL_MINIMUM};

    #[test]
    fn protocol_minimum_covers_plan_split_report_and_clone() {
        let p = PROTOCOL_MINIMUM;
        assert!(
            p.contains("plan.json") && p.contains("Approve creates Tasks"),
            "Initial plan must use plan.json then Approve: {p}"
        );
        assert!(
            p.contains("split.json") && p.contains("Approve creates siblings"),
            "split must use split.json then Approve: {p}"
        );
        assert!(
            p.contains("report.json"),
            "must name report.json: {p}"
        );
        assert!(
            p.contains("Name the repository to clone"),
            "must instruct naming the clone target: {p}"
        );
        assert!(
            p.contains("Merging is a human action"),
            "must keep merge invariant: {p}"
        );
    }

    #[test]
    fn board_standing_prompt_default_is_empty() {
        assert!(
            DEFAULT_BOARD_STANDING_PROMPT.trim().is_empty(),
            "board standing prompt must not ship an essay by default"
        );
    }
}

/// One end of a pull request (GitHub `base` / `head`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
pub struct PullRequestEnd {
    /// `owner/name` (`full_name`).
    pub repo: String,
    /// Branch name (GitHub JSON field `ref`).
    #[serde(rename = "ref")]
    pub git_ref: String,
}

impl PullRequestEnd {
    pub fn new(repo: impl Into<String>, git_ref: impl Into<String>) -> Self {
        Self {
            repo: repo.into().trim().to_string(),
            git_ref: {
                let r = git_ref.into().trim().to_string();
                if r.is_empty() {
                    "main".into()
                } else {
                    r
                }
            },
        }
    }

    pub fn is_usable(&self) -> bool {
        !self.repo.trim().is_empty() && !self.git_ref.trim().is_empty()
    }
}

/// Pull request on a card — forge facts for resume/clone/rebase.
/// Shape matches `report.json` / GitHub base&head naming. URL lives here, not
/// as a top-level `pr_url` field. Cards own a list; `merged` is set by
/// webhook/poll, not by human Approve.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default, schemars::JsonSchema)]
pub struct PullRequest {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<PullRequestEnd>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<PullRequestEnd>,
    /// True after GitHub reports this PR merged. Review stays until every
    /// listed PR is merged.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub merged: bool,
    /// When the agent recorded this PR on the card. Oldest unmerged timestamp
    /// drives Review staleness reporting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(skip)]
    pub reported_at: Option<DateTime<Utc>>,
}

impl PullRequest {
    pub fn from_url(url: impl Into<String>) -> Self {
        Self {
            url: url.into().trim().to_string(),
            base: None,
            head: None,
            merged: false,
            reported_at: None,
        }
    }

    pub fn url_str(&self) -> Option<&str> {
        let u = self.url.trim();
        if u.is_empty() {
            None
        } else {
            Some(u)
        }
    }

    /// Base+head present — enough to clone without inventing a fork.
    pub fn has_forge_ends(&self) -> bool {
        self.base.as_ref().is_some_and(PullRequestEnd::is_usable)
            && self.head.as_ref().is_some_and(PullRequestEnd::is_usable)
    }

    pub fn to_repo_config(&self) -> Option<crate::schema::RepoConfig> {
        let base = self.base.as_ref().filter(|b| b.is_usable())?;
        let head = self.head.as_ref().filter(|h| h.is_usable()).unwrap_or(base);
        Some(
            crate::schema::RepoConfig {
                upstream: base.repo.clone(),
                fork: head.repo.clone(),
                base: base.git_ref.clone(),
            }
            .normalized(),
        )
    }

    /// `owner/name` to push against: head (fork), else base, else the PR URL.
    pub fn push_owner_repo(&self) -> Option<String> {
        if let Some(h) = self.head.as_ref().filter(|h| !h.repo.trim().is_empty()) {
            return crate::schema::parse_owner_name(&h.repo).ok();
        }
        if let Some(b) = self.base.as_ref().filter(|b| !b.repo.trim().is_empty()) {
            return crate::schema::parse_owner_name(&b.repo).ok();
        }
        self.url_str()
            .and_then(crate::store::parse_github_pr_url)
            .map(|(owner_repo, _)| owner_repo)
    }
}

/// One Task row as shown to an agent from the Project Plan.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PlanTaskBrief {
    pub key: String,
    pub title: String,
    pub intent: String,
    pub definition_of_done: String,
    #[serde(default)]
    pub blocked_by_keys: Vec<String>,
    /// True when this row is the card being claimed.
    #[serde(default)]
    pub current: bool,
}

/// Desired OpenShell provider (Settings → OpenShell → Providers).
///
/// Sandboard is source of truth; Sync/Apply pushes to the gateway via gRPC.
/// Credential values are sealed — GET APIs expose keys only.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OpenShellProviderDesired {
    pub name: String,
    #[serde(rename = "type")]
    pub provider_type: String,
    #[serde(default)]
    pub config: BTreeMap<String, String>,
    /// Sealed JSON object of credential key → value (never returned on GET).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credentials_sealed: Option<String>,
    /// Plain keys present in the sealed credentials blob (safe to return).
    #[serde(default)]
    pub credential_keys: Vec<String>,
    /// Optional gateway-owned refresh bootstrap (e.g. gcloud ADC).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh: Option<OpenShellProviderRefreshDesired>,
}

/// Refresh material for [`OpenShellProviderDesired`] (Vertex ADC, etc.).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OpenShellProviderRefreshDesired {
    /// Provider credential env key that refresh writes into (e.g. `GOOGLE_VERTEX_AI_TOKEN`).
    pub credential_key: String,
    /// Proto strategy name: `oauth2_refresh_token`, `google_service_account_jwt`, …
    pub strategy: String,
    /// Sealed JSON object of refresh material key → value.
    pub material_sealed: String,
    /// Material keys treated as secret by the gateway.
    #[serde(default)]
    pub secret_material_keys: Vec<String>,
}

impl OpenShellProviderDesired {
    pub fn normalized(mut self) -> Self {
        self.name = self.name.trim().to_string();
        self.provider_type = self.provider_type.trim().to_string();
        self.config = self
            .config
            .into_iter()
            .map(|(k, v)| (k.trim().to_string(), v))
            .filter(|(k, _)| !k.is_empty())
            .collect();
        self.credential_keys = self
            .credential_keys
            .into_iter()
            .map(|k| k.trim().to_string())
            .filter(|k| !k.is_empty())
            .collect();
        self.credential_keys.sort();
        self.credential_keys.dedup();
        if let Some(ref mut r) = self.refresh {
            r.credential_key = r.credential_key.trim().to_string();
            r.strategy = r.strategy.trim().to_string();
            r.secret_material_keys = r
                .secret_material_keys
                .iter()
                .map(|k| k.trim().to_string())
                .filter(|k| !k.is_empty())
                .collect();
        }
        self
    }

    pub fn has_credentials(&self) -> bool {
        self.credentials_sealed
            .as_ref()
            .is_some_and(|s| !s.trim().is_empty())
            || self.refresh.is_some()
    }
}

/// Per-install agent process knobs (Settings → Agent runtime).
///
/// Empty boards seed from compiled [`Default`]. Board is source of truth after.
/// Image / policy / cpu / memory live on sandbox profiles; work remotes on
/// card `pull_requests`. Branch / sandbox names are fixed `sandboard/card-*` (not
/// a Settings knob).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AgentRuntimeConfig {
    /// Primary agent CLI (`cursor`, `agy`, `claude`, `opencode`, or `hermes`).
    #[serde(default = "default_runtime_engine")]
    pub engine: String,
    #[serde(default = "default_runtime_concurrent")]
    pub max_concurrent: usize,
    #[serde(default = "default_runtime_timeout")]
    pub agent_timeout_secs: u64,
    #[serde(default = "default_runtime_attempts")]
    pub max_attempts: u32,
    /// How often the supervisor checks overdue run deadlines (ms).
    #[serde(default = "default_runtime_sweep")]
    pub sweep_interval_ms: u64,
    /// Board-wide standing agent policy (briefing). Default essay from
    /// [`DEFAULT_BOARD_STANDING_PROMPT`]. Project `project_prompt` is extras.
    #[serde(default = "default_runtime_standing_prompt")]
    pub standing_prompt: String,
}

fn default_runtime_engine() -> String {
    "cursor".into()
}
fn default_runtime_concurrent() -> usize {
    2
}
fn default_runtime_timeout() -> u64 {
    1800
}
fn default_runtime_attempts() -> u32 {
    3
}
fn default_runtime_sweep() -> u64 {
    2000
}
fn default_runtime_standing_prompt() -> String {
    DEFAULT_BOARD_STANDING_PROMPT.to_string()
}

impl Default for AgentRuntimeConfig {
    fn default() -> Self {
        Self {
            engine: default_runtime_engine(),
            max_concurrent: default_runtime_concurrent(),
            agent_timeout_secs: default_runtime_timeout(),
            max_attempts: default_runtime_attempts(),
            sweep_interval_ms: default_runtime_sweep(),
            standing_prompt: default_runtime_standing_prompt(),
        }
    }
}

impl AgentRuntimeConfig {
    /// Trim string fields; normalize counters.
    pub fn normalized(mut self) -> Self {
        self.engine = self.engine.trim().to_string();
        if self.engine.is_empty() {
            self.engine = default_runtime_engine();
        }
        self.standing_prompt = self.standing_prompt.trim().to_string();
        if self.max_concurrent == 0 {
            self.max_concurrent = 1;
        }
        if self.agent_timeout_secs == 0 {
            self.agent_timeout_secs = default_runtime_timeout();
        }
        if self.max_attempts == 0 {
            self.max_attempts = default_runtime_attempts();
        }
        if self.sweep_interval_ms < 100 {
            self.sweep_interval_ms = default_runtime_sweep();
        }
        self
    }
}

/// Settings → Forge: poll GitHub when webhooks are missing or delayed.
///
/// When enabled, sandboard polls on `interval_secs` **in addition to** webhooks.
/// Both paths call the same Board methods (merge → Done, tip → MainAdvanced).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WebhookPollConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Seconds between ticks. Clamped to ≥ [`MIN_WEBHOOK_POLL_INTERVAL_SECS`].
    #[serde(default = "default_webhook_poll_interval_secs")]
    pub interval_secs: u64,
    /// OpenShell provider instance that supplies the host poll token
    /// (`github-app` mint, or a `github` / other row with sealed `GH_TOKEN`).
    /// Required when polling is enabled — never inferred.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_name: Option<String>,
}

/// Floor for poll interval (Settings + loop). Below this, GitHub rate limits hurt.
pub const MIN_WEBHOOK_POLL_INTERVAL_SECS: u64 = 15;

fn default_webhook_poll_interval_secs() -> u64 {
    60
}

impl Default for WebhookPollConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            interval_secs: default_webhook_poll_interval_secs(),
            provider_name: None,
        }
    }
}

impl WebhookPollConfig {
    /// Clamp interval; trim provider name (empty → None).
    pub fn normalized(mut self) -> Self {
        if self.interval_secs < MIN_WEBHOOK_POLL_INTERVAL_SECS {
            self.interval_secs = MIN_WEBHOOK_POLL_INTERVAL_SECS;
        }
        self.provider_name = self
            .provider_name
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        self
    }
}

/// Per-install forge identity (Settings → Forge).
/// Work remotes live on each card's [`PullRequest`] after the agent reports.
/// See `docs/architecture.md`.
///
/// Legacy wire keys (`beads_sync_repo`, `upstream`, `branching_prompt`) are
/// ignored on deserialize so old Settings payloads still load.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkspaceBinding {
    /// Forge provider. Only `github` is implemented; `gitlab` is a future seam.
    #[serde(default = "default_forge")]
    pub forge: String,
}

/// How sandboard authenticates to the OpenShell gateway. Explicit Settings choice —
/// never inferred from PEMs, tokens, or URL scheme.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OpenShellAuthMode {
    Mtls,
    Oidc,
}

impl OpenShellAuthMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mtls => "mtls",
            Self::Oidc => "oidc",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "mtls" => Some(Self::Mtls),
            "oidc" => Some(Self::Oidc),
            _ => None,
        }
    }
}

/// Non-secret OIDC client settings for gateway auth (Settings → OpenShell).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct OpenShellOidcConfig {
    #[serde(default)]
    pub issuer: String,
    #[serde(default)]
    pub client_id: String,
    #[serde(default)]
    pub audience: String,
}

impl OpenShellOidcConfig {
    pub fn trimmed(self) -> Self {
        Self {
            issuer: self.issuer.trim().trim_end_matches('/').to_string(),
            client_id: self.client_id.trim().to_string(),
            audience: self.audience.trim().to_string(),
        }
    }

    pub fn is_complete(&self) -> bool {
        !self.issuer.trim().is_empty() && !self.client_id.trim().is_empty()
    }

    /// Issuer must be an `https://` URL (same rule as the gateway endpoint).
    pub fn validate(&self) -> Result<(), String> {
        let issuer = self.issuer.trim();
        if issuer.is_empty() {
            return Err("OIDC issuer is required".into());
        }
        if issuer.starts_with("http://") {
            return Err(
                "OIDC issuer must be https:// (Keycloak is not reachable over plaintext HTTP)"
                    .into(),
            );
        }
        if !issuer.starts_with("https://") {
            return Err("OIDC issuer must be an https:// URL".into());
        }
        if self.client_id.trim().is_empty() {
            return Err("OIDC client id is required".into());
        }
        Ok(())
    }
}

fn default_forge() -> String {
    "github".into()
}

impl Default for WorkspaceBinding {
    fn default() -> Self {
        Self {
            forge: default_forge(),
        }
    }
}

/// Hold for the durable control-plane cockpit. Distinct from card `parked`:
/// this is not claim/heartbeat/report lifecycle — it is the Board record that
/// lets chat/TTY reconnect keep the same sandbox + conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CockpitSessionStatus {
    /// Cockpit agent may be live in the sandbox.
    #[default]
    Running,
    /// Park-like hold: sandbox + conversation kept; agent stopped until resume.
    Parked,
}

/// Supervisor-owned cockpit sandbox lifecycle for UI feedback.
///
/// Distinct from [`CockpitSessionStatus`] (Running/Parked hold): this tracks
/// OpenShell create/delete/ready so the UI can explain Stop→Start reclaim delays.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CockpitSandboxPhase {
    /// No session / unknown (legacy rows).
    #[default]
    Idle,
    /// Session created; seat loop has not begun create yet.
    Starting,
    /// Waiting for a prior sandbox delete (or name collision) to finish.
    WaitingForDelete,
    /// Creating / waiting until OpenShell reports Ready.
    Provisioning,
    /// Environment published; attach/MCP may proceed.
    Ready,
    /// Stop requested; sandbox reap in flight (session may clear soon after).
    Stopping,
    /// Seat/provision failure; see `phase_detail`.
    Error,
}

/// Durable cockpit-session singleton on the Board. Chat and TTY are faces over this
/// record — they must not grow a second lifecycle.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CockpitSession {
    /// OpenShell sandbox environment name (e.g. `sandboard-cockpit`).
    #[serde(default)]
    pub environment: Option<String>,
    /// agy conversation id for reconnect (`--conversation`).
    #[serde(default)]
    pub conversation_id: Option<String>,
    #[serde(default)]
    pub status: CockpitSessionStatus,
    /// OpenShell sandbox lifecycle (supervisor writes; UI reads).
    #[serde(default)]
    pub sandbox_phase: CockpitSandboxPhase,
    /// Short human line for the current phase (e.g. reclaim wait).
    #[serde(default)]
    pub phase_detail: Option<String>,
    /// When `sandbox_phase` last changed (elapsed UI).
    #[serde(default = "Utc::now")]
    pub phase_since: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl CockpitSession {
    pub fn new(environment: Option<String>, conversation_id: Option<String>) -> Self {
        let now = Utc::now();
        let env = normalize_cockpit_field(environment);
        // Env already known → treat as ready (rare on create); else starting.
        let (sandbox_phase, phase_detail) = if env.is_some() {
            (CockpitSandboxPhase::Ready, None)
        } else {
            (CockpitSandboxPhase::Starting, None)
        };
        Self {
            environment: env,
            conversation_id: normalize_cockpit_field(conversation_id),
            status: CockpitSessionStatus::Running,
            sandbox_phase,
            phase_detail,
            phase_since: now,
            created_at: now,
            updated_at: now,
        }
    }

    /// Apply a sandbox phase change; bumps `phase_since` only when the phase differs.
    pub fn set_sandbox_phase(
        &mut self,
        phase: CockpitSandboxPhase,
        detail: Option<String>,
    ) {
        let detail = normalize_cockpit_field(detail);
        if self.sandbox_phase != phase {
            self.sandbox_phase = phase;
            self.phase_since = Utc::now();
        }
        self.phase_detail = detail;
        self.updated_at = Utc::now();
    }
}

/// Trim; empty → `None`.
pub fn normalize_cockpit_field(value: Option<String>) -> Option<String> {
    value
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Board-owned OpenShell policy (Settings → OpenShell → Policies).
///
/// Specs reference these by id; create materializes `yaml` for OpenShell.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OpenShellPolicy {
    pub id: String,
    pub name: String,
    /// Inline OpenShell policy YAML text.
    pub yaml: String,
}

/// Shipped board MCP server id for the host sandboard Streamable HTTP seat.
pub const SANDBOARD_MCP_SERVER_ID: &str = "sandboard";

/// Which sandboxes may receive this MCP server.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum McpAudience {
    /// Operator cockpit seat only (includes host `/mcp`).
    #[default]
    Cockpit,
    /// Card-worker sandboxes only (never host `/mcp`).
    Worker,
    /// Both cockpit and workers.
    Both,
}

impl McpAudience {
    pub fn allows_cockpit(self) -> bool {
        matches!(self, Self::Cockpit | Self::Both)
    }

    pub fn allows_worker(self) -> bool {
        matches!(self, Self::Worker | Self::Both)
    }
}

/// HTTP MCP auth for engine client config.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum McpHttpAuth {
    /// No Authorization header.
    None,
    /// Host-minted cockpit seat Bearer for the shipped `sandboard` MCP only.
    /// Not an operator-facing auth choice — inject/`ensure_cockpit_sandboard_mcp_attach`
    /// wire this behind the scenes.
    CockpitBearer,
    /// Host-mediated MCP OAuth: OpenShell provider holds refresh; inject uses
    /// the credential env placeholder (gateway rewrites on egress).
    ///
    /// Serialized as `oauth` (not snake_case `o_auth`). `o_auth` remains an
    /// alias for rows written before the rename.
    #[serde(rename = "oauth", alias = "o_auth")]
    OAuth { provider: String, env: String },
}

/// How an MCP server is exposed to the agent engine.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum McpTransport {
    Http {
        url: String,
        #[serde(default = "default_mcp_http_auth")]
        auth: McpHttpAuth,
    },
    Stdio {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        cwd: Option<String>,
    },
}

fn default_mcp_http_auth() -> McpHttpAuth {
    McpHttpAuth::None
}

/// Board catalog entry for an MCP server (Settings → OpenShell → MCP servers).
///
/// Specs attach by id; create merges policy fragments + provider names, and
/// inject writes Cursor/Claude/agy/OpenCode client config.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct McpServerDesired {
    pub id: String,
    pub name: String,
    pub transport: McpTransport,
    /// Optional OpenShell YAML fragment merged into the sandbox policy at create.
    /// Accepts a full policy document or a bare `network_policies:` map.
    #[serde(default)]
    pub policy_fragment_yaml: Option<String>,
    /// Extra OpenShell provider names required by this server.
    #[serde(default)]
    pub provider_names: Vec<String>,
    /// Non-secret env for stdio children (and optional HTTP client hints).
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub audience: McpAudience,
    /// Seeded from the repo; operators may edit.
    #[serde(default)]
    pub shipped: bool,
}

impl McpServerDesired {
    pub fn normalized(mut self) -> Result<Self, String> {
        self.id = self.id.trim().to_string();
        self.name = self.name.trim().to_string();
        if self.name.is_empty() {
            return Err("mcp server name must not be empty".into());
        }
        self.provider_names = self
            .provider_names
            .into_iter()
            .map(|n| n.trim().to_string())
            .filter(|n| !n.is_empty())
            .collect();
        self.env = self
            .env
            .into_iter()
            .map(|(k, v)| (k.trim().to_string(), v))
            .filter(|(k, _)| !k.is_empty())
            .collect();
        if let Some(frag) = self.policy_fragment_yaml.take() {
            let t = frag.trim();
            self.policy_fragment_yaml = if t.is_empty() {
                None
            } else {
                Some(t.to_string())
            };
        }
        match &mut self.transport {
            McpTransport::Http { url, auth } => {
                *url = url.trim().to_string();
                if url.is_empty() && !matches!(auth, McpHttpAuth::CockpitBearer) {
                    return Err("http mcp server url must not be empty".into());
                }
                if let McpHttpAuth::OAuth { provider, env } = auth {
                    *provider = provider.trim().to_string();
                    *env = env.trim().to_string();
                    if provider.is_empty() {
                        return Err("oauth auth requires provider".into());
                    }
                    if env.is_empty() {
                        return Err("oauth auth requires env".into());
                    }
                }
            }
            McpTransport::Stdio {
                command,
                args,
                cwd,
            } => {
                *command = command.trim().to_string();
                // Shipped sandboard placeholder — cockpit_mcp resolves it to the
                // `socat - UNIX-CONNECT:<AGENT_SOCK_PATH>` relay client at
                // inject time, the same way the shipped Http entry leaves
                // `url` empty.
                if command.is_empty() && self.id != SANDBOARD_MCP_SERVER_ID {
                    return Err("stdio mcp server command must not be empty".into());
                }
                *args = args.iter().map(|a| a.to_string()).collect();
                if let Some(c) = cwd.take() {
                    let t = c.trim();
                    *cwd = if t.is_empty() {
                        None
                    } else {
                        Some(t.to_string())
                    };
                }
            }
        }
        if let McpTransport::Http {
            auth: McpHttpAuth::OAuth { provider, .. },
            ..
        } = &self.transport
        {
            if !self.provider_names.iter().any(|n| n == provider) {
                self.provider_names.push(provider.clone());
            }
        }
        if matches!(
            &self.transport,
            McpTransport::Http {
                auth: McpHttpAuth::CockpitBearer,
                ..
            }
        ) {
            if self.id != SANDBOARD_MCP_SERVER_ID {
                return Err(
                    "cockpit_bearer is reserved for the shipped sandboard cockpit MCP".into(),
                );
            }
            if !matches!(self.audience, McpAudience::Cockpit) {
                return Err("shipped sandboard MCP must use cockpit audience".into());
            }
        }
        Ok(self)
    }

    /// Shipped host sandboard MCP: stdio over the cockpit sandbox's local
    /// `socat`-over-unix-socket relay (see `cockpit_mcp_tunnel`) — no network
    /// hop, no Bearer. `command` empty is the inject-time placeholder;
    /// `cockpit_mcp::render_*` resolve it the same way they resolve an empty
    /// HTTP `url`.
    pub fn shipped_sandboard() -> Self {
        Self {
            id: SANDBOARD_MCP_SERVER_ID.into(),
            name: "sandboard".into(),
            transport: McpTransport::Stdio {
                command: String::new(),
                args: Vec::new(),
                cwd: None,
            },
            policy_fragment_yaml: None,
            provider_names: Vec::new(),
            env: BTreeMap::new(),
            audience: McpAudience::Cockpit,
            shipped: true,
        }
    }
}

/// Named create-spec for OpenShell sandboxes. Board-state catalog entries;
/// empty catalogs seed from compiled [`crate::schema::AgentConfig::default`]
/// and embedded policy constants (not from host `sandboard.yaml` create knobs).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SandboxProfile {
    pub id: String,
    pub name: String,
    /// Passed to `openshell sandbox create --from`.
    pub image: String,
    /// Policies catalog id. Required on upsert; resolved to YAML at create.
    #[serde(default)]
    pub policy_id: String,
    /// Pre-catalog boards stored inline YAML (or a host path) under `policy`.
    /// One-shot load migration only — never written back.
    #[serde(default, rename = "policy", skip_serializing)]
    pub policy_inline_legacy: Option<String>,
    #[serde(default)]
    pub cpu: Option<String>,
    #[serde(default)]
    pub memory: Option<String>,
    /// Agent CLI for cards using this profile (`cursor`, `agy`, `claude`, `opencode`, `hermes`).
    /// When unset, claim/run falls back to Settings → Agent runtime engine.
    #[serde(default)]
    pub engine: Option<String>,
    /// Model passed to the agent CLI when set (`agy --model`, `hermes --model`, etc.).
    /// When unset, claim/run resolves card.model → engine default.
    #[serde(default)]
    pub model: Option<String>,
    /// OpenShell provider names to attach on sandbox create for this profile.
    /// Empty = attach none. Unknown names are dropped at create time.
    #[serde(default)]
    pub provider_names: Vec<String>,
    /// MCP server catalog ids to attach (config inject + policy/provider merge).
    /// Empty = none from the catalog. Cockpit always attaches shipped `sandboard`
    /// (profile ensure + resolve + inject) even when omitted here.
    #[serde(default)]
    pub mcp_server_ids: Vec<String>,
    /// Non-secret env overlaid onto agent env at sandbox create (profile wins on clash).
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Seat notes injected into cold/Cockpit briefing when non-empty.
    #[serde(default)]
    pub prompt: Option<String>,
    /// Seeded from the repo (one per split `sandbox-<engine>` image);
    /// operators may edit. See `store::ensure_shipped_sandbox_profiles`.
    #[serde(default)]
    pub shipped: bool,
}

/// Create-form / last-resort knobs when the catalog has no matching profile.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SandboxProfileCreateDefaults {
    pub name: String,
    pub image: String,
    /// Prefill: seeded minimal Policies catalog id.
    pub policy_id: String,
    pub cpu: Option<String>,
    pub memory: Option<String>,
    pub engine: Option<String>,
}

/// Minimal defaults for Settings → Sandbox specs → Create.
pub fn sandbox_profile_create_defaults() -> SandboxProfileCreateDefaults {
    let agents = crate::schema::AgentConfig::default();
    let engine = {
        let e = agents.engine.trim();
        if e.is_empty() {
            None
        } else {
            Some(e.to_string())
        }
    };
    SandboxProfileCreateDefaults {
        name: "Default".into(),
        image: agents.image,
        policy_id: crate::seed_policies::MINIMAL_POLICY_ID.to_string(),
        cpu: None,
        memory: None,
        engine,
    }
}

/// OpenShell provider instance name / provider type id for Antigravity (`agy`).
pub const ANTIGRAVITY_PROVIDER: &str = "antigravity";

/// Custom board provider type for Cursor Agent CLI (`CURSOR_API_KEY`).
/// Distinct from OpenShell builtin `cursor` (egress-only, no credentials).
pub const CURSOR_AGENT_PROVIDER_TYPE: &str = "cursor-agent";

/// Custom board provider type for Hermes Agent's OpenRouter API key.
pub const OPENROUTER_HERMES_PROVIDER_TYPE: &str = "openrouter-hermes";

/// Custom board provider type for GitHub App–minted `GH_TOKEN`.
/// Distinct from OpenShell builtin `github` (paste a PAT).
pub const GITHUB_APP_PROVIDER_TYPE: &str = "github-app";

/// Board-owned OpenShell provider type profile (Settings → OpenShell → Provider types).
///
/// YAML is the OpenShell profile document. `form_config_keys` drives non-secret
/// config fields on the Add Provider form (not declared in OpenShell YAML).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OpenShellProviderTypeDesired {
    pub id: String,
    pub yaml: String,
    /// Seeded from the repo; operators may edit yaml / form keys.
    #[serde(default)]
    pub shipped: bool,
    /// Non-secret config keys shown on Add Provider for this type.
    #[serde(default)]
    pub form_config_keys: Vec<String>,
}

impl OpenShellProviderTypeDesired {
    pub fn normalized(mut self) -> Self {
        self.id = self.id.trim().to_string();
        self.yaml = self.yaml.trim().to_string();
        self.form_config_keys = self
            .form_config_keys
            .into_iter()
            .map(|k| k.trim().to_string())
            .filter(|k| !k.is_empty())
            .collect();
        self.form_config_keys.sort();
        self.form_config_keys.dedup();
        self
    }
}

/// Stable id slug from a display name. Lowercase ASCII alphanumerics; runs of
/// whitespace/`_`/`-` become a single hyphen. Empty/punctuation-only names
/// fall back to `profile` so create never invents a blank key.
pub fn slugify_sandbox_profile_id(name: &str) -> String {
    let mut out = String::new();
    let mut pending_hyphen = false;
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            if pending_hyphen && !out.is_empty() {
                out.push('-');
            }
            pending_hyphen = false;
            out.push(c.to_ascii_lowercase());
        } else if c.is_whitespace() || c == '-' || c == '_' {
            pending_hyphen = true;
        }
        // other punctuation is dropped
    }
    if out.is_empty() {
        "profile".into()
    } else {
        out
    }
}

/// Create knobs after Project override → global default → compiled-default
/// resolution. `policy` is always YAML **content** ready to materialize as a
/// temp file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedSandboxCreate {
    pub image: String,
    pub policy: String,
    pub cpu: Option<String>,
    pub memory: Option<String>,
    /// Profile engine when set; compiled-default fallback carries `agents.engine`.
    pub engine: Option<String>,
    /// Profile model when set; unset for compiled-default fallback.
    pub model: Option<String>,
    /// Catalog profile that won, if any. `None` means compiled-default fallback.
    pub profile_id: Option<String>,
    /// Provider names to attach (from the winning profile + MCP servers; empty for fallback).
    pub providers: Vec<String>,
    /// MCP server catalog ids attached for this create (audience-filtered).
    pub mcp_server_ids: Vec<String>,
    /// Non-secret env from the winning profile (overlaid at create; profile wins on clash).
    pub env: BTreeMap<String, String>,
    /// Seat notes from the winning profile (briefing injection when non-empty).
    pub prompt: Option<String>,
}

impl ResolvedSandboxCreate {
    /// Build create knobs from a catalog profile + materialized policy YAML.
    pub fn from_profile(p: &SandboxProfile, policy_yaml: &str) -> Self {
        Self {
            image: p.image.clone(),
            policy: policy_yaml.to_string(),
            cpu: p.cpu.clone(),
            memory: p.memory.clone(),
            engine: p
                .engine
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string()),
            model: p
                .model
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string()),
            profile_id: Some(p.id.clone()),
            providers: p.provider_names.clone(),
            mcp_server_ids: p.mcp_server_ids.clone(),
            env: p.env.clone(),
            prompt: p
                .prompt
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string()),
        }
    }

    pub fn from_agents(agents: &crate::schema::AgentConfig) -> Self {
        let engine = {
            let e = agents.engine.trim();
            if e.is_empty() {
                None
            } else {
                Some(e.to_string())
            }
        };
        Self {
            image: agents.image.clone(),
            // Last-resort create knobs — usually AgentConfig::default(); never a host file.
            policy: resolve_policy_yaml(&agents.policy),
            cpu: agents.cpu.clone(),
            memory: agents.memory.clone(),
            engine,
            model: None,
            profile_id: None,
            providers: Vec::new(),
            mcp_server_ids: Vec::new(),
            env: BTreeMap::new(),
            prompt: None,
        }
    }
}

/// Heuristic: already-inline YAML vs a short marker / legacy path string.
pub fn is_inline_policy_yaml(s: &str) -> bool {
    let t = s.trim();
    t.contains('\n') || t.starts_with('#') || t.starts_with("version:")
}

/// Whether `execution.agents.policy` is a supported seed / YAML-fallback value.
///
/// Accepts only `embedded`, empty, the legacy `sandbox/policy.yaml` marker, or
/// already-inline YAML. Host paths are not a config surface here (one-shot
/// profile migration still inlines old path-valued catalog rows separately).
pub fn is_supported_agents_policy(policy: &str) -> bool {
    let t = policy.trim();
    t.is_empty() || t == "embedded" || t == "sandbox/policy.yaml" || is_inline_policy_yaml(policy)
}

/// Turn `execution.agents.policy` into last-resort YAML content.
///
/// Live policy is the board Policies catalog (referenced by sandbox specs).
/// This never reads a host policy file: inline YAML is returned as-is;
/// `embedded` / empty / legacy `sandbox/policy.yaml` (and any other non-inline
/// value) resolve to the minimal built-in default.
pub fn resolve_policy_yaml(path_or_yaml: &str) -> String {
    if is_inline_policy_yaml(path_or_yaml) {
        return path_or_yaml.to_string();
    }
    crate::seed_policies::MINIMAL_SANDBOX_POLICY.to_string()
}

/// If a stored profile still holds a host path (pre–inline-policy boards),
/// replace it with file contents when the path is readable.
///
/// One-shot upgrade only — do not reintroduce host paths as a supported
/// `execution.agents.policy` surface.
pub fn migrate_profile_policy_to_inline(policy: &str) -> Option<String> {
    if is_inline_policy_yaml(policy) {
        return None;
    }
    match std::fs::read_to_string(policy) {
        Ok(content) if !content.trim().is_empty() => Some(content),
        _ => None,
    }
}

/// Proposed sibling Tasks awaiting human Approve on a card (Initial plan or split).
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TaskProposal {
    #[serde(default)]
    pub summary: String,
    pub tasks: Vec<PlanTaskSpec>,
}

/// Child spec for `Board::propose_split` / `split.json` (deps match PlanTaskSpec).
#[derive(Debug, Clone)]
pub struct SplitChildSpec {
    pub title: String,
    pub intent: String,
    pub definition_of_done: String,
    pub key: Option<String>,
    pub blocked_by_keys: Vec<String>,
    /// Optional per-child remotes; Approve defaults from the splitting parent Task.
    pub repo: Option<crate::schema::RepoConfig>,
}

impl SplitChildSpec {
    pub fn new(
        title: impl Into<String>,
        intent: impl Into<String>,
        definition_of_done: impl Into<String>,
    ) -> Self {
        Self {
            title: title.into(),
            intent: intent.into(),
            definition_of_done: definition_of_done.into(),
            key: None,
            blocked_by_keys: Vec::new(),
            repo: None,
        }
    }

    #[must_use]
    #[allow(dead_code)] // used from unit tests; production builds via SplitChildSpec fields
    pub fn with_repo(mut self, repo: crate::schema::RepoConfig) -> Self {
        self.repo = Some(repo);
        self
    }

    #[must_use]
    #[allow(dead_code)] // used from unit tests; production builds via SplitChildSpec fields
    pub fn with_deps(mut self, key: impl Into<String>, blocked_by_keys: Vec<String>) -> Self {
        self.key = Some(key.into());
        self.blocked_by_keys = blocked_by_keys;
        self
    }
}

impl From<(String, String, String)> for SplitChildSpec {
    fn from((title, intent, definition_of_done): (String, String, String)) -> Self {
        Self::new(title, intent, definition_of_done)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkItem {
    pub id: ItemId,
    #[serde(default)]
    pub parent: Option<ItemId>,
    /// Label from the level schema. `None` for machine-created depth below the
    /// commitment line — it collapses into its nearest schema rung for display.
    #[serde(default)]
    pub level: Option<String>,

    /// Short and distinct. You cannot chunk what you cannot name.
    pub title: String,
    /// One sentence of intent. This chain is the highest-leverage payload in
    /// the system.
    pub intent: String,
    /// Every leaf must have one, mechanically checkable. Without it the tree is
    /// a wish list; with it, everything below the line is executable by
    /// construction.
    #[serde(default)]
    pub definition_of_done: Option<String>,

    pub state: State,
    pub origin: Origin,
    /// Above the line: human-approved, stable. Below: agents create, split and
    /// retire freely.
    #[serde(default)]
    pub above_line: bool,

    #[serde(default)]
    pub blocked_by: Vec<ItemId>,
    #[serde(default)]
    pub blockers: Vec<BlockerSummary>,
    #[serde(default)]
    pub capability: Option<String>,

    #[serde(default)]
    pub lease: Option<Lease>,
    /// Hard end of this run (`claim` + `agent_timeout_secs`). Not renewed.
    /// Sweeper requeues when past; UI shows countdown to this instant.
    #[serde(default)]
    pub run_deadline_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub engine: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    /// Display-only: card.model → profile model → engine default. Set on snapshot/detail.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_model: Option<String>,
    #[serde(default)]
    pub progress: f32,

    #[serde(default)]
    pub escalation: Option<Escalation>,
    #[serde(default)]
    pub gates: Vec<GateRun>,
    #[serde(default)]
    pub gate_failures: u32,
    /// Runs that died before producing anything — sandbox wouldn't start, clone
    /// failed, agent overran. Distinct from `gate_failures`, which means the
    /// work arrived and was judged wrong. Both have a retry budget; this one
    /// exists because early failures otherwise requeue forever with no signal.
    #[serde(default)]
    pub run_failures: u32,
    #[serde(default)]
    pub diff_added: u32,
    #[serde(default)]
    pub diff_removed: u32,

    #[serde(default)]
    pub notes: Vec<Note>,

    /// Standing agent instructions for this Project (Tasks inherit via claim).
    /// Null on Tasks. Optional Project-only standing extras (board policy is
    /// Settings → Agent runtime `standing_prompt`).
    #[serde(default)]
    pub project_prompt: Option<String>,

    /// Optional sandbox profile override for this Project. Null / unset means
    /// inherit [`crate::store::BoardState::default_sandbox_profile_id`].
    /// Null on Tasks.
    #[serde(default)]
    pub sandbox_profile_id: Option<String>,

    /// When true on a Project, the supervisor continuously queues claimable
    /// Backlog leaves under it (`awaiting_dispatch`). Tasks ignore this field.
    /// Default off — Backlog stays inert until Start/dispatch.
    #[serde(default)]
    pub auto_dispatch: bool,

    /// The bounce reason if this card was returned to Backlog due to an infra or execution bounce.
    #[serde(default)]
    pub last_bounce_reason: Option<String>,
    /// Conflicting file paths from the last rebase conflict.
    #[serde(default)]
    pub last_conflict_files: Vec<String>,

    /// The tree says *why*; the release target says *which shipped artifact*.
    /// These vary independently.
    #[serde(default)]
    pub release_target: Option<String>,
    /// The sandbox this card ran in, e.g. `sandboard-card-7`. Set by the supervisor
    /// at creation, and the key that lets a restarted sandboard find live sandboxes
    /// again instead of orphaning them.
    #[serde(default)]
    pub environment: Option<String>,
    /// agy conversation id for the current sandbox session. Park keeps it so
    /// the next claim can `--conversation` resume; halt clears it.
    #[serde(default)]
    pub conversation_id: Option<String>,
    /// Set by park: card is Backlog but must not be claimed until unpark.
    /// Unpark clears this and queues dispatch.
    #[serde(default)]
    pub parked: bool,
    /// Supervisor should claim this Backlog card. Set by Start / dispatch, or
    /// by unpark (resume). Cleared on claim, bounce to Backlog, or cancel.
    #[serde(default)]
    pub awaiting_dispatch: bool,
    /// Review catch-up retry queue: GitHub mergeable was UNKNOWN (or the check
    /// was deferred). Cleared on MERGEABLE (no-op) or CONFLICTING bounce. Not
    /// set when main advances under a still-MERGEABLE Review PR.
    #[serde(default)]
    pub rebase_requested: bool,
    /// Pull requests the agent opened (url + base/head). Approving surfaces
    /// them; merging stays a human action. Review does not leave until every
    /// listed PR is merged. Legacy singular `pull_request` / `pr_url` migrate here.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pull_requests: Vec<PullRequest>,
    /// Legacy singular wire field — read on load, never written.
    #[serde(default, rename = "pull_request", skip_serializing)]
    pub legacy_pull_request: Option<PullRequest>,
    /// Legacy wire field — read on load, never written.
    #[serde(default, rename = "pr_url", skip_serializing)]
    pub legacy_pr_url: Option<String>,

    /// Durable product remotes for a claimable Task (`upstream` required;
    /// optional `fork`; `base` defaults to `main`). Null on Projects — remotes
    /// are task-scoped, never a Project `product_repo`. After report,
    /// [`Self::pull_requests`] still wins for resume (see `resolve_card_repo`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<crate::schema::RepoConfig>,

    /// Plan artifact — Projects only (Phase 1). Source of truth for Approve Plan.
    #[serde(default)]
    pub plan: Option<PlanArtifact>,

    /// Proposed sibling Tasks on this card (Initial plan or impl split). Approve
    /// materializes them; request_changes clears. Null when none.
    #[serde(default)]
    pub proposal: Option<TaskProposal>,

    pub created_at: DateTime<Utc>,
    pub entered_state_at: DateTime<Utc>,
    #[serde(default)]
    pub history: Vec<Transition>,
}

impl WorkItem {
    pub fn new(id: ItemId, title: impl Into<String>, intent: impl Into<String>) -> Self {
        let now = Utc::now();
        Self {
            id,
            parent: None,
            level: None,
            title: title.into(),
            intent: intent.into(),
            definition_of_done: None,
            state: State::Draft,
            origin: Origin::Human,
            above_line: false,
            blocked_by: Vec::new(),
            blockers: Vec::new(),
            capability: None,
            lease: None,
            run_deadline_at: None,
            engine: None,
            model: None,
            resolved_model: None,
            progress: 0.0,
            escalation: None,
            gates: Vec::new(),
            gate_failures: 0,
            run_failures: 0,
            diff_added: 0,
            diff_removed: 0,
            notes: Vec::new(),
            project_prompt: None,
            sandbox_profile_id: None,
            auto_dispatch: false,
            last_bounce_reason: None,
            last_conflict_files: Vec::new(),
            release_target: None,
            environment: None,
            conversation_id: None,
            parked: false,
            awaiting_dispatch: false,
            rebase_requested: false,
            pull_requests: Vec::new(),
            legacy_pull_request: None,
            legacy_pr_url: None,
            repo: None,
            plan: None,
            proposal: None,
            created_at: now,
            entered_state_at: now,
            history: Vec::new(),
        }
    }

    pub fn is_project(&self) -> bool {
        self.parent.is_none() && self.level.as_deref() != Some("Task")
    }

    /// Primary PR HTML URL: oldest unmerged, else first listed (`pull_requests`).
    pub fn pr_url(&self) -> Option<&str> {
        self.unmerged_prs()
            .find_map(PullRequest::url_str)
            .or_else(|| self.pull_requests.iter().find_map(PullRequest::url_str))
    }

    /// Every listed PR with a non-empty URL.
    pub fn pr_urls(&self) -> Vec<&str> {
        self.pull_requests
            .iter()
            .filter_map(PullRequest::url_str)
            .collect()
    }

    /// Listed PRs that GitHub has not yet marked merged.
    pub fn unmerged_prs(&self) -> impl Iterator<Item = &PullRequest> {
        self.pull_requests
            .iter()
            .filter(|p| !p.merged && p.url_str().is_some())
    }

    /// True when the card lists at least one PR and every listed PR is merged.
    pub fn all_pull_requests_merged(&self) -> bool {
        !self.pull_requests.is_empty()
            && self
                .pull_requests
                .iter()
                .all(|p| p.merged || p.url_str().is_none())
    }

    /// Age for Review staleness: oldest unmerged PR `reported_at`, else time in state.
    pub fn oldest_unmerged_age(&self, now: DateTime<Utc>) -> Duration {
        self.unmerged_prs()
            .filter_map(|p| p.reported_at)
            .min()
            .map(|at| now - at)
            .unwrap_or_else(|| self.time_in_state(now))
    }

    /// Fold legacy singular `pull_request` / top-level `pr_url` into [`Self::pull_requests`].
    pub fn migrate_legacy_pr_url(&mut self) {
        if let Some(pr) = self.legacy_pull_request.take() {
            if self.pull_requests.is_empty()
                && (pr.url_str().is_some() || pr.has_forge_ends())
            {
                self.pull_requests.push(pr);
            }
        }
        let Some(url) = self.legacy_pr_url.take() else {
            return;
        };
        let url = url.trim().to_string();
        if url.is_empty() {
            return;
        }
        if self.pull_requests.is_empty() {
            self.pull_requests.push(PullRequest::from_url(url));
            return;
        }
        if let Some(pr) = self.pull_requests.first_mut() {
            if pr.url.trim().is_empty() {
                pr.url = url;
            }
        }
    }

    pub fn is_initial_plan_task(&self) -> bool {
        title_is_initial_plan(&self.title)
            || self
                .definition_of_done
                .as_deref()
                .is_some_and(|d| d.contains("Plan artifact approved"))
    }

    pub fn time_in_state(&self, now: DateTime<Utc>) -> Duration {
        now - self.entered_state_at
    }
}

/// Human-readable elapsed time. `4s`, `12m`, `3h 5m`.
pub fn humanize(d: Duration) -> String {
    let secs = d.num_seconds().max(0);
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else {
        let h = secs / 3600;
        let m = (secs % 3600) / 60;
        if m == 0 {
            format!("{h}h")
        } else {
            format!("{h}h {m}m")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pull_request_push_owner_repo_prefers_head() {
        let mut pr = PullRequest::from_url("https://github.com/acme/base/pull/9");
        assert_eq!(pr.push_owner_repo().as_deref(), Some("acme/base"));
        pr.base = Some(PullRequestEnd::new("acme/base", "main"));
        pr.head = Some(PullRequestEnd::new("forks/base", "sandboard/card-1"));
        assert_eq!(pr.push_owner_repo().as_deref(), Some("forks/base"));
    }

    #[test]
    fn legacy_ready_wire_value_loads_as_backlog() {
        let json = r#"{"id":1,"title":"t","intent":"i","state":"ready","origin":{"kind":"human"},"created_at":"2026-01-01T00:00:00Z","entered_state_at":"2026-01-01T00:00:00Z"}"#;
        let item: WorkItem = serde_json::from_str(json).expect("deserialize");
        assert_eq!(item.state, State::Backlog);
        assert_eq!(item.state.column(), Column::Backlog);
        assert!(!item.awaiting_dispatch);
    }

    #[test]
    fn slugify_sandbox_profile_id_from_display_name() {
        assert_eq!(slugify_sandbox_profile_id("Heavy CI"), "heavy-ci");
        assert_eq!(slugify_sandbox_profile_id("  Default  "), "default");
        assert_eq!(slugify_sandbox_profile_id("Foo_Bar--Baz"), "foo-bar-baz");
        assert_eq!(slugify_sandbox_profile_id("!!!"), "profile");
        assert_eq!(slugify_sandbox_profile_id(""), "profile");
        assert_eq!(slugify_sandbox_profile_id("A"), "a");
    }

    #[test]
    fn mcp_oauth_auth_auto_attaches_provider_name() {
        let s = McpServerDesired {
            id: "jira".into(),
            name: "Jira".into(),
            transport: McpTransport::Http {
                url: "https://mcp.example.com/v1".into(),
                auth: McpHttpAuth::OAuth {
                    provider: "mcp-jira".into(),
                    env: "MCP_OAUTH_JIRA_ACCESS_TOKEN".into(),
                },
            },
            policy_fragment_yaml: None,
            provider_names: vec![],
            env: BTreeMap::new(),
            audience: McpAudience::Both,
            shipped: false,
        }
        .normalized()
        .expect("normalize");
        assert_eq!(s.provider_names, vec!["mcp-jira".to_string()]);
    }

    #[test]
    fn cockpit_bearer_reserved_for_shipped_sandboard() {
        let err = McpServerDesired {
            id: "other".into(),
            name: "Other".into(),
            transport: McpTransport::Http {
                url: String::new(),
                auth: McpHttpAuth::CockpitBearer,
            },
            policy_fragment_yaml: None,
            provider_names: vec![],
            env: BTreeMap::new(),
            audience: McpAudience::Cockpit,
            shipped: false,
        }
        .normalized()
        .expect_err("foreign cockpit_bearer");
        assert!(err.contains("reserved"), "{err}");
        McpServerDesired::shipped_sandboard()
            .normalized()
            .expect("shipped sandboard ok");
    }

    #[test]
    fn minimal_sandbox_policy_parses_and_stays_minimal() {
        let policy = crate::seed_policies::MINIMAL_SANDBOX_POLICY;
        assert!(
            !policy.contains("sandboard-mcp") && !policy.contains("host.docker.internal"),
            "create default must not bake sandboard MCP egress"
        );
        assert!(
            !policy.contains("index.crates.io") && !policy.contains("/opt/rust"),
            "create default must not bake package registries or rust toolchain paths"
        );
        openshell_policy::parse_sandbox_policy(policy).expect("minimal policy parses");
        let defaults = sandbox_profile_create_defaults();
        assert_eq!(defaults.name, "Default");
        assert_eq!(
            defaults.policy_id,
            crate::seed_policies::MINIMAL_POLICY_ID
        );
        assert!(defaults.cpu.is_none());
        assert!(defaults.memory.is_none());
    }

    #[test]
    fn hermes_cockpit_policy_parses() {
        openshell_policy::parse_sandbox_policy(crate::seed_policies::COCKPIT_HERMES_POLICY)
            .expect("Hermes cockpit policy parses");
    }

    #[test]
    fn sandbox_profile_deserializes_legacy_inline_policy() {
        let json = r#"{
            "id": "default",
            "name": "Default",
            "image": "img:1",
            "policy": "version: 1\n# keep-bytes\n"
        }"#;
        let p: SandboxProfile = serde_json::from_str(json).expect("legacy profile");
        assert!(p.policy_id.is_empty());
        assert_eq!(
            p.policy_inline_legacy.as_deref(),
            Some("version: 1\n# keep-bytes\n")
        );
        let wire = serde_json::to_value(&p).expect("serialize");
        assert!(wire.get("policy").is_none(), "legacy field must not write back");
        assert_eq!(wire.get("policy_id").and_then(|v| v.as_str()), Some(""));
    }

    #[test]
    fn sandbox_profile_round_trips_model() {
        let p = SandboxProfile {
            id: "agy".into(),
            name: "AGY".into(),
            image: "img:1".into(),
            policy_id: "minimal".into(),
            policy_inline_legacy: None,
            cpu: None,
            memory: None,
            engine: Some("agy".into()),
            model: Some("claude-sonnet-4".into()),
            provider_names: Vec::new(),
            mcp_server_ids: Vec::new(),
            env: BTreeMap::new(),
            prompt: None,
            shipped: false,
        };
        let json = serde_json::to_string(&p).unwrap();
        assert!(json.contains("claude-sonnet-4"));
        let back: SandboxProfile = serde_json::from_str(&json).unwrap();
        assert_eq!(back.model.as_deref(), Some("claude-sonnet-4"));
        let legacy: SandboxProfile = serde_json::from_str(
            r#"{"id":"x","name":"X","image":"i","policy_id":"p"}"#,
        )
        .unwrap();
        assert!(legacy.model.is_none());
    }

    #[test]
    fn sandbox_profile_round_trips_policy_id() {
        let p = SandboxProfile {
            id: "default".into(),
            name: "Default".into(),
            image: "img:1".into(),
            policy_id: "minimal".into(),
            policy_inline_legacy: None,
            cpu: None,
            memory: None,
            engine: None,
            model: None,
            provider_names: Vec::new(),
            mcp_server_ids: Vec::new(),
            env: BTreeMap::new(),
            prompt: None,
            shipped: false,
        };
        let json = serde_json::to_string(&p).unwrap();
        assert!(json.contains("policy_id"));
        assert!(!json.contains("\"policy\""));
        let back: SandboxProfile = serde_json::from_str(&json).unwrap();
        assert_eq!(back.policy_id, "minimal");
        assert!(back.policy_inline_legacy.is_none());
    }

    #[test]
    fn sandbox_profile_round_trips_env_and_prompt() {
        let mut env = BTreeMap::new();
        env.insert("API_URL".into(), "https://example.test".into());
        let p = SandboxProfile {
            id: "oc".into(),
            name: "OpenShift".into(),
            image: "img:1".into(),
            policy_id: "minimal".into(),
            policy_inline_legacy: None,
            cpu: None,
            memory: None,
            engine: None,
            model: None,
            provider_names: Vec::new(),
            mcp_server_ids: Vec::new(),
            env,
            prompt: Some("Use oc against the cluster URL in API_URL.".into()),
            shipped: false,
        };
        let json = serde_json::to_string(&p).unwrap();
        let back: SandboxProfile = serde_json::from_str(&json).unwrap();
        assert_eq!(
            back.env.get("API_URL").map(String::as_str),
            Some("https://example.test")
        );
        assert_eq!(
            back.prompt.as_deref(),
            Some("Use oc against the cluster URL in API_URL.")
        );
        let legacy: SandboxProfile = serde_json::from_str(
            r#"{"id":"x","name":"X","image":"i","policy_id":"p"}"#,
        )
        .unwrap();
        assert!(legacy.env.is_empty());
        assert!(legacy.prompt.is_none());
    }

    #[test]
    fn resolved_sandbox_create_from_profile_carries_env_and_prompt() {
        let mut env = BTreeMap::new();
        env.insert("KUBECONFIG".into(), "/tmp/kube".into());
        let p = SandboxProfile {
            id: "p1".into(),
            name: "P1".into(),
            image: "img:1".into(),
            policy_id: "minimal".into(),
            policy_inline_legacy: None,
            cpu: None,
            memory: None,
            engine: None,
            model: None,
            provider_names: Vec::new(),
            mcp_server_ids: Vec::new(),
            env,
            prompt: Some("  seat notes  ".into()),
            shipped: false,
        };
        let resolved = ResolvedSandboxCreate::from_profile(&p, "version: 1\n");
        assert_eq!(
            resolved.env.get("KUBECONFIG").map(String::as_str),
            Some("/tmp/kube")
        );
        assert_eq!(resolved.prompt.as_deref(), Some("seat notes"));
    }

    #[test]
    fn resolve_policy_yaml_never_reads_host_paths() {
        let minimal = crate::seed_policies::MINIMAL_SANDBOX_POLICY;
        assert_eq!(resolve_policy_yaml("embedded"), minimal);
        assert_eq!(resolve_policy_yaml(""), minimal);
        assert_eq!(resolve_policy_yaml("sandbox/policy.yaml"), minimal);
        assert_eq!(resolve_policy_yaml("version: 1\n# inline\n"), "version: 1\n# inline\n");

        let dir = std::env::temp_dir().join(format!(
            "sandboard-test-resolve-policy-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("custom.yaml");
        std::fs::write(&path, "version: 1\n# must-not-load\n").unwrap();
        assert_eq!(
            resolve_policy_yaml(path.to_str().unwrap()),
            minimal,
            "YAML-fallback must not read an arbitrary host path"
        );
        let _ = std::fs::remove_dir_all(&dir);

        assert!(is_supported_agents_policy("embedded"));
        assert!(is_supported_agents_policy("sandbox/policy.yaml"));
        assert!(is_supported_agents_policy("version: 1\n"));
        assert!(!is_supported_agents_policy("sandbox/custom.yaml"));
        assert!(!is_supported_agents_policy(path.to_str().unwrap_or("gone")));
    }
}
