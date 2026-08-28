//! The other UI. It happens to be an API, but it's a designed surface with the
//! same care — the operator has to be able to do the right thing without the
//! human reading the board, and an agent has to without any human at all.
//!
//! Two families share one state machine; seats gate which family is visible:
//!   * operator tools — what a liaison needs to triage and decide
//!   * worker verbs  — `list_ready` `claim` `heartbeat` `split` `escalate`
//!     `report` `report_pull_request` `release`, and nothing else
//!
//! `/mcp` (OAuth) is the **operator seat**: operator tools only. Worker verbs
//! stay on the host seat for supervisor/host tooling; the live supervisor path
//! calls `Board` directly. If the worker surface grows past roughly that size,
//! the orchestrator has started leaking its own complexity into the workers.

use crate::model::{Column, EscalationOption, ItemId, State};
use crate::store::SharedBoard;

use axum::extract::Request;
use axum::http::{header, HeaderValue, Method};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::Router;

use rmcp::handler::server::wrapper::{Json as ToolJson, Parameters};
use rmcp::transport::streamable_http_server::session::never::NeverSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{tool, tool_handler, tool_router, ErrorData, ServerHandler};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

fn bad(msg: impl Into<std::borrow::Cow<'static, str>>) -> ErrorData {
    ErrorData::invalid_params(msg, None)
}

type Out<T> = Result<ToolJson<T>, ErrorData>;

/// Which MCP tool family a session may use.
///
/// Operator (default `/mcp`) is the privileged chatbot / OAuth client. Host
/// keeps worker verbs for supervisor-facing tooling and tests; production card
/// lifecycle still goes through `Board` in `store.rs`, not this seat.
///
/// Not named `Cockpit`: the cockpit is a *sandbox seat* that happens to be one
/// client of this surface. Sharing a name made "is the cockpit down" ambiguous
/// between a container and an HTTP route.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum McpSeat {
    /// Operator tools only — no claim/heartbeat/report/report_pull_request/split/escalate/release/list_ready.
    #[default]
    Operator,
    /// Operator tools plus worker verbs (host / supervisor tooling).
    #[allow(dead_code)] // constructed via `Operator::host` (tests + host tooling)
    Host,
}

/// Worker-verb tool names. The operator seat hides these from `tools/list` and
/// rejects calls.
pub const WORKER_VERB_TOOLS: &[&str] = &[
    "list_ready",
    "claim",
    "heartbeat",
    "split",
    "escalate",
    "report",
    "report_pull_request",
    "release",
];

// ------------------------------------------------------------------ payloads

