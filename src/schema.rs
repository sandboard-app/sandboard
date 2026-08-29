//! Project + Task — the only two rungs.
//!
//! Project is the container; Tasks are flat claimable leaves under it. The
//! engine still reads the tree (parent edges) for containment; task↔task
//! ordering lives in board dependency edges (`blocked_by`).

use crate::db::BoardDatabaseConfig;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Level {
    pub name: String,
    #[serde(default)]
    pub horizon: Option<String>,
    #[serde(default)]
    pub owner: Option<String>,
    #[serde(default)]
    pub elaborate: Option<String>,
    #[serde(default)]
    pub requires: Vec<String>,
    #[serde(default)]
    pub claimable: bool,
}

/// How work actually gets executed. The run budget is
/// `agents.agent_timeout_secs`; `lease_secs` / `heartbeat_expect_secs` /
/// `sweep_interval_ms` are ignored leftovers kept so older `sandboard.yaml` files
/// still parse. Live sweep interval is Settings → Agent runtime.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionConfig {
    /// Deprecated — ignored. Run deadline is `agents.agent_timeout_secs`.
    #[serde(default = "d_lease")]
    pub lease_secs: i64,
    /// Deprecated — ignored. UI shows countdown to `run_deadline_at`.
    #[serde(default = "d_hb")]
    pub heartbeat_expect_secs: i64,
    /// Deprecated — ignored. Live value is `AgentRuntimeConfig.sweep_interval_ms`.
    #[serde(default = "d_sweep")]
    pub sweep_interval_ms: u64,
    /// Compiled create-knob defaults (image/policy/engine/…). Live process
    /// knobs overlay from Settings → Agent runtime.
    #[serde(default)]
    pub agents: AgentConfig,
}

fn d_lease() -> i64 { 600 }
fn d_hb() -> i64 { 6 }
fn d_sweep() -> u64 { 2000 }

impl Default for ExecutionConfig {
    fn default() -> Self {
        Self {
            lease_secs: d_lease(),
            heartbeat_expect_secs: d_hb(),
            sweep_interval_ms: d_sweep(),
            agents: AgentConfig::default(),
        }
    }
}

/// Validate a GitHub-style `owner/name` (no URL, no trailing `.git`).
pub fn parse_owner_name(raw: &str) -> Result<String, String> {
    let s = raw.trim();
    if s.is_empty() {
        return Err("clone_repo is required (`owner/name`)".into());
    }
    if s.contains("://") || s.starts_with("git@") {
        return Err(format!(
            "clone_repo must be `owner/name`, not a URL ({s})"
        ));
    }
    let s = s.strip_suffix(".git").unwrap_or(s);
    let mut parts = s.split('/');
    let (Some(owner), Some(name), None) = (parts.next(), parts.next(), parts.next()) else {
        return Err(format!(
            "clone_repo must be exactly `owner/name` (got {s:?})"
        ));
    };
    if owner.is_empty()
        || name.is_empty()
        || !owner
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        return Err(format!(
            "clone_repo must be a valid `owner/name` (got {s:?})"
        ));
    }
    Ok(format!("{owner}/{name}"))
}

/// Standing line stamped into Project intent / Initial plan prose.
pub fn clone_repo_prose_line(owner_name: &str) -> String {
    format!(
        "Clone repository: {owner_name} into /sandbox/repo for planning and as the default Task clone target."
    )
}

/// Pull `owner/name` out of stamped prose (`Clone repository: owner/name …`).
pub fn clone_repo_from_prose(text: &str) -> Option<String> {
    for line in text.lines() {
        let t = line.trim();
        let Some(rest) = t
            .strip_prefix("Clone repository:")
            .or_else(|| t.strip_prefix("clone repository:"))
        else {
            continue;
        };
        let Some(token) = rest.split_whitespace().next() else {
            continue;
        };
        if let Ok(name) = parse_owner_name(token) {
            return Some(name);
        }
    }
    None
}