#[derive(Debug, Deserialize, JsonSchema)]
pub struct IdArg {
    /// Work item id, as shown on the card (`#41` is `41`).
    pub id: ItemId,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TextArg {
    pub id: ItemId,
    pub text: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReasonArg {
    pub id: ItemId,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AnswerArg {
    pub id: ItemId,
    /// The option label you are choosing, or free text if none of them fit.
    pub choice: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AutoDispatchArg {
    /// Project id (`#167` is `167`).
    pub id: ItemId,
    /// `true` = play (auto-queue Backlog); `false` = pause (clear queue).
    pub enabled: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CreateProjectArg {
    /// Short and distinct — you cannot chunk what you cannot name.
    pub title: String,
    /// One sentence of product intent (not the clone target — use `clone_repo`).
    pub intent: String,
    /// Repository Initial plan clones for planning (`owner/name`). Required.
    /// Stamped into Project intent and the seeded Initial plan so workers do
    /// not invent a remotes target. Proposed Tasks usually use the same repo.
    pub clone_repo: String,
    /// Projects are roots. Nesting a Project under another is refused.
    #[serde(default)]
    pub parent: Option<ItemId>,
    #[serde(default = "default_above_line")]
    pub above_line: bool,
    /// Standing agent instructions for this Project (defaults on create if omitted).
    #[serde(default)]
    pub project_prompt: Option<String>,
    /// Ignored — use `clone_repo`. Kept so old callers do not invent `product_repo`.
    #[serde(default)]
    #[schemars(skip)]
    pub product_repo: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CreateTaskArg {
    /// Parent Project id (`#167` is `167`). Must be a Project — nesting under
    /// a Task is refused.
    pub parent: ItemId,
    /// Short and distinct — you cannot chunk what you cannot name.
    pub title: String,
    /// One sentence of task intent. Name the clone repo here and/or in
    /// `definition_of_done` (`Clone repository: owner/name`); when omitted,
    /// the Project's `clone_repo` (from Project intent) is stamped.
    pub intent: String,
    /// Must be mechanically checkable. May carry the clone target line.
    pub definition_of_done: String,
    /// Sibling Task ids this card is blocked by.
    #[serde(default)]
    pub blocked_by: Vec<ItemId>,
    /// Optional capability tag for list_ready filtering.
    #[serde(default)]
    pub capability: Option<String>,
    #[serde(default = "default_above_line")]
    pub above_line: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct InitPlanArg {
    /// Project id (`#167` is `167`). Container must already exist.
    pub project: ItemId,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct UpdateArg {
    pub id: ItemId,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub intent: Option<String>,
    #[serde(default)]
    pub definition_of_done: Option<String>,
    /// Accepted and unused — engine lives on the sandbox profile.
    #[serde(default)]
    pub engine: Option<String>,
    /// Standing instructions — Project cards only.
    #[serde(default)]
    pub project_prompt: Option<String>,
    /// Accepted and unused — name the clone target in intent/DoD.
    #[serde(default)]
    #[schemars(skip)]
    pub repo: Option<crate::schema::RepoConfig>,
    /// Accepted and unused on Project/Task update.
    #[serde(default)]
    #[schemars(skip)]
    pub product_repo: Option<serde_json::Value>,
}

fn default_above_line() -> bool {
    true
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ChildSpec {
    pub title: String,
    /// One sentence. Not a restatement of the Project.
    pub intent: String,
    /// Must be mechanically checkable by a verifier.
    pub definition_of_done: String,
    #[serde(default)]
    pub capability: Option<String>,
    /// Stable key within the Plan (defaults to t1, t2, …).
    #[serde(default)]
    pub key: Option<String>,
    /// Plan keys this task is blocked by (sibling Tasks in the same Plan).
    #[serde(default)]
    pub blocked_by_keys: Vec<String>,
    /// Legacy: board item ids. Prefer `blocked_by_keys` for new Plans.
    #[serde(default)]
    pub blocked_by: Vec<ItemId>,
    /// Ignored — name the repository to clone in `intent` / `definition_of_done`.
    #[serde(default)]
    #[schemars(skip)]
    pub repo: Option<crate::schema::RepoConfig>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct BreakdownArg {
    /// Must be a Project.
    pub parent: ItemId,
    pub children: Vec<ChildSpec>,
    /// One-line summary of this Plan revision.
    #[serde(default)]
    pub summary: Option<String>,
    /// Plan keys to retire when this revision is approved.
    #[serde(default)]
    pub cancel_keys: Vec<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ColumnArg {
    /// One of: backlog, running, needs_you, review, done, shaping, intake, retired.
    pub column: Column,
    /// Restrict to one goal. Omit for all goals.
    #[serde(default)]
    pub goal: Option<ItemId>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchArg {
    /// Case-insensitive substring — title, intent, DoD, notes, history reasons.
    pub query: String,
    /// Restrict to one Project (and its Tasks). Omit for the whole board.
    #[serde(default)]
    pub goal: Option<ItemId>,
    /// Max hits (default 20, hard cap 50).
    #[serde(default = "default_search_limit")]
    pub limit: usize,
}

fn default_search_limit() -> usize {
    20
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListReadyArg {
    /// Capability tags this agent can serve, e.g. `["any"]` or `["any","writer"]`.
    pub capabilities: Vec<String>,
}


#[derive(Debug, Deserialize, JsonSchema)]
pub struct ClaimArg {
    pub item_id: ItemId,
    pub agent_id: String,
    #[serde(default)]
    pub model: Option<String>,
    /// Ignored — run deadline is `agents.agent_timeout_secs` on the board.
    #[serde(default = "default_lease")]
    #[allow(dead_code)]
    pub lease_secs: i64,
}
fn default_lease() -> i64 {
    1800
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct HeartbeatArg {
    pub item_id: ItemId,
    pub agent_id: String,
    /// 0.0 to 1.0.
    pub progress: f32,
    /// Ignored — legacy wire field; cost tracking was removed.
    #[serde(default)]
    #[allow(dead_code)]
    pub cost_cents: u64,
    /// Ignored — does not extend the run deadline.
    #[serde(default = "default_lease")]
    pub lease_secs: i64,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SplitArg {
    pub item_id: ItemId,
    pub agent_id: String,
    /// Two or more. If it's really one card, use `report` instead.
    pub children: Vec<ChildSpec>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct OptionSpec {
    pub label: String,
    /// What choosing this actually means, including the cost of being wrong.
    pub detail: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct EscalateArg {
    pub item_id: ItemId,
    pub agent_id: String,
    pub question: String,
    /// At least two. An open-ended question hands the whole problem back.
    pub options: Vec<OptionSpec>,
    /// Index into `options` of the one you recommend.
    pub recommended: usize,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReportArg {
    pub item_id: ItemId,
    pub agent_id: String,
    #[serde(default)]
    pub lines_added: u32,
    #[serde(default)]
    pub lines_removed: u32,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReportPullRequestEndArg {
    /// `owner/name` (`full_name`).
    pub repo: String,
    /// Branch name (GitHub JSON field `ref`).
    #[serde(rename = "ref")]
    pub git_ref: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReportPullRequestArg {
    pub item_id: ItemId,
    pub agent_id: String,
    /// PR HTML URL.
    pub url: String,
    #[serde(default)]
    pub base: Option<ReportPullRequestEndArg>,
    #[serde(default)]
    pub head: Option<ReportPullRequestEndArg>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AgentItemArg {
    pub item_id: ItemId,
    pub agent_id: String,
}

// ------------------------------------------------------------------ returns

#[derive(Debug, Serialize, JsonSchema)]
pub struct Ack {
    pub ok: bool,
    pub item: ItemId,
    pub state: String,
    pub note: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct GoalLine {
    pub goal: ItemId,
    pub title: String,
    pub health: String,
    pub progress: String,
    pub needs_you: usize,
    /// Project auto mode (play) — claimable Backlog queues itself.
    pub auto_dispatch: bool,
    /// One chunked line per column — smaller than a list *and* answers the
    /// column's question.
    pub columns: Vec<String>,
    pub latest: Option<String>,
    /// Mid-project scope cuts still under this live goal (newest first).
    /// Empty when nothing was retired, or when the Project itself is archived.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recent_retired: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema, PartialEq, Clone)]
pub struct SearchOut {
    pub items: Vec<crate::store::SearchHit>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct SnapshotOut {
    pub goals: Vec<GoalLine>,
    pub hint: String,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema, PartialEq, Clone)]
pub struct CardLine {
    pub id: ItemId,
    pub title: String,
    pub state: String,
    pub detail: String,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema, PartialEq, Clone)]
pub struct ListColumnOut {
    pub items: Vec<CardLine>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema, PartialEq, Clone)]
pub struct BreakdownOut {
    pub items: Vec<ItemId>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema, PartialEq, Clone)]
pub struct ApprovePlanOut {
    pub items: Vec<ItemId>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema, PartialEq, Clone)]
pub struct CutScopeOut {
    pub items: Vec<ItemId>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema, PartialEq, Clone)]
pub struct UnarchiveScopeOut {
    pub items: Vec<ItemId>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema, PartialEq, Clone)]
pub struct ListReadyOut {
    pub items: Vec<CardLine>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema, PartialEq, Clone)]
pub struct SplitOut {
    pub items: Vec<ItemId>,
}


#[derive(Debug, Serialize, Deserialize, JsonSchema, PartialEq, Clone)]
pub struct HealEpicsOut {
    pub healed_count: usize,
    pub note: String,
}

// ---------------------------------------------------------------- the server

#[derive(Clone)]
pub struct Operator {
    board: SharedBoard,
    seat: McpSeat,
    /// Used by `#[tool_handler(router = self.tool_router)]` for list/call.
    #[allow(dead_code)]
    tool_router: rmcp::handler::server::tool::ToolRouter<Operator>,
}

#[tool_router]
impl Operator {
    /// Operator seat — operator tools only. This is what `/mcp` mounts.
    pub fn new(board: SharedBoard) -> Self {
        Self::with_seat(board, McpSeat::Operator)
    }

    /// Host seat — operator tools plus worker verbs (supervisor/host tooling).
    #[allow(dead_code)] // used by unit tests; live supervisor calls Board directly
    pub fn host(board: SharedBoard) -> Self {
        Self::with_seat(board, McpSeat::Host)
    }

    pub fn with_seat(board: SharedBoard, seat: McpSeat) -> Self {
        let mut tool_router = Self::tool_router();
        if seat == McpSeat::Operator {
            for name in WORKER_VERB_TOOLS {
                tool_router.disable_route(*name);
            }
        }
        Self {
            board,
            seat,
            tool_router,
        }
    }

    #[allow(dead_code)] // used by unit tests asserting seat identity
    pub fn seat(&self) -> McpSeat {
        self.seat
    }

    fn deny_worker(&self, verb: &str) -> Result<(), ErrorData> {
        if self.seat == McpSeat::Operator {
            return Err(bad(format!(
                "{verb} is a worker verb; this seat is operator tools only"
            )));
        }
        Ok(())
    }

    fn ack(&self, id: ItemId, note: impl Into<String>) -> Out<Ack> {
        let state = self
            .board
            .get(id)
            .map(|i| format!("{:?}", i.state))
            .unwrap_or_else(|| "gone".into());
        Ok(ToolJson(Ack { ok: true, item: id, state, note: note.into() }))
    }

    // ============================================================== operator

    #[tool(
        name = "board_snapshot",
        description = "Start here. One chunked line per column per goal — is anything on fire, \
                       will it ship, what is left. Call this at the start of any board \
                       conversation, and again after you change something. It is deliberately \
                       not a list of cards: use list_column when you need the actual items."
    )]
    fn board_snapshot(&self) -> Out<SnapshotOut> {
        let snap = self.board.snapshot();
        let goals = snap
            .goals
            .iter()
            .map(|g| GoalLine {
                goal: g.id,
                title: g.title.clone(),
                health: if g.needs_you > 0 {
                    format!("{} blocked on you", g.needs_you)
                } else if g.agents_live > 0 {
                    format!("{} agents working", g.agents_live)
                } else if g.auto_dispatch {
                    "auto".into()
                } else {
                    "idle".into()
                },
                progress: format!(
                    "{}/{} leaves ({:.0}%)",
                    g.leaves_done,
                    g.leaves_total,
                    g.progress * 100.0
                ),
                needs_you: g.needs_you,
                auto_dispatch: g.auto_dispatch,
                columns: g
                    .columns
                    .iter()
                    .filter(|c| c.summary.count > 0)
                    .map(|c| format!("{:?}: {}", c.column, c.summary.text))
                    .collect(),
                latest: g.story.last().map(|s| s.text.clone()),
                recent_retired: g
                    .recent_retired
                    .iter()
                    .map(|r| match &r.reason {
                        Some(reason) if !reason.is_empty() => {
                            format!("#{} {} — {}", r.id, r.title, reason)
                        }
                        _ => format!("#{} {}", r.id, r.title),
                    })
                    .collect(),
            })
            .collect();

        Ok(ToolJson(SnapshotOut {
            goals,
            hint: "Anything in needs_you is stopping an agent and costing throughput. Review can \
                   wait until this evening. recent_retired on a live goal is a mid-project cut — \
                   use list_column(retired) or item_detail for the full card."
                .into(),
        }))
    }

    #[tool(
        name = "board_digest",
        description = "What the human should read on their phone: merged count, the \
                       specific questions blocking agents, and whether anything is stalled. Call \
                       this when asked 'what's the status', 'anything need me', or at the start \
                       of a session after time away."
    )]
    fn board_digest(&self) -> Out<crate::store::Digest> {
        Ok(ToolJson(self.board.digest()))
    }

    #[tool(
        name = "list_column",
        description = "The actual cards in one column, once the snapshot has told you which \
                       column matters. Call this before acting on individual items — never guess \
                       an item id."
    )]
    fn list_column(&self, Parameters(a): Parameters<ColumnArg>) -> Out<ListColumnOut> {
        let snap = self.board.snapshot();
        let now = snap.server_time;
        let mut items: Vec<&crate::model::WorkItem> = snap
            .items
            .iter()
            .filter(|i| i.state.column() == a.column)
            .filter(|i| match a.goal {
                None => true,
                Some(g) => self.board.goal_for(i.id) == g,
            })
            .collect();

        if a.column == Column::Backlog {
            items.sort_by(|a, b| {
                let a_blocked = a.blockers.iter().any(|blk| !blk.state.is_terminal())
                    || (a.blockers.is_empty() && !a.blocked_by.is_empty());
                let b_blocked = b.blockers.iter().any(|blk| !blk.state.is_terminal())
                    || (b.blockers.is_empty() && !b.blocked_by.is_empty());
                if a_blocked != b_blocked {
                    return a_blocked.cmp(&b_blocked);
                }
                a.entered_state_at.cmp(&b.entered_state_at)
            });
        }

        let rows = items
            .into_iter()
            .map(|i| CardLine {
                id: i.id,
                title: i.title.clone(),
                state: format!("{:?}", i.state),
                detail: match i.state {
                    State::NeedsHuman => i
                        .escalation
                        .as_ref()
                        .map(|e| format!("{} (blocked {})", e.question, crate::model::humanize(chrono::Duration::seconds(e.blocked_secs(now)))))
                        .unwrap_or_default(),
                    State::Running | State::Claimed => format!(
                        "{:.0}% · agent {}",
                        i.progress * 100.0,
                        i.lease.as_ref().map(|l| l.agent_id.as_str()).unwrap_or("?")
                    ),
                    State::Review => format!("+{} −{} · gates passed", i.diff_added, i.diff_removed),
                    State::Backlog if !i.blocked_by.is_empty() => {
                        if !i.blockers.is_empty() {
                            let summaries: Vec<String> = i
                                .blockers
                                .iter()
                                .map(|b| format!("#{} \"{}\" ({:?})", b.id, b.title, b.state))
                                .collect();
                            format!("blocked by {}", summaries.join(", "))
                        } else {
                            format!("blocked by {:?}", i.blocked_by)
                        }
                    }
                    _ => i.intent.clone(),
                },
            })
            .collect();
        Ok(ToolJson(ListColumnOut { items: rows }))
    }

    #[tool(
        name = "item_detail",
        description = "Everything about one card: ancestry, Plan on the Project, project_prompt, \
                       cost, history and any pending question. On a Project, `children` is \
                       [{id,title,state,last_reason}] (not bare ids) so mid-project retirements \
                       are visible. Call this before answering an escalation or approving a \
                       review — the Plan says whether the work serves the goal."
    )]
    fn item_detail(&self, Parameters(a): Parameters<IdArg>) -> Out<serde_json::Value> {
        let item = self.board.get(a.id).ok_or_else(|| bad(format!("no work item #{}", a.id)))?;
        Ok(ToolJson(serde_json::json!({
            "item": item,
            "ancestry": self.board.ancestry(a.id),
            "children": self.board.child_summaries(a.id),
        })))
    }

    #[tool(
        name = "search_items",
        description = "Find cards by keyword across title, intent, definition_of_done, notes, \
                       and history reasons. Use when you know a phrase ('sandbox image', \
                       'OpenCode') but not the id, or when board_snapshot's retired Project \
                       lines are the wrong place to look. Optional goal scopes to one Project."
    )]
    fn search_items(&self, Parameters(a): Parameters<SearchArg>) -> Out<SearchOut> {
        let q = a.query.trim();
        if q.is_empty() {
            return Err(bad("query must not be empty"));
        }
        Ok(ToolJson(SearchOut {
            items: self.board.search_items(q, a.goal, a.limit),
        }))
    }

    #[tool(
        name = "create_project",
        description = "Create a Project container. Requires clone_repo (`owner/name`) — the \
                       repository Initial plan clones into for planning (and the default for \
                       proposed Tasks). Auto-seeds one Backlog Initial plan Task with that \
                       clone target stamped in. Optional project_prompt is Project-only standing \
                       extras; board-wide policy is Settings → Agent runtime standing prompt. \
                       Dispatch the Initial plan when ready; the planner writes plan.json (each \
                       proposed Task names its clone target in intent/DoD, usually the same \
                       clone_repo)."
    )]
    fn create_project(&self, Parameters(a): Parameters<CreateProjectArg>) -> Out<Ack> {
        if a.parent.is_some() {
            return Err(bad("Projects are roots; omit parent"));
        }
        let _ = a.product_repo;
        let item = self
            .board
            .create_project(
                a.title,
                a.intent,
                &a.clone_repo,
                a.above_line,
                a.project_prompt,
            )
            .map_err(bad)?;
        let _ = self.board.transition(item.id, State::Shaping, "operator", None);
        self.ack(
            item.id,
            "Project created in shaping with auto-seeded Initial plan — dispatch that Task to plan",
        )
    }

    #[tool(
        name = "create_task",
        description = "Create a flat Task under an existing Project, landing in Backlog \
                       (dispatchable). Parent must be a Project id — nesting under a Task \
                       is refused. Each Task must name its clone repository (`owner/name`) \
                       in intent and/or definition_of_done; when omitted, the Project's \
                       clone_repo (from Project intent) is stamped as the default. Optional \
                       blocked_by ItemIds and capability. Same Board path as POST /api/items \
                       with parent."
    )]
    fn create_task(&self, Parameters(a): Parameters<CreateTaskArg>) -> Out<Ack> {
        let item = self
            .board
            .create_task(
                a.parent,
                a.title,
                a.intent,
                a.definition_of_done,
                a.blocked_by,
                a.capability,
                a.above_line,
            )
            .map_err(bad)?;
        self.ack(
            item.id,
            "Task created in Backlog under Project — dispatch when ready",
        )
    }

    #[tool(
        name = "init_plan",
        description = "Ensure a Project has an Initial plan Task (usually already auto-seeded \
                       by create_project). Idempotent. Project intent should already name \
                       clone_repo (`Clone repository: owner/name …`). Each proposed task \
                       must name the repository to clone in its intent/DoD. Dispatch the \
                       Initial plan to write plan.json."
    )]
    fn init_plan(&self, Parameters(a): Parameters<InitPlanArg>) -> Out<Ack> {
        let seed = self.board.init_plan(a.project).map_err(bad)?;
        self.ack(
            seed.id,
            "Initial plan Task ready in Backlog — dispatch to write plan.json",
        )
    }