/// Resolved remotes for one card run (from card `pull_request` base/head).
///
/// Before a PR exists, `resolve_card_repo` returns `None` and the agent clones
/// from card prose. `upstream` = PR base repo; `fork` = head/push repo (same
/// as upstream for same-repo). Yaml `execution.agents.repo` is legacy/optional.
/// Containment is forge token permissions.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
pub struct RepoConfig {
    /// `owner/name` that PRs target.
    pub upstream: String,
    /// Optional distinct push remote (`owner/name`). Empty → same-repo.
    #[serde(default)]
    pub fork: String,
    #[serde(default = "d_base")]
    pub base: String,
}

fn d_base() -> String { "main".into() }

impl Default for RepoConfig {
    fn default() -> Self {
        Self { upstream: String::new(), fork: String::new(), base: d_base() }
    }
}

impl RepoConfig {
    /// Usable when the PR-target repo is known. Fork is optional.
    pub fn is_complete(&self) -> bool {
        !self.upstream.trim().is_empty()
    }

    /// Distinct push remote configured (cross-fork workflow).
    pub fn uses_cross_fork(&self) -> bool {
        let f = self.fork.trim();
        let u = self.upstream.trim();
        !f.is_empty() && !u.is_empty() && f != u
    }

    /// Clone and push target: fork when cross-fork, else upstream.
    pub fn clone_target(&self) -> &str {
        if self.uses_cross_fork() {
            self.fork.trim()
        } else {
            self.upstream.trim()
        }
    }

    /// Git ref to rebase onto / start from (`upstream/<base>` or `origin/<base>`).
    pub fn base_ref(&self) -> String {
        if self.uses_cross_fork() {
            format!("upstream/{}", self.base.trim())
        } else {
            format!("origin/{}", self.base.trim())
        }
    }

    /// Normalize empty base to `main`; trim owner/name fields.
    pub fn normalized(mut self) -> Self {
        if self.base.trim().is_empty() {
            self.base = d_base();
        }
        self.upstream = self.upstream.trim().to_string();
        self.fork = self.fork.trim().to_string();
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    /// Sandbox create image. Compiled default seeds empty board profiles;
    /// live edits are Settings → Sandbox specs.
    #[serde(default = "d_image")]
    pub image: String,
    /// Seed / last-resort policy marker: `embedded` (default), empty, legacy
    /// `sandbox/policy.yaml`, or already-inline YAML. Not a host file path —
    /// live policy is the board Policies catalog (referenced by sandbox specs).
    #[serde(default = "d_policy")]
    pub policy: String,
    /// Optional legacy remotes stanza. Card work remotes come from
    /// `pull_request`; not a live create-knob SoT.
    #[serde(default)]
    pub repo: RepoConfig,
    #[serde(default)]
    pub cpu: Option<String>,
    #[serde(default)]
    pub memory: Option<String>,
    /// Sandboxes are heavy and this is alpha software. Do not start at seven.
    /// Primary agent CLI engine (`cursor`, `agy`, `claude`, `opencode`, or `hermes`).
    /// Seeded into profiles / Agent runtime from compiled defaults.
    #[serde(default = "d_engine")]
    pub engine: String,
    #[serde(default = "d_concurrent")]
    pub max_concurrent: usize,
    /// Hard ceiling on one agent run. Everything here fails as a hang.
    #[serde(default = "d_agent_timeout")]
    pub agent_timeout_secs: u64,
    /// Runs that die without producing work before the card becomes a human's
    /// problem. Without a count, early failures requeue forever.
    #[serde(default = "d_max_attempts")]
    pub max_attempts: u32,
}

fn d_image() -> String { "sandboard-sandbox:latest".into() }
/// Marker: resolve to the built-in worker seed policy (not a host file).
fn d_policy() -> String { "embedded".into() }
fn d_engine() -> String { "cursor".into() }
fn d_concurrent() -> usize { 2 }
fn d_agent_timeout() -> u64 { 1800 }
fn d_max_attempts() -> u32 { 3 }

/// Fixed stem for card branches and the Cockpit (`sandboard/card-N`, …).
pub const BRANCH_STEM: &str = "sandboard";
/// Short stem for OpenShell card sandbox names (OpenShell allows 19 characters).
pub const SANDBOX_STEM: &str = "sb";

/// Card feature branch: `sandboard/card-{id}`.
pub fn card_branch_name(id: impl std::fmt::Display) -> String {
    format!("{BRANCH_STEM}/card-{id}")
}

/// Sandbox name: `sb-card-{id}-a{attempt}`.
pub fn card_sandbox_name(id: impl std::fmt::Display, attempt: u32) -> String {
    format!("{SANDBOX_STEM}-card-{id}-a{attempt}")
}

/// Prefix match stem for reconcile keep: `sb-card-{id}-`.
pub fn card_sandbox_stem(id: impl std::fmt::Display) -> String {
    format!("{SANDBOX_STEM}-card-{id}-")
}

/// Prefix for card sandboxes created before the OpenShell name limit fix.
pub fn legacy_card_sandbox_stem(id: impl std::fmt::Display) -> String {
    format!("{BRANCH_STEM}-card-{id}-")
}

/// Stable singleton name for the control-plane cockpit: `sandboard-cockpit`.
pub fn cockpit_sandbox_name() -> String {
    format!("{BRANCH_STEM}-cockpit")
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            image: d_image(),
            policy: d_policy(),
            repo: RepoConfig::default(),
            cpu: None,
            memory: None,
            engine: d_engine(),
            max_concurrent: d_concurrent(),
            agent_timeout_secs: d_agent_timeout(),
            max_attempts: d_max_attempts(),
        }
    }
}

impl AgentConfig {
    /// Refuse to run rather than half-run. Every one of these presents as a
    /// hang if it's wrong at exec time, so check it at startup instead.
    ///
    /// Work remotes (`repo.upstream`, optional `fork`) are **not** required
    /// here: they resolve per card from `pr_url` (see
    /// `Board::resolve_card_repo`). An incomplete install default only fails
    /// when a card has no `pr_url` and no yaml upstream.
    pub fn validate(&self) -> Result<(), String> {
        // Live policy is the board Policies catalog. `agents.policy` is seed /
        // YAML-fallback only — never a host path that must exist on disk.
        if crate::model::is_supported_agents_policy(&self.policy) {
            return Ok(());
        }
        Err(format!(
            "execution.agents.policy {:?} is not supported (use embedded, empty, sandbox/policy.yaml, or inline YAML)",
            self.policy
        ))
    }
}

/// Top-level `board:` stanza — persistence and related control-plane settings.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BoardConfig {
    #[serde(default)]
    pub database: BoardDatabaseConfig,
}

/// Fixed Project + Task hierarchy (the only supported ladder).
pub fn default_levels() -> Vec<Level> {
    vec![
        Level {
            name: "Project".into(),
            horizon: Some("2q".into()),
            owner: Some("human".into()),
            elaborate: Some("on_commit".into()),
            requires: vec![],
            claimable: false,
        },
        Level {
            name: "Task".into(),
            horizon: Some("1d".into()),
            owner: Some("agent".into()),
            elaborate: None,
            requires: vec!["definition_of_done".into()],
            claimable: true,
        },
    ]
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Schema {
    /// Always Project + Task. Serde default fills compiled levels when absent
    /// from a leftover yaml fixture; boot no longer reads `sandboard.yaml`.
    #[serde(default = "default_levels")]
    pub levels: Vec<Level>,
    #[serde(default)]
    pub execution: ExecutionConfig,
    #[serde(default)]
    pub board: BoardConfig,
}

impl Default for Schema {
    fn default() -> Self {
        Self {
            levels: default_levels(),
            execution: ExecutionConfig::default(),
            board: BoardConfig::default(),
        }
    }
}

impl Schema {
    /// Depth 0 → Project, depth ≥1 → Task (flat under Project). Extra depth
    /// collapses to Task so a mistaken nest still labels correctly.
    pub fn level_for_depth(&self, depth: usize) -> Option<&Level> {
        if self.levels.is_empty() {
            return None;
        }
        if depth == 0 {
            self.levels.first()
        } else {
            self.levels.iter().find(|l| l.claimable).or_else(|| self.levels.last())
        }
    }

    pub fn project_level(&self) -> Option<&Level> {
        self.levels.iter().find(|l| !l.claimable).or_else(|| self.levels.first())
    }

    pub fn task_level(&self) -> Option<&Level> {
        self.levels.iter().find(|l| l.claimable).or_else(|| self.levels.last())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workable() -> AgentConfig {
        AgentConfig {
            repo: RepoConfig {
                upstream: "sandboard-app/sandboard".into(),
                fork: "clankrshq/sandboard".into(),
                base: "main".into(),
            },
            ..Default::default()
        }
    }

    #[test]
    fn default_schema_is_project_plus_task() {
        let s = Schema::default();
        assert_eq!(s.levels.len(), 2);
        assert_eq!(s.project_level().unwrap().name, "Project");
        assert_eq!(s.task_level().unwrap().name, "Task");
        assert!(s.task_level().unwrap().claimable);
        assert!(s.execution.agents.validate().is_ok());
        let db = s.board.database.parsed().expect("default sqlite url");
        assert_eq!(db.backend(), crate::db::DatabaseBackend::Sqlite);
    }

    /// Every one of these presents as a hang if it's wrong at exec time, so
    /// they are checked at startup instead.
    #[test]
    fn agent_policy_must_be_supported() {
        assert!(workable().validate().is_ok(), "the reference config must pass");

        // Work remotes are resolved per card — empty fork is fine at process start.
        let mut no_fork = workable();
        no_fork.repo.fork = String::new();
        assert!(no_fork.validate().is_ok());

        let mut bad_policy = workable();
        bad_policy.policy = "sandbox/does-not-exist.yaml".into();
        let err = bad_policy.validate().expect_err("host paths are not supported");
        assert!(
            err.contains("not supported"),
            "expected unsupported-policy error, got {err}"
        );

        // Existing path is still rejected — seed never reads host policy files.
        let mut path_policy = workable();
        path_policy.policy = "legacy-host-path.yaml".into();
        assert!(
            path_policy.validate().is_err(),
            "existing host path must not pass as agents.policy"
        );

        let mut inline = workable();
        inline.policy = "version: 1\n# inline-ok\n".into();
        assert!(inline.validate().is_ok());
    }

    #[test]
    fn card_branch_and_sandbox_names_are_fixed_sandboard() {
        assert_eq!(card_branch_name(7), "sandboard/card-7");
        assert_eq!(card_sandbox_name(7, 2), "sb-card-7-a2");
        assert_eq!(card_sandbox_stem(9), "sb-card-9-");
        assert!(card_sandbox_name(66, 2).len() <= 19);
        assert_eq!(cockpit_sandbox_name(), "sandboard-cockpit");
    }

    #[test]
    fn board_database_accepts_postgres_url_in_yaml() {
        let raw = r#"
levels:
  - name: Project
    claimable: false
  - name: Task
    claimable: true
board:
  database:
    url: postgres://sandboard:sandboard@127.0.0.1:5432/sandboard
"#;
        let s: Schema = serde_yaml::from_str(raw).expect("yaml");
        let db = s.board.database.parsed().expect("postgres url");
        assert_eq!(db.backend(), crate::db::DatabaseBackend::Postgres);
    }

    #[test]
    fn parse_owner_name_accepts_github_style() {
        assert_eq!(parse_owner_name(" sandboard-app/sandboard ").unwrap(), "sandboard-app/sandboard");
        assert_eq!(parse_owner_name("acme/widgets.git").unwrap(), "acme/widgets");
        assert!(parse_owner_name("").is_err());
        assert!(parse_owner_name("noslash").is_err());
        assert!(parse_owner_name("https://github.com/a/b").is_err());
        assert!(parse_owner_name("a/b/c").is_err());
    }

    #[test]
    fn clone_repo_from_prose_reads_stamped_line() {
        let text = "Rework settings.\n\nClone repository: sandboard-app/sandboard into /sandbox/repo for planning and as the default Task clone target.\n";
        assert_eq!(
            clone_repo_from_prose(text).as_deref(),
            Some("sandboard-app/sandboard")
        );
        assert!(clone_repo_from_prose("no stamp here").is_none());
    }
}