    #[tool(
        name = "propose_breakdown",
        description = "Write a Task proposal on the Project's Initial plan card (flat Tasks + \
                       deps by plan key). Does not create board cards — Approve on that card \
                       (or approve_plan) materializes them. Every task needs a definition of \
                       done a verifier can mechanically check, and must name which repository \
                       to clone (`owner/name`) in intent and/or DoD. Parent may be the Project or \
                       the Initial plan Task id."
    )]
    fn propose_breakdown(&self, Parameters(a): Parameters<BreakdownArg>) -> Out<BreakdownOut> {
        use crate::model::PlanTaskSpec;

        let parent = self
            .board
            .get(a.parent)
            .ok_or_else(|| bad(format!("no work item #{}", a.parent)))?;
        let is_project = parent.is_project();
        let is_initial = parent.is_initial_plan_task();
        if !is_project && !is_initial {
            return Err(bad("breakdown parent must be a Project or Initial plan Task"));
        }
        if a.children.is_empty() {
            return Err(bad("a breakdown needs at least one task"));
        }

        // Map legacy blocked_by ItemIds → keys from an existing proposal (if any).
        let seed_id = self
            .board
            .resolve_initial_plan_id(a.parent)
            .map_err(bad)?;
        let id_to_key: std::collections::BTreeMap<ItemId, String> = self
            .board
            .get(seed_id)
            .and_then(|s| s.proposal)
            .map(|p| {
                p.tasks
                    .iter()
                    .filter_map(|t| t.item_id.map(|id| (id, t.key.clone())))
                    .collect()
            })
            .unwrap_or_default();

        let mut specs = Vec::new();
        for (idx, c) in a.children.into_iter().enumerate() {
            let key = c
                .key
                .filter(|k| !k.trim().is_empty())
                .unwrap_or_else(|| format!("t{}", idx + 1));
            let mut blocked_by_keys = c.blocked_by_keys;
            for bid in c.blocked_by {
                if let Some(k) = id_to_key.get(&bid) {
                    if !blocked_by_keys.contains(k) {
                        blocked_by_keys.push(k.clone());
                    }
                } else {
                    let k = format!("id-{bid}");
                    if !blocked_by_keys.contains(&k) {
                        blocked_by_keys.push(k);
                    }
                }
            }
            let _ = c.repo;
            specs.push(PlanTaskSpec {
                key,
                title: c.title,
                intent: c.intent,
                definition_of_done: c.definition_of_done,
                blocked_by_keys,
                capability: c.capability,
                repo: None,
                item_id: None,
            });
        }
        let summary = a.summary.unwrap_or_else(|| {
            format!("{} tasks proposed", specs.len())
        });
        let proposal = self
            .board
            .propose_plan(a.parent, summary, specs, a.cancel_keys)
            .map_err(bad)?;
        let linked: Vec<ItemId> = proposal.tasks.iter().filter_map(|t| t.item_id).collect();
        Ok(ToolJson(BreakdownOut { items: linked }))
    }

    #[tool(
        name = "approve_plan",
        description = "Approve the Initial plan proposal: materialize flat Tasks + deps to \
                       Backlog and finish the Initial plan card. Pass the Project id or the \
                       Initial plan Task id. Same gate as approve_review on Initial plan. \
                       Never moves the Project itself to Backlog. Does not start runs — \
                       dispatch each Task explicitly."
    )]
    fn approve_plan(&self, Parameters(a): Parameters<IdArg>) -> Out<ApprovePlanOut> {
        let published = self.board.approve_plan(a.id).map_err(bad)?;
        Ok(ToolJson(ApprovePlanOut { items: published }))
    }

    #[tool(
        name = "answer_escalation",
        description = "Resolve a card sitting in Needs You. Do this first, before anything in \
                       Review — a blocked agent is burning throughput every minute, while \
                       finished work is safe and can wait. Read item_detail first; the answer is \
                       recorded as standing context for whoever picks the card up."
    )]
    fn answer_escalation(&self, Parameters(a): Parameters<AnswerArg>) -> Out<Ack> {
        self.board.answer_escalation(a.id, a.choice).map_err(bad)?;
        self.ack(a.id, "unblocked and requeued")
    }

    #[tool(
        name = "steer",
        description = "Inject a note into a running agent's next turn. Free — no restart, no \
                       context loss. Reach for this instead of halt whenever the agent is only \
                       slightly off course."
    )]
    fn steer(&self, Parameters(a): Parameters<TextArg>) -> Out<Ack> {
        self.board.steer(a.id, a.text).map_err(bad)?;
        self.ack(a.id, "note will reach the agent on its next turn")
    }

    #[tool(
        name = "update",
        description = "Edit fields on a card. When changing intent/DoD on a Task, name the \
                       repository to clone explicitly (`owner/name`) if the worker will need \
                       git. Agent engine is on the sandbox profile, not via `engine` on the \
                       card. `project_prompt` only applies to Project cards."
    )]
    fn update(&self, Parameters(a): Parameters<UpdateArg>) -> Out<Ack> {
        if a.title.is_none()
            && a.intent.is_none()
            && a.definition_of_done.is_none()
            && a.project_prompt.is_none()
        {
            if a.engine.is_some() {
                return Err(bad(
                    "engine is set on the sandbox profile (Settings → OpenShell → Profiles), not the card",
                ));
            }
            return Err(bad("update needs at least one field"));
        }
        let _ = a.product_repo;
        let _ = a.repo;
        let _ = a.engine;
        let _item = self
            .board
            .update_item(
                a.id,
                a.title,
                a.intent,
                a.definition_of_done,
                None,
                a.project_prompt,
            )
            .map_err(bad)?;
        self.ack(a.id, "updated")
    }

    #[tool(
        name = "dispatch",
        description = "Queue a Backlog card for the supervisor to claim and start a sandbox run. \
                       Normally Backlog is inert until this (or UI Start); Projects with auto \
                       mode on queue themselves. Requires unblocked and unparked. Does not start \
                       immediately if max_concurrent or budget is saturated; the supervisor \
                       drains the queue."
    )]
    fn dispatch(&self, Parameters(a): Parameters<IdArg>) -> Out<Ack> {
        self.board.enqueue_dispatch(a.id).map_err(bad)?;
        self.ack(a.id, "queued for dispatch")
    }

    #[tool(
        name = "set_auto_dispatch",
        description = "Play/pause Project auto mode. When enabled, claimable Backlog leaves under \
                       that Project are queued automatically each supervisor tick. Pause clears \
                       the queue but does not halt in-flight runs. Project cards only."
    )]
    fn set_auto_dispatch(&self, Parameters(a): Parameters<AutoDispatchArg>) -> Out<Ack> {
        self.board
            .set_auto_dispatch(a.id, a.enabled)
            .map_err(bad)?;
        self.ack(
            a.id,
            if a.enabled {
                "auto mode on"
            } else {
                "auto mode off"
            },
        )
    }

    #[tool(
        name = "park",
        description = "Stop the agent and return the card to Backlog, keep the sandbox and agy \
                       conversation, and hold the card until unpark. Prefer this when a run is \
                       wedged. Optional reason becomes a binding note on resume. Unpark queues \
                       the supervisor to resume (no separate dispatch)."
    )]
    fn park(&self, Parameters(a): Parameters<ReasonArg>) -> Out<Ack> {
        self.board.park(a.id, a.reason).map_err(bad)?;
        self.ack(a.id, "agent parked; unpark to resume")
    }

    #[tool(
        name = "unpark",
        description = "Clear a park hold and queue the card for the supervisor (same as Start). \
                       If a conversation id is still on the card, the next claim resumes that \
                       agy session."
    )]
    fn unpark(&self, Parameters(a): Parameters<IdArg>) -> Out<Ack> {
        self.board.unpark(a.id).map_err(bad)?;
        self.ack(a.id, "unparked and queued for resume")
    }

    #[tool(
        name = "halt",
        description = "Kill the agent, discard the LLM session, delete the sandbox, and return \
                       the card to Backlog. Does not auto-reclaim — dispatch again to restart \
                       clean. Prefer park when you want to keep the sandbox and resume the same \
                       conversation; prefer steer for a soft note that can wait until the next turn."
    )]
    fn halt(&self, Parameters(a): Parameters<ReasonArg>) -> Out<Ack> {
        self.board.halt(a.id, a.reason).map_err(bad)?;
        self.ack(a.id, "agent released; session and sandbox discarded; dispatch to restart")
    }

    #[tool(
        name = "cut_scope",
        description = "Retire a card and its whole subtree. Retired, not deleted — it stays \
                       visible and greyed, because 'we chose not to' is a fact you will need \
                       later. Confirm with the human before calling this."
    )]
    fn cut_scope(&self, Parameters(a): Parameters<ReasonArg>) -> Out<CutScopeOut> {
        let ids = self.board.cut_scope(a.id, a.reason).map_err(bad)?;
        Ok(ToolJson(CutScopeOut { items: ids }))
    }

    #[tool(
        name = "unarchive_scope",
        description = "Restore a retired Project or subtree from history (inverse of cut_scope / \
                       Archive). In-flight priors remapped to Backlog — never Claimed/Running. \
                       Confirm with the human before calling this."
    )]
    fn unarchive_scope(&self, Parameters(a): Parameters<ReasonArg>) -> Out<UnarchiveScopeOut> {
        let ids = self.board.unarchive_scope(a.id, a.reason).map_err(bad)?;
        Ok(ToolJson(UnarchiveScopeOut { items: ids }))
    }

    #[tool(
        name = "approve_review",
        description = "Approve a Review card and move it to Done. Initial plan / split proposals \
                       with a Task list materialize sibling Tasks on that Done. Merge webhook \
                       remains a backup if Approve never ran. Sort Review by blast radius and novelty."
    )]
    fn approve_review(&self, Parameters(a): Parameters<IdArg>) -> Out<Ack> {
        let before: std::collections::HashSet<_> = self
            .board
            .get(a.id)
            .and_then(|i| i.parent)
            .map(|p| self.board.children_of(p))
            .unwrap_or_default()
            .into_iter()
            .collect();
        let item = self.board.approve_review(a.id).map_err(bad)?;
        if let Some(parent) = item.parent {
            let mut new_ids = Vec::new();
            for cid in self.board.children_of(parent) {
                if !before.contains(&cid) {
                    new_ids.push(cid);
                }
            }
        }
        let unblocked = self.board.newly_unblocked_siblings(a.id);
        let note = if unblocked.len() == 1 {
            format!("approved — dispatch #{} next", unblocked[0].id)
        } else if unblocked.len() > 1 {
            let ids: Vec<_> = unblocked.iter().map(|u| format!("#{}", u.id)).collect();
            format!("approved — unblocked: {}", ids.join(", "))
        } else {
            "approved".to_string()
        };
        self.ack(a.id, &note)
    }

    #[tool(
        name = "request_changes",
        description = "Send a reviewed card back to Backlog with a note. The note is attached to \
                       the card, so the next run (after dispatch) sees why. Does not auto-start."
    )]
    fn request_changes(&self, Parameters(a): Parameters<TextArg>) -> Out<Ack> {
        self.board.request_changes(a.id, a.text).map_err(bad)?;
        self.ack(a.id, "returned to Backlog with your note — dispatch to restart")
    }

    // =============================================================== worker

    #[tool(
        name = "heal_epics",
        description = "One-shot heal: mark Projects Done when all child Tasks are Done or Retired."
    )]
    async fn heal_epics(&self) -> Out<HealEpicsOut> {
        let healed_count = self.board.heal_completed_epics().await;
        Ok(ToolJson(HealEpicsOut {
            healed_count,
            note: format!("healed {healed_count} completed epic(s)"),
        }))
    }

    #[tool(
        name = "list_ready",
        description = "WORKER VERB / operator alias. Lists Backlog leaves filtered by capabilities. \
                       Not a start queue — operator must dispatch before the supervisor claims."
    )]
    fn list_ready(&self, Parameters(a): Parameters<ListReadyArg>) -> Out<ListReadyOut> {
        self.deny_worker("list_ready")?;
        let rows = self
            .board
            .list_ready(&a.capabilities)
            .into_iter()
            .map(|i| CardLine {
                id: i.id,
                title: i.title.clone(),
                state: "Backlog".into(),
                detail: i.intent.clone(),
            })
            .collect();
        Ok(ToolJson(ListReadyOut { items: rows }))
    }

    #[tool(
        name = "claim",
        description = "WORKER VERB. Take a Backlog card (supervisor path after dispatch). \
                       Returns the full intent chain — read it before you start. The run \
                       ends at agent_timeout_secs; heartbeats do not extend that deadline."
    )]
    fn claim(&self, Parameters(a): Parameters<ClaimArg>) -> Out<crate::store::ClaimGrant> {
        self.deny_worker("claim")?;
        let timeout = self.board.effective_agents().agent_timeout_secs as i64;
        let grant = self
            .board
            .claim(a.item_id, &a.agent_id, a.model, timeout)
            .map_err(|e| bad(e.to_string()))?;
        Ok(ToolJson(grant))
    }

    #[tool(
        name = "heartbeat",
        description = "WORKER VERB. Report progress. Does not extend the run deadline — \
                       that was fixed at claim."
    )]
    fn heartbeat(&self, Parameters(a): Parameters<HeartbeatArg>) -> Out<Ack> {
        self.deny_worker("heartbeat")?;
        let _ = a.cost_cents; // legacy clients may still send it
        self.board
            .heartbeat(a.item_id, &a.agent_id, a.progress, a.lease_secs)
            .map_err(|e| bad(e.to_string()))?;
        self.ack(a.item_id, "progress recorded")
    }

    #[tool(
        name = "split",
        description = "WORKER VERB. The work is bigger than this card: propose sibling Tasks \
                       (Review). Human Approve creates them under the Project — nothing is \
                       created until then. Needs two or more children; if it is really one card, \
                       just report. Mutually exclusive with opening a PR."
    )]
    fn split(&self, Parameters(a): Parameters<SplitArg>) -> Out<SplitOut> {
        self.deny_worker("split")?;
        let children = a
            .children
            .into_iter()
            .map(|c| {
                let mut spec =
                    crate::model::SplitChildSpec::new(c.title, c.intent, c.definition_of_done);
                spec.key = c.key;
                spec.blocked_by_keys = c.blocked_by_keys;
                spec.repo = c.repo;
                spec
            })
            .collect();
        let card = self
            .board
            .propose_split(a.item_id, &a.agent_id, children, 5)
            .map_err(bad)?;
        Ok(ToolJson(SplitOut {
            // Proposal card id — siblings do not exist until Approve.
            items: vec![card.id],
        }))
    }

    #[tool(
        name = "escalate",
        description = "WORKER VERB. You have hit a real decision. You must supply at least two \
                       concrete options and a recommendation — an open-ended 'what should I do?' \
                       transfers the whole problem back to the human, and turns a one-tap \
                       decision into a five-minute think. Escalate only when the contract \
                       genuinely does not settle the question; a high escalation rate is a \
                       quality signal, not just a workflow event."
    )]
    fn escalate(&self, Parameters(a): Parameters<EscalateArg>) -> Out<Ack> {
        self.deny_worker("escalate")?;
        let options = a
            .options
            .into_iter()
            .map(|o| EscalationOption { label: o.label, detail: o.detail })
            .collect();
        self.board
            .escalate(a.item_id, &a.agent_id, a.question, options, a.recommended)
            .map_err(bad)?;
        self.ack(a.item_id, "escalated; a human has been asked")
    }

    #[tool(
        name = "report",
        description = "WORKER VERB. You believe the definition of done is met. Hands the card to \
                       Review — CI on the PR is the mechanical gate."
    )]
    fn report(&self, Parameters(a): Parameters<ReportArg>) -> Out<Ack> {
        self.deny_worker("report")?;
        self.board
            .report(
                a.item_id,
                &a.agent_id,
                a.lines_added,
                a.lines_removed,
                vec!["lint".into(), "types".into(), "tests".into()],
            )
            .map_err(|e| bad(e.to_string()))?;
        self.ack(a.item_id, "handed to the verifier")
    }

    #[tool(
        name = "report_pull_request",
        description = "WORKER VERB. Record a PR this running card just opened. Appends to the \
                       card's pull_requests list without replacing earlier PRs or leaving Running. \
                       Resolves GitHub App installation from the repo-access cache and attaches \
                       GH_TOKEN for that installation to the live sandbox. Uncovered owner/name \
                       parks Needs You (Settings → Repo access). Nothing is pre-selected by a human. \
                       Final `report` still hands the card to Review."
    )]
    async fn report_pull_request(&self, Parameters(a): Parameters<ReportPullRequestArg>) -> Out<Ack> {
        self.deny_worker("report_pull_request")?;
        let mut pr = crate::model::PullRequest::from_url(a.url);
        if let Some(b) = a.base {
            pr.base = Some(crate::model::PullRequestEnd::new(b.repo, b.git_ref));
        }
        if let Some(h) = a.head {
            pr.head = Some(crate::model::PullRequestEnd::new(h.repo, h.git_ref));
        }
        let item = self
            .board
            .report_pull_request(a.item_id, pr.clone())
            .map_err(bad)?;
        if let Some(repo) = pr.push_owner_repo() {
            match crate::github_app::ensure_push_token(
                &self.board,
                a.item_id,
                &a.agent_id,
                item.environment.as_deref(),
                Some(&repo),
            )
            .await
            {
                Ok(crate::github_app::EnsurePushToken::Parked) => {
                    return self.ack(
                        a.item_id,
                        format!(
                            "recorded PR; parked Needs You — App not installed on {repo} \
(Settings → Repo access)"
                        ),
                    );
                }
                Ok(_) => {}
                Err(e) => return Err(bad(e.to_string())),
            }
        }
        let n = item.pull_requests.len();
        self.ack(a.item_id, format!("recorded PR ({n} on card)"))
    }

    #[tool(
        name = "release",
        description = "WORKER VERB. Graceful surrender — give the card back to Backlog without \
                       waiting for your lease to expire. Operator must dispatch again to restart."
    )]
    fn release(&self, Parameters(a): Parameters<AgentItemArg>) -> Out<Ack> {
        self.deny_worker("release")?;
        self.board
            .release(a.item_id, &a.agent_id)
            .map_err(|e| bad(e.to_string()))?;
        self.ack(a.item_id, "released to Backlog")
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for Operator {
    fn get_info(&self) -> rmcp::model::ServerInfo {
        rmcp::model::ServerInfo::new(
            rmcp::model::ServerCapabilities::builder().enable_tools().build(),
        )
        .with_server_info({
            let mut me = rmcp::model::Implementation::default();
            me.name = "sandboard".into();
            me.title = Some("sandboard — agent orchestrator".into());
            me.version = env!("CARGO_PKG_VERSION").into();
            me
        })
        .with_instructions(match self.seat {
            McpSeat::Operator => {
                "sandboard — an agent orchestration board. You are the operator seat: the \
                 human's liaison over operator tools only (no claim/heartbeat/report/report_pull_request/split/escalate/\
                 release/list_ready — those are worker verbs on the host/supervisor path).\n\n\
                 Start with board_snapshot. Live goals expose recent_retired for mid-project \
                 cuts (retired leaves are not in column rollups). Use search_items when you \
                 know a phrase but not an id. item_detail on a Project returns children with \
                 state and last_reason.\n\n\
                 Triage in this order, because urgency differs:\n\
                 1. Needs You — an agent is stopped and burning nothing while it waits. Every \
                    minute costs throughput. Resolve these first.\n\
                 2. Review — finished and safe. It can wait until this evening. Sort by blast \
                    radius and novelty, not arrival time.\n\
                 3. Everything else waits for a digest.\n\n\
                 Interrupt the human for three things only: irreversible actions, \
                 an ambiguity blocking several items, and repeated failure on the same card. \
                 Prefer escalating ambiguous irreversibles; do not widen merge semantics — \
                 approving merges stays human. Otherwise summarise and let them walk away.\n\n\
                 Backlog cards do not auto-start unless the Project's auto mode is on \
                 (swimlane play/pause or set_auto_dispatch). Otherwise use dispatch (or Start). \
                 Park/halt/lease expiry/request_changes return to Backlog without reclaim — \
                 dispatch again (or wait for auto). Prefer park over halt when a run is wedged — \
                 park keeps the sandbox and agy session; halt deletes the sandbox; unpark queues resume. Prefer \
                 steer for a soft note that can wait (steer alone does not inject \
                 mid-turn). MainAdvanced does not park live runs; Review catch-up observes \
                 GitHub mergeable and bounces only on CONFLICTING.\n\n\
                 Configuration layers: process boot and board Settings (Policies, sandbox \
                 specs, agent runtime including standing prompt, Forge) are operator setup; \
                 Project fields (clone_repo, optional sandbox override) seed the Initial plan; \
                 project_prompt is Project-only standing extras; per-card intent/DoD names clone \
                 targets and card-specific gates. Boot, Settings, and Project fields do not \
                 belong in project_prompt — put board-wide escalation and quality gates in \
                 Settings → Agent runtime standing prompt; Project-specific rules via update on \
                 the Project. Name test/lint commands explicitly; sandboard does not assume cargo \
                 or any toolchain unless the board standing prompt, project_prompt, or a card's \
                 DoD names it. Task inputs are the Plan. Initial plan and impl splits write a \
                 proposal on the card → Review; Approve creates sibling Tasks. Read \
                 item_detail's proposal/Plan before approving; a card that passes its gates can \
                 still be building the wrong thing, because coherence is not a property of any \
                 single card."
            }
            McpSeat::Host => {
                 "sandboard — host MCP seat: operator tools plus worker verbs \
                 (list_ready/claim/heartbeat/split/escalate/report/report_pull_request/release). \
                 Card lifecycle mutations still go through Board; this seat is for \
                 supervisor/host tooling, not the operator chatbot."
            }
        })
    }
}

/// Hosts rmcp's DNS-rebinding guard will accept on `/mcp`.
///
/// Loopback + docker defaults, plus the authority from `SANDBOARD_PUBLIC_URL` /
/// `~/.config/sandboard/public_url` (Tailscale Serve / reverse proxy) and any
/// comma-separated `SANDBOARD_MCP_ALLOWED_HOSTS`.
fn mcp_allowed_hosts() -> Vec<String> {
    let mut hosts = vec![
        "localhost".into(),
        "127.0.0.1".into(),
        "::1".into(),
        "host.docker.internal".into(),
        "host.docker.internal:8080".into(),
    ];
    if let Some(auth) = configured_public_url().as_deref().and_then(authority_from_public_url) {
        // Entry without port matches any port; with port matches exactly.
        if let Some((host, _)) = split_host_port(&auth) {
            if !hosts.iter().any(|h| h == host) {
                hosts.push(host.to_string());
            }
        }
        if !hosts.iter().any(|h| h == &auth) {
            hosts.push(auth);
        }
    }
    if let Ok(extra) = std::env::var("SANDBOARD_MCP_ALLOWED_HOSTS") {
        for part in extra.split(',') {
            let h = part.trim();
            if h.is_empty() || hosts.iter().any(|e| e == h) {
                continue;
            }
            hosts.push(h.to_string());
        }
    }
    hosts
}

fn configured_public_url() -> Option<String> {
    if let Some(base) = std::env::var("SANDBOARD_PUBLIC_URL")
        .ok()
        .map(|s| s.trim().trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty())
    {
        return Some(base);
    }
    let path = dirs_public_url_path()?;
    let raw = std::fs::read_to_string(path).ok()?;
    let base = raw
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with('#'))?
        .trim_end_matches('/')
        .to_string();
    (!base.is_empty()).then_some(base)
}

fn dirs_public_url_path() -> Option<std::path::PathBuf> {
    let home = std::env::var_os("HOME").filter(|h| !h.is_empty())?;
    Some(std::path::PathBuf::from(home).join(".config/sandboard/public_url"))
}

fn authority_from_public_url(url: &str) -> Option<String> {
    let url = url.trim().trim_end_matches('/');
    if url.is_empty() {
        return None;
    }
    let uri: axum::http::Uri = url.parse().ok()?;
    uri.authority().map(|a| a.as_str().to_string())
}

fn split_host_port(authority: &str) -> Option<(&str, Option<&str>)> {
    if authority.starts_with('[') {
        let end = authority.find(']')?;
        let host = &authority[..=end];
        let port = authority.get(end + 1..).and_then(|rest| rest.strip_prefix(':'));
        return Some((host, port.filter(|p| !p.is_empty())));
    }
    match authority.rsplit_once(':') {
        Some((host, port)) if !host.is_empty() && port.chars().all(|c| c.is_ascii_digit()) => {
            Some((host, Some(port)))
        }
        _ => Some((authority, None)),
    }
}

/// Mounted on the same axum router, same port, same state as the human face.
///
/// `/mcp` is the **operator seat** (operator tools only). Stateless on purpose:
/// tools are request/response over `SharedBoard`; an in-memory `Mcp-Session-Id`
/// only made Cursor brittle across `cargo run` restarts ("Session not found")
/// without buying us server→client streams.
pub fn service(board: SharedBoard) -> StreamableHttpService<Operator, NeverSessionManager> {
    // rmcp defaults to localhost/127.0.0.1/::1 only (DNS-rebinding guard).
    // Cockpit's shipped sandboard MCP is stdio now (no HTTP hop at all); this
    // allowlist only matters for other HTTP MCP clients reaching /mcp
    // directly (host Cursor, worker sandboxes on host.docker.internal,
    // remote Cursor via Tailscale Serve — set SANDBOARD_PUBLIC_URL).
    let mcp_http = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_allowed_hosts(mcp_allowed_hosts());
    StreamableHttpService::new(
        move || Ok(Operator::new(board.clone())),
        Arc::new(NeverSessionManager::default()),
        mcp_http,
    )
}

async fn normalize_mcp_request(req: Request, next: Next) -> Response {
    let (mut parts, body) = req.into_parts();
    let method = parts.method.clone();
    let query_string = parts.uri.query().map(|q| q.to_owned());

    // Copy session id from query parameters if missing in headers.
    if let Some(query) = query_string {
        for pair in query.split('&') {
            let mut sub = pair.splitn(2, '=');
            if let (Some(k), Some(v)) = (sub.next(), sub.next()) {
                if (k.eq_ignore_ascii_case("sessionid")
                    || k.eq_ignore_ascii_case("mcp-session-id")
                    || k.eq_ignore_ascii_case("session_id"))
                    && !parts.headers.contains_key("mcp-session-id")
                {
                    if let Ok(hv) = HeaderValue::from_str(v) {
                        parts.headers.insert("mcp-session-id", hv);
                    }
                }
            }
        }
    }

    let mut body_bytes = None;

    if method == Method::POST {
        // `rmcp` strictly validates that Accept contains BOTH `application/json` AND `text/event-stream`.
        // Standard MCP clients (Cursor, VS Code, Claude, etc.) send `Accept: application/json` or `Accept: */*`.
        let needs_fix = match parts.headers.get(header::ACCEPT) {
            Some(val) => {
                if let Ok(s) = val.to_str() {
                    !(s.contains("application/json") && s.contains("text/event-stream"))
                } else {
                    true
                }
            }
            None => true,
        };
        if needs_fix {
            parts.headers.insert(
                header::ACCEPT,
                HeaderValue::from_static("application/json, text/event-stream"),
            );
        }

        // Buffer body to check if this is an `initialize` request or unsupported custom method.
        if let Ok(bytes) = axum::body::to_bytes(body, 4 * 1024 * 1024).await {
            if let Ok(json) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                let method_str = json.get("method").and_then(|m| m.as_str());
                if method_str == Some("initialize") {
                    parts.headers.remove("mcp-session-id");
                    parts.headers.remove("x-mcp-session-id");
                } else if method_str == Some("subscriptions/listen") || method_str == Some("subscriptions/subscribe") {
                    let id = json.get("id").cloned().unwrap_or(serde_json::Value::Null);
                    let resp_json = serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {}
                    });
                    let mut response = (
                        [
                            (header::CONTENT_TYPE, "application/json"),
                            (header::ACCESS_CONTROL_ALLOW_ORIGIN, "*"),
                            (header::ACCESS_CONTROL_EXPOSE_HEADERS, "*"),
                        ],
                        serde_json::to_string(&resp_json).unwrap_or_default(),
                    )
                        .into_response();
                    if let Some(sess_id) = parts.headers.get("mcp-session-id") {
                        response.headers_mut().insert("mcp-session-id", sess_id.clone());
                    }
                    return response;
                }
            }
            body_bytes = Some(bytes);
        }
    } else if method == Method::GET {
        // If GET request lacks mcp-session-id header, handle standard SSE endpoint discovery.
        if !parts.headers.contains_key("mcp-session-id") {
            let is_sse = parts
                .headers
                .get(header::ACCEPT)
                .and_then(|v| v.to_str().ok())
                .map(|s| s.contains("text/event-stream"))
                .unwrap_or(false);

            if is_sse {
                return (
                    [
                        (header::CONTENT_TYPE, "text/event-stream"),
                        (header::CACHE_CONTROL, "no-cache"),
                        (header::CONNECTION, "keep-alive"),
                        (header::ACCESS_CONTROL_ALLOW_ORIGIN, "*"),
                        (header::ACCESS_CONTROL_EXPOSE_HEADERS, "*"),
                    ],
                    "event: endpoint\ndata: /mcp\n\n",
                )
                    .into_response();
            }
        } else if !parts.headers.contains_key(header::ACCEPT) {
            parts.headers.insert(header::ACCEPT, HeaderValue::from_static("text/event-stream"));
        }
    }

    let req_body = body_bytes
        .map(axum::body::Body::from)
        .unwrap_or_else(axum::body::Body::empty);
    let req = Request::from_parts(parts, req_body);

    let mut response = next.run(req).await;
    let res_headers = response.headers_mut();
    res_headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"));
    res_headers.insert(
        header::ACCESS_CONTROL_EXPOSE_HEADERS,
        HeaderValue::from_static("mcp-session-id, content-type, authorization"),
    );
    response
}

pub fn router<S>(board: SharedBoard) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    Router::new()
        .fallback_service(service(board))
        .layer(middleware::from_fn(normalize_mcp_request))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Column, Origin};
    use crate::schema::Schema;
    use crate::store::Board;

    #[test]
    fn authority_from_public_url_strips_scheme_and_path() {
        assert_eq!(
            authority_from_public_url("https://board.example.ts.net:8080/mcp"),
            Some("board.example.ts.net:8080".into())
        );
        assert_eq!(
            authority_from_public_url("http://localhost:5173/"),
            Some("localhost:5173".into())
        );
    }

    #[test]
    fn split_host_port_handles_ipv6_and_bare_host() {
        assert_eq!(split_host_port("tot.example:8080"), Some(("tot.example", Some("8080"))));
        assert_eq!(split_host_port("tot.example"), Some(("tot.example", None)));
        assert_eq!(split_host_port("[::1]:8080"), Some(("[::1]", Some("8080"))));
    }

    fn test_board() -> (SharedBoard, ItemId) {
        let path = std::env::temp_dir().join(format!(
            "sandboard-mcp-test-{}.json",
            std::process::id()
        ));
        let b = Arc::new(Board::new(Schema::default(), path));
        let goal = b
            .create(None, "Test Goal", "Test Intent", None, Origin::Human, true, None)
            .expect("project");
        let _ = b.transition(goal.id, State::Shaping, "test", None);
        (b, goal.id)
    }

    #[test]
    fn list_column_returns_record_with_items_for_triage_columns() {
        let (board, goal_id) = test_board();
        let operator = Operator::new(board.clone());

        // Ready card
        let card_ready = board
            .create(
                Some(goal_id),
                "Ready Card",
                "Ready Intent",
                Some("DoD".into()),
                Origin::Human,
                false,
                None,
            )
            .expect("ready card");
        let _ = board.transition(card_ready.id, State::Shaping, "test", None);
        let _ = board.transition(card_ready.id, State::Backlog, "test", None);

        // NeedsYou card (escalated)
        let card_needs = board
            .create(
                Some(goal_id),
                "NeedsYou Card",
                "NeedsYou Intent",
                Some("DoD".into()),
                Origin::Human,
                false,
                None,
            )
            .expect("needs card");
        let _ = board.transition(card_needs.id, State::Shaping, "test", None);
        let _ = board.transition(card_needs.id, State::Backlog, "test", None);
        let _ = board.claim(card_needs.id, "agent-1", None, 60);
        let options = vec![
            crate::model::EscalationOption { label: "Opt A".into(), detail: "Detail A".into() },
            crate::model::EscalationOption { label: "Opt B".into(), detail: "Detail B".into() },
        ];
        let _ = board.escalate(card_needs.id, "agent-1", "Which option?".into(), options, 0);

        // Shaping card
        let card_shaping = board
            .create(
                Some(goal_id),
                "Shaping Card",
                "Shaping Intent",
                None,
                Origin::Human,
                false,
                None,
            )
            .expect("shaping card");
        let _ = board.transition(card_shaping.id, State::Shaping, "test", None);

        // Verify list_column for needs_you, ready, shaping returns a record object with "items"
        for col in [Column::NeedsYou, Column::Backlog, Column::Shaping] {
            let res = operator
                .list_column(Parameters(ColumnArg { column: col, goal: None }))
                .expect("list_column should succeed");
            let value = serde_json::to_value(&res.0).expect("serialize to value");

            assert!(value.is_object(), "structuredContent must be a JSON record/object, got: {:?}", value);
            let obj = value.as_object().unwrap();
            assert!(obj.contains_key("items"), "record must contain 'items' key");
            assert!(obj["items"].is_array(), "'items' value must be a JSON array");
        }
    }

    #[test]
    fn propose_breakdown_and_approve_plan_return_record_with_items() {
        let (board, goal_id) = test_board();
        let operator = Operator::new(board.clone());
        let _ = board
            .init_plan(goal_id)
            .expect("init_plan");

        let breakdown_arg = BreakdownArg {
            parent: goal_id,
            children: vec![ChildSpec {
                title: "Subtask 1".into(),
                intent: "Intent 1".into(),
                definition_of_done: "DoD 1".into(),
                capability: None,
                key: Some("t1".into()),
                blocked_by_keys: vec![],
                blocked_by: vec![],
                repo: None,
            }],
            summary: Some("one task".into()),
            cancel_keys: vec![],
        };

        let bd_res = operator
            .propose_breakdown(Parameters(breakdown_arg))
            .expect("propose_breakdown should succeed");
        let bd_val = serde_json::to_value(&bd_res.0).expect("serialize to value");
        assert!(bd_val.is_object(), "propose_breakdown response must be a JSON record object");
        assert!(bd_val.as_object().unwrap().contains_key("items"));

        let approve_res = operator
            .approve_plan(Parameters(IdArg { id: goal_id }))
            .expect("approve_plan should succeed");
        let app_val = serde_json::to_value(&approve_res.0).expect("serialize to value");
        assert!(app_val.is_object(), "approve_plan response must be a JSON record object");
        assert!(app_val.as_object().unwrap().contains_key("items"));
    }

    #[test]
    fn cut_scope_list_ready_and_split_return_record_with_items() {
        let (board, goal_id) = test_board();
        // Worker-verb shape checks use the host seat (operator hides list_ready).
        let operator = Operator::host(board.clone());

        // list_ready
        let ready_res = operator
            .list_ready(Parameters(ListReadyArg { capabilities: vec!["any".into()] }))
            .expect("list_ready should succeed");
        let ready_val = serde_json::to_value(&ready_res.0).expect("serialize to value");
        assert!(ready_val.is_object(), "list_ready response must be a JSON record object");
        assert!(ready_val.as_object().unwrap().contains_key("items"));

        // cut_scope
        let cut_res = operator
            .cut_scope(Parameters(ReasonArg { id: goal_id, reason: Some("retired".into()) }))
            .expect("cut_scope should succeed");
        let cut_val = serde_json::to_value(&cut_res.0).expect("serialize to value");
        assert!(cut_val.is_object(), "cut_scope response must be a JSON record object");
        assert!(cut_val.as_object().unwrap().contains_key("items"));
    }

    #[test]
    fn unarchive_scope_restores_and_rejects_non_retired() {
        let (board, goal_id) = test_board();
        let operator = Operator::new(board.clone());

        let reject = operator.unarchive_scope(Parameters(ReasonArg {
            id: goal_id,
            reason: None,
        }));
        let err = match reject {
            Ok(_) => panic!("unarchive on live Project must fail"),
            Err(e) => e,
        };
        assert!(
            err.message.contains("not retired"),
            "expected not-retired error, got {}",
            err.message
        );

        board
            .cut_scope(goal_id, Some("archived".into()))
            .expect("cut");
        let ok = operator
            .unarchive_scope(Parameters(ReasonArg {
                id: goal_id,
                reason: Some("restored".into()),
            }))
            .expect("unarchive_scope should succeed");
        let ok_val = serde_json::to_value(&ok.0).expect("serialize");
        assert!(ok_val.is_object(), "unarchive_scope response must be a JSON record");
        assert!(ok_val.as_object().unwrap().contains_key("items"));
        assert!(
            ok.0.items.contains(&goal_id),
            "restored root must be in items: {:?}",
            ok.0.items
        );
        assert_eq!(board.get(goal_id).unwrap().state, State::Shaping);
    }

    #[test]
    fn board_snapshot_surfaces_recent_retired_under_live_goal() {
        let (board, goal_id) = test_board();
        let operator = Operator::new(board.clone());
        let leaf = board
            .create(
                Some(goal_id),
                "Bake CLI into sandbox image",
                "install binary in Containerfile",
                Some("image rebuilds".into()),
                Origin::Human,
                false,
                None,
            )
            .expect("leaf");
        let _ = board.transition(leaf.id, State::Shaping, "test", None);
        let _ = board.transition(leaf.id, State::Backlog, "test", None);
        board
            .cut_scope(leaf.id, Some("done locally, not by an agent".into()))
            .expect("retire leaf");

        let snap = operator.board_snapshot().expect("snapshot");
        let goal = snap
            .0
            .goals
            .iter()
            .find(|g| g.goal == goal_id)
            .expect("goal line");
        assert!(
            goal.recent_retired
                .iter()
                .any(|l| l.contains(&format!("#{}", leaf.id))
                    && l.contains("Bake CLI")
                    && l.contains("done locally")),
            "snapshot must surface mid-project retire: {:?}",
            goal.recent_retired
        );

        let digest = operator.board_digest().expect("digest");
        let dg = digest
            .0
            .goals
            .iter()
            .find(|g| g.goal_id == goal_id)
            .expect("digest goal");
        assert_eq!(dg.recently_retired.len(), 1);
        assert_eq!(dg.recently_retired[0].id, leaf.id);
        assert_eq!(
            dg.recently_retired[0].reason.as_deref(),
            Some("done locally, not by an agent")
        );
    }

    #[test]
    fn item_detail_children_include_state_and_last_reason() {
        let (board, goal_id) = test_board();
        let operator = Operator::new(board.clone());
        let leaf = board
            .create(
                Some(goal_id),
                "Child task",
                "why",
                Some("dod".into()),
                Origin::Human,
                false,
                None,
            )
            .expect("leaf");
        let _ = board.transition(leaf.id, State::Shaping, "test", None);
        board
            .cut_scope(leaf.id, Some("cut reason".into()))
            .expect("retire");

        let detail = operator
            .item_detail(Parameters(IdArg { id: goal_id }))
            .expect("detail");
        let kids = detail.0["children"].as_array().expect("children array");
        let child = kids
            .iter()
            .find(|c| c["id"] == leaf.id)
            .expect("child row");
        assert_eq!(child["title"], "Child task");
        assert_eq!(child["state"], "Retired");
        assert_eq!(child["last_reason"], "cut reason");
    }

    #[test]
    fn search_items_finds_by_title_and_history_reason() {
        let (board, goal_id) = test_board();
        let operator = Operator::new(board.clone());
        let leaf = board
            .create(
                Some(goal_id),
                "Bake OpenCode CLI into sandbox image",
                "policy hosts",
                Some("opencode --version".into()),
                Origin::Human,
                false,
                None,
            )
            .expect("leaf");
        let _ = board.transition(leaf.id, State::Shaping, "test", None);
        board
            .cut_scope(
                leaf.id,
                Some("Sandbox image work will be done locally".into()),
            )
            .expect("retire");

        let by_title = operator
            .search_items(Parameters(SearchArg {
                query: "sandbox image".into(),
                goal: None,
                limit: 20,
            }))
            .expect("search title");
        assert!(
            by_title.0.items.iter().any(|h| h.id == leaf.id && h.matched_in == "title"),
            "title hit: {:?}",
            by_title.0.items
        );

        let by_reason = operator
            .search_items(Parameters(SearchArg {
                query: "done locally".into(),
                goal: Some(goal_id),
                limit: 20,
            }))
            .expect("search reason");
        assert!(
            by_reason
                .0
                .items
                .iter()
                .any(|h| h.id == leaf.id && h.matched_in == "history"),
            "history hit: {:?}",
            by_reason.0.items
        );
    }

    #[test]
    fn list_column_sorts_unblocked_ready_first() {
        let (board, goal_id) = test_board();
        let operator = Operator::new(board.clone());

        // Card 1: unblocked
        let c1 = board.create(Some(goal_id), "Card 1", "Unblocked", Some("DoD".into()), Origin::Human, false, None).expect("c1");
        let _ = board.transition(c1.id, State::Shaping, "test", None);
        let _ = board.transition(c1.id, State::Backlog, "test", None);

        // Card 2: blocked by Card 1
        let c2 = board.create(Some(goal_id), "Card 2", "Blocked", Some("DoD".into()), Origin::Human, false, None).expect("c2");
        let _ = board.transition(c2.id, State::Shaping, "test", None);
        let _ = board.transition(c2.id, State::Backlog, "test", None);
        board.set_blocked_by(c2.id, vec![c1.id]);

        // Bounce Card 1 through claim and release so its entered_state_at is NEWER than Card 2
        let _ = board.claim(c1.id, "agent-1", None, 60).expect("claim");
        let _ = board.release(c1.id, "agent-1").expect("release");

        let res = operator
            .list_column(Parameters(ColumnArg { column: Column::Backlog, goal: Some(goal_id) }))
            .expect("list_column should succeed");

        let pos_c1 = res.0.items.iter().position(|i| i.id == c1.id).expect("c1 present");
        let pos_c2 = res.0.items.iter().position(|i| i.id == c2.id).expect("c2 present");
        assert!(pos_c1 < pos_c2, "Unblocked card #1 must sort before blocked card #2");
    }

    #[tokio::test]
    async fn normalize_mcp_request_fixes_accept_header_and_handles_sse_discovery() {
        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use tower_service::Service;

        let (board, _) = test_board();
        let mut app = router::<()>(board);

        // POST request with Accept: application/json should NOT return 406
        let req = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("content-type", "application/json")
            .header("accept", "application/json")
            .body(Body::from(
                r#"{"jsonrpc":"2.0","method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}},"id":1}"#,
            ))
            .unwrap();

        let response = app.call(req).await.unwrap();
        assert_ne!(response.status(), StatusCode::NOT_ACCEPTABLE);

        // GET request without session id should return SSE endpoint discovery
        let req = Request::builder()
            .method("GET")
            .uri("/mcp")
            .header("accept", "text/event-stream")
            .body(Body::empty())
            .unwrap();

        let response = app.call(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let ct = response.headers().get("content-type").unwrap().to_str().unwrap();
        assert!(ct.contains("text/event-stream"));

        // POST request with subscriptions/listen method should return JSON-RPC 200 result
        let req = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("content-type", "application/json")
            .header("accept", "application/json")
            .body(Body::from(
                r#"{"jsonrpc":"2.0","method":"subscriptions/listen","id":99}"#,
            ))
            .unwrap();

        let response = app.call(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    /// Cursor keeps a dead `Mcp-Session-Id` across `cargo run` restarts. With
    /// legacy sessions that was a hard 404; stateless mode must ignore it.
    #[tokio::test]
    async fn mcp_ignores_stale_session_id_after_restart() {
        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use tower_service::Service;

        let (board, _) = test_board();
        let mut app = router::<()>(board);

        let init = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("host", "127.0.0.1:8080")
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .header("mcp-session-id", "dead-session-from-previous-sandboard-process")
            .body(Body::from(
                r#"{"jsonrpc":"2.0","method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"cursor","version":"1.0"}},"id":1}"#,
            ))
            .unwrap();
        let response = app.call(init).await.unwrap();
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "stale session must not 404 initialize"
        );
        assert!(
            response.headers().get("mcp-session-id").is_none(),
            "stateless initialize must not mint a session id for Cursor to cling to"
        );

        // tools/list with the same dead id — the restart failure mode.
        let list = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("host", "127.0.0.1:8080")
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .header("mcp-protocol-version", "2024-11-05")
            .header("mcp-session-id", "dead-session-from-previous-sandboard-process")
            .body(Body::from(
                r#"{"jsonrpc":"2.0","method":"tools/list","params":{},"id":2}"#,
            ))
            .unwrap();
        let response = app.call(list).await.unwrap();
        assert_ne!(
            response.status(),
            StatusCode::NOT_FOUND,
            "stale session must not 404 tools/list"
        );
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024)
            .await
            .expect("read body");
        let body = String::from_utf8_lossy(&bytes);
        assert!(
            body.contains("board_snapshot") || body.contains("tools"),
            "expected tools listing, got: {body}"
        );
    }

    #[test]
    fn approve_review_mcp_returns_dispatch_next_note() {
        let (board, goal_id) = test_board();
        let operator = Operator::new(board.clone());

        let t1 = board
            .create(Some(goal_id), "Task 1", "intent 1", Some("dod 1".into()), Origin::Human, false, None)
            .unwrap();
        let t2 = board
            .create(Some(goal_id), "Task 2", "intent 2", Some("dod 2".into()), Origin::Human, false, None)
            .unwrap();
        board.set_blocked_by(t2.id, vec![t1.id]);

        let _ = board.transition(t1.id, State::Shaping, "test", None);
        let _ = board.transition(t1.id, State::Backlog, "test", None);
        let _ = board.transition(t1.id, State::Claimed, "agent", None);
        let _ = board.transition(t1.id, State::Running, "agent", None);
        let _ = board.transition(t1.id, State::Review, "agent", None);

        let ack = operator.approve_review(Parameters(IdArg { id: t1.id })).expect("approve_review");
        assert_eq!(ack.0.note, format!("approved — dispatch #{} next", t2.id));
    }

    #[test]
    fn create_task_mcp_lands_in_backlog_and_refuses_nest() {
        let path = std::env::temp_dir().join(format!(
            "sandboard-mcp-create-task-{}.json",
            std::process::id()
        ));
        let board = Arc::new(Board::new(Schema::default(), path));
        let operator = Operator::new(board.clone());

        let project = board
            .create_project("Proj", "Ship it", "sandboard-app/sandboard", true, None)
            .expect("project");

        let ack = operator
            .create_task(Parameters(CreateTaskArg {
                parent: project.id,
                title: "Ad hoc".into(),
                intent: "Do the thing".into(),
                definition_of_done: "thing done".into(),
                blocked_by: vec![],
                capability: None,
                above_line: false,
            }))
            .expect("create_task");
        assert!(ack.0.ok);
        assert_eq!(ack.0.state, format!("{:?}", State::Backlog));
        assert!(
            ack.0.note.contains("Backlog"),
            "ack should mirror create_project style: {}",
            ack.0.note
        );

        let task = board.get(ack.0.item).expect("created task");
        assert_eq!(task.state, State::Backlog);
        assert_eq!(task.parent, Some(project.id));
        assert!(
            task.intent.contains("sandboard-app/sandboard"),
            "Project clone_repo should stamp when intent omits clone: {}",
            task.intent
        );

        let nest = operator.create_task(Parameters(CreateTaskArg {
            parent: task.id,
            title: "Nested".into(),
            intent: "should fail".into(),
            definition_of_done: "no".into(),
            blocked_by: vec![],
            capability: None,
            above_line: false,
        }));
        let nest_err = match nest {
            Ok(_) => panic!("nesting under a Task must fail"),
            Err(e) => e,
        };
        assert!(
            nest_err.message.contains("flat under a Project")
                || nest_err.message.contains("nest"),
            "nest refusal must be clear: {}",
            nest_err.message
        );

        let missing = operator.create_task(Parameters(CreateTaskArg {
            parent: 9_999_999,
            title: "Orphan".into(),
            intent: "no parent".into(),
            definition_of_done: "no".into(),
            blocked_by: vec![],
            capability: None,
            above_line: false,
        }));
        let missing_err = match missing {
            Ok(_) => panic!("missing parent must fail"),
            Err(e) => e,
        };
        assert!(
            missing_err.message.contains("no parent"),
            "missing parent must be clear: {}",
            missing_err.message
        );
    }

    #[test]
    fn operator_seat_instructions_align_configuration_layers() {
        let (board, _) = test_board();
        let operator = Operator::new(board);
        let instructions = operator
            .get_info()
            .instructions
            .unwrap_or_default();
        assert!(
            instructions.contains("Configuration layers"),
            "operator instructions must name configuration layers: {instructions}"
        );
        assert!(
            instructions.contains("project_prompt"),
            "operator instructions must explain project_prompt: {instructions}"
        );
        assert!(
            instructions.contains("do not assume cargo") || instructions.contains("does not assume cargo"),
            "operator instructions must not invent cargo gates: {instructions}"
        );
        assert!(
            instructions.contains("standing prompt") || instructions.contains("Agent runtime"),
            "operator instructions must mention board standing prompt: {instructions}"
        );
        assert!(
            instructions.contains("Boot, Settings, and Project fields"),
            "operator instructions must separate operator config from project_prompt: {instructions}"
        );
    }

    #[test]
    fn operator_seat_lists_operator_tools_and_hides_worker_verbs() {
        let (board, _) = test_board();
        let operator = Operator::new(board);
        assert_eq!(operator.seat(), McpSeat::Operator);

        let names: Vec<_> = operator
            .tool_router
            .list_all()
            .into_iter()
            .map(|t| t.name.to_string())
            .collect();

        for tool in [
            "board_snapshot",
            "search_items",
            "item_detail",
            "create_project",
            "create_task",
            "dispatch",
            "park",
            "steer",
            "cut_scope",
            "unarchive_scope",
            "approve_review",
            "approve_plan",
            "answer_escalation",
        ] {
            assert!(
                names.iter().any(|n| n == tool),
                "operator must list operator tool {tool}; got {names:?}"
            );
            assert!(
                operator.tool_router.has_route(tool),
                "operator must expose {tool}"
            );
        }

        for verb in WORKER_VERB_TOOLS {
            assert!(
                !names.iter().any(|n| n == *verb),
                "operator must not list worker verb {verb}; got {names:?}"
            );
            assert!(
                !operator.tool_router.has_route(verb),
                "operator must disable {verb}"
            );
        }
    }

    #[test]
    fn operator_seat_invokes_operator_tools_and_denies_worker_verbs() {
        let (board, goal_id) = test_board();
        let operator = Operator::new(board.clone());

        let snap = operator.board_snapshot().expect("operator board_snapshot");
        assert!(!snap.0.goals.is_empty(), "operator can read the board");

        let park = operator.park(Parameters(ReasonArg {
            id: goal_id,
            reason: Some("operator smoke".into()),
        }));
        if let Err(e) = &park {
            assert!(
                !e.message.contains("worker verb"),
                "operator park must not be seat-denied: {}",
                e.message
            );
        }

        let claim = operator.claim(Parameters(ClaimArg {
            item_id: goal_id,
            agent_id: "operator-agent".into(),
            model: None,
            lease_secs: 60,
        }));
        let err = match claim {
            Ok(_) => panic!("operator must deny claim"),
            Err(e) => e,
        };
        assert!(
            err.message.contains("worker verb") && err.message.contains("operator tools only"),
            "unexpected deny message: {}",
            err.message
        );

        for (label, result) in [
            (
                "list_ready",
                operator
                    .list_ready(Parameters(ListReadyArg {
                        capabilities: vec!["any".into()],
                    }))
                    .map(|_| ()),
            ),
            (
                "heartbeat",
                operator
                    .heartbeat(Parameters(HeartbeatArg {
                        item_id: goal_id,
                        agent_id: "operator-agent".into(),
                        progress: 0.1,
                        lease_secs: 30,
                        cost_cents: 0,
                    }))
                    .map(|_| ()),
            ),
            (
                "report",
                operator
                    .report(Parameters(ReportArg {
                        item_id: goal_id,
                        agent_id: "operator-agent".into(),
                        lines_added: 1,
                        lines_removed: 0,
                    }))
                    .map(|_| ()),
            ),
            (
                "release",
                operator
                    .release(Parameters(AgentItemArg {
                        item_id: goal_id,
                        agent_id: "operator-agent".into(),
                    }))
                    .map(|_| ()),
            ),
            (
                "escalate",
                operator
                    .escalate(Parameters(EscalateArg {
                        item_id: goal_id,
                        agent_id: "operator-agent".into(),
                        question: "pick?".into(),
                        options: vec![
                            OptionSpec {
                                label: "A".into(),
                                detail: "one".into(),
                            },
                            OptionSpec {
                                label: "B".into(),
                                detail: "two".into(),
                            },
                        ],
                        recommended: 0,
                    }))
                    .map(|_| ()),
            ),
            (
                "split",
                operator
                    .split(Parameters(SplitArg {
                        item_id: goal_id,
                        agent_id: "operator-agent".into(),
                        children: vec![
                            ChildSpec {
                                title: "a".into(),
                                intent: "a".into(),
                                definition_of_done: "a".into(),
                                capability: None,
                                key: None,
                                blocked_by_keys: vec![],
                                blocked_by: vec![],
                                repo: None,
                            },
                            ChildSpec {
                                title: "b".into(),
                                intent: "b".into(),
                                definition_of_done: "b".into(),
                                capability: None,
                                key: None,
                                blocked_by_keys: vec![],
                                blocked_by: vec![],
                                repo: None,
                            },
                        ],
                    }))
                    .map(|_| ()),
            ),
        ] {
            match result {
                Ok(_) => panic!("operator must deny {label}"),
                Err(e) => assert!(
                    e.message.contains("worker verb"),
                    "{label} deny message: {}",
                    e.message
                ),
            }
        }
    }

    #[test]
    fn host_seat_lists_and_invokes_worker_verbs() {
        let (board, goal_id) = test_board();
        let host = Operator::host(board.clone());
        assert_eq!(host.seat(), McpSeat::Host);

        let names: Vec<_> = host
            .tool_router
            .list_all()
            .into_iter()
            .map(|t| t.name.to_string())
            .collect();
        for verb in WORKER_VERB_TOOLS {
            assert!(
                names.iter().any(|n| n == *verb),
                "host seat must list {verb}; got {names:?}"
            );
        }

        let leaf = board
            .create(
                Some(goal_id),
                "Host leaf",
                "claimable",
                Some("DoD".into()),
                Origin::Human,
                false,
                None,
            )
            .expect("leaf");
        let _ = board.transition(leaf.id, State::Shaping, "test", None);
        let _ = board.transition(leaf.id, State::Backlog, "test", None);

        let grant = match host.claim(Parameters(ClaimArg {
            item_id: leaf.id,
            agent_id: "host-agent".into(),
            model: None,
            lease_secs: 60,
        })) {
            Ok(g) => g,
            Err(e) => panic!("host claim: {}", e.message),
        };
        assert_eq!(grant.0.item_id, leaf.id);

        if let Err(e) = host.heartbeat(Parameters(HeartbeatArg {
            item_id: leaf.id,
            agent_id: "host-agent".into(),
            progress: 0.5,
            lease_secs: 30,
            cost_cents: 0,
        })) {
            panic!("host heartbeat: {}", e.message);
        }

        // Board-direct supervisor path remains authoritative.
        board
            .heartbeat(leaf.id, "host-agent", 0.6, 30)
            .expect("board heartbeat still works");
    }


}
