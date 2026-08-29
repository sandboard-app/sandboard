//! The execution side of the board.
//!
//! An agent is **material, not a participant in the control plane**. It gets no
//! network path to sandboard; the supervisor calls `claim`/`heartbeat`/`report` on
//! its behalf. An agent that could reach sandboard's MCP could approve its own
//! review. (OpenShell only forwards host→sandbox anyway, which independently
//! forces this shape.)
//!
//! Three properties are load-bearing:
//!
//! - **Liveness is observed, never self-reported.** It comes from parsing the
//!   agent's `stream-json` as it arrives, so a hung agent cannot claim to be
//!   fine by sending heartbeats on its own.
//! - **Everything fails as a hang.** Every exec carries a deadline, and silence
//!   is treated as failure rather than patience.
//! - **The supervisor reads the run; it does not own it.** The agent is started
//!   detached and writes to a log, so watching is a thing a *different* sandboard
//!   process can pick up after a restart. See `reconcile`.

use crate::model::{
    CockpitSandboxPhase, CockpitSession, CockpitSessionStatus, ItemId, State, WorkItem,
};
use crate::openshell::{OpenShell, Output, SandboxSpec, LABEL_COCKPIT, LABEL_ITEM};
use crate::schema::{AgentConfig, ExecutionConfig};
use crate::store::{ClaimGrant, SharedBoard};

use chrono::{DateTime, Utc};
use std::collections::{BTreeMap, HashSet};
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

/// Where the agent works inside the sandbox. `/sandbox` is $HOME and writable;
/// the policy's `read_write` list has to agree with this.
const WORKDIR: &str = "/sandbox/repo";
/// Control-plane verdict dir — must be under `/sandbox` (writable HOME). `/work`
/// does not exist in the OpenShell image and the sandbox user cannot `mkdir` it
/// at `/` (Permission denied). Never put verdicts under `{WORKDIR}/.sandboard`.
const VERDICT_DIR: &str = "/sandbox/.sandboard";

/// The agent's output, its process group, and its exit code — in `/tmp` rather
/// than the checkout, so the agent's own `git clean` cannot take the record of
/// its run with it. These three files are the entire contract between a run and
/// whichever supervisor happens to be watching it.
const AGENT_LOG: &str = "/tmp/agent.log";
const AGENT_PID: &str = "/tmp/agent.pid";
const AGENT_STATUS: &str = "/tmp/agent.status";

type Active = Arc<parking_lot::Mutex<std::collections::HashSet<ItemId>>>;
type Cooldown = Arc<parking_lot::Mutex<Option<std::time::Instant>>>;

pub fn spawn(board: SharedBoard, cfg: ExecutionConfig) {
    // Webhook poll is independent of agent execution — always run so Forge
    // Settings can complete merges when `gh webhook forward` is down.
    tokio::spawn(crate::github_poll::poll_loop(board.clone()));
    tokio::spawn(crate::github_app::repo_access_refresh_loop(board.clone()));

    // Durable Settings overlay (seeded from compiled defaults at board load).
    let agents = board.effective_agents();
    if let Err(e) = agents.validate() {
        tracing::error!("agent config misconfigured: {e}");
        tokio::spawn(sweeper_loop(board, cfg, Arc::default()));
        return;
    }
    let cfg = ExecutionConfig { agents, ..cfg };
    // The sweeper starts *inside* `dispatch_loop`, once reconciliation has
    // finished. A card that was mid-run when sandboard died has not been
    // heartbeaten since, so a sweep that lands first requeues a run that is
    // still going — and then dispatch starts a second agent on the same branch.
    tokio::spawn(dispatch_loop(board.clone(), cfg.clone()));
    // Cockpit is independent of the card-dispatch queue: Board `cockpit_session`
    // is authoritative; this loop materializes sandbox + detached agent only.
    tokio::spawn(cockpit_seat_loop(board, cfg));
}

/// Requeue cards past `run_deadline_at`. The matching supervise task notices on
/// its next board poll (see `watch_agent`) and frees the concurrency slot —
/// the sweeper alone must not leave `in_flight` stuck on a zombie watch.
///
/// Also periodically reconciles sandbox inventory (reap terminal / keep parked).
/// `active` must be the same set dispatch uses so we never treat an in-flight
/// setup as "no live agent" and bounce the card back to Backlog.
async fn sweeper_loop(board: SharedBoard, cfg: ExecutionConfig, active: Active) {
    loop {
        for id in board.sweep_leases() {
            tracing::info!("run deadline exceeded on #{id}; requeued");
        }
        // Live Settings → OpenShell (endpoint + mTLS). Do not freeze a client at
        // spawn — operators often paste certs after sandboard is already up.
        let os = board.openshell_client();
        // Periodic sweep: do not reopen Backlog cards (that loops). Detach
        // leaves Claimed|Running; only startup reconcile repairs old damage.
        let _ = reconcile(
            &os,
            &board,
            &active,
            cfg.agents.agent_timeout_secs as i64,
            false,
        )
        .await;
        let _ = process_awaiting_mergeable_checks(&board, &cfg.agents).await;
        // Refresh installation token into the gateway well before the ~1h expiry.
        if let Err(e) = crate::github_app::ensure_github_provider(&board).await {
            tracing::warn!("GitHub App provider sync: {e}");
        }
        // Re-read Settings → Agent runtime each tick so saves apply without restart.
        let ms = board
            .agent_runtime()
            .map(|r| r.sweep_interval_ms)
            .filter(|ms| *ms >= 100)
            .unwrap_or(cfg.sweep_interval_ms)
            .max(100);
        tokio::time::sleep(Duration::from_millis(ms)).await;
    }
}

/// How often setup/watch loops re-check that the board still owns the card.
/// Production: 2s is plenty (Halt is human-speed). Tests: 10ms so cancel
/// coverage does not burn ~2s per case. Setup used to ignore the board —
/// a Halt mid-clone left `in_flight` stuck at `max_concurrent`.
fn watch_board_poll() -> Duration {
    if cfg!(test) {
        Duration::from_millis(10)
    } else {
        Duration::from_secs(2)
    }
}

/// Board states in which this process may keep watching a sandbox.
fn board_still_owns_run(state: State) -> bool {
    matches!(state, State::Claimed | State::Running)
}

/// Supervise ended because the board (deadline sweeper / Halt) already released
/// the card — not because the work failed.
fn is_supervisor_cancel(err: &str) -> bool {
    err.contains("run cancelled:")
}

/// The log follower died because sandboard was interrupted — not because the agent
/// finished. `follow_script` is an `openshell exec` of `tail -f --pid=…`; Ctrl-C
/// kills that exec with -1/130/143 while the setsid agent inside the sandbox
/// keeps going. Treating that as a card failure bounced the card to Backlog so
/// restart could not re-adopt.
fn is_supervisor_detach(err: &str) -> bool {
    matches!(agent_exit_code(err), Some(-1 | 130 | 143))
}

/// Parse `finish()`'s `"agent exited {code}: …"` (first occurrence).
fn agent_exit_code(err: &str) -> Option<i32> {
    const P: &str = "agent exited ";
    let i = err.find(P)?;
    let rest = &err[i + P.len()..];
    let token: String = rest
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '-')
        .collect();
    token.parse().ok()
}

/// Bail if Halt / deadline sweep / cut already moved the card off Claimed|Running.
/// Scratch directory for a file pulled out of a sandbox.
///
/// The card id plus a timestamp is not unique enough: `Utc::now()` is not
/// guaranteed to advance between two calls on the same tick, and a retried card
/// keeps its id. Two downloads then share a directory and the first one to
/// finish deletes the other's file mid-read — which is exactly how the
/// `process_verdict` tests flake under a parallel test runner.
fn scratch_dir(prefix: &str, id: ItemId) -> std::path::PathBuf {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "sandboard-{prefix}-{id}-{}-{seq}",
        std::process::id()
    ))
}

fn ensure_board_owns_run(board: &SharedBoard, id: ItemId) -> anyhow::Result<()> {
    let Some(item) = board.get(id) else {
        anyhow::bail!("run cancelled: card #{id} gone from the board");
    };
    if !board_still_owns_run(item.state) {
        anyhow::bail!(
            "run cancelled: card left {:?} (deadline exceeded or halted)",
            item.state
        );
    }
    Ok(())
}

/// Run `fut`, aborting within about [`watch_board_poll`] if the board
/// releases the card. Dropping `fut` cancels the underlying openshell exec
/// (`kill_on_drop`), which is what frees `in_flight` after Halt during setup.
async fn with_board_cancel<F, T>(board: &SharedBoard, id: ItemId, fut: F) -> anyhow::Result<T>
where
    F: Future<Output = anyhow::Result<T>>,
{
    tokio::pin!(fut);
    let mut poll = tokio::time::interval(watch_board_poll());
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // First tick completes immediately; skip so we don't race the caller.
    poll.tick().await;
    loop {
        tokio::select! {
            res = &mut fut => return res,
            _ = poll.tick() => ensure_board_owns_run(board, id)?,
        }
    }
}

/// Wait this long after the infrastructure fails before trying again. The
/// podman machine stops on its own — three times in one session — and retrying
/// every 3s just converts an outage into a wall of identical errors.
const INFRA_COOLDOWN: Duration = Duration::from_secs(60);

/// Did this run fail because of the machinery rather than the card?
///
/// It matters because the two get different treatment. A card that genuinely
/// cannot be done should exhaust its retries and ask a human. A dead podman
/// socket — or an OpenShell client `h2 protocol error` / `broken pipe` mid
/// stream — must not burn those retries. Otherwise the board reports "failed
/// to run 3 times without producing any work" about a card that never got the
/// chance to run at all, which is exactly what it did report.
fn is_infrastructure(err: &str) -> bool {
    // Compared case-insensitively so OS "Broken pipe" and Rust ErrorKind
    // "broken pipe" both count. Strings below are lowercase on purpose.
    const SIGNS: [&str; 15] = [
        "podman.sock",
        "connection error",
        "connection closed before message completed",
        "create sandbox failed",
        "gateway",
        // OpenShell relay flakes during create→exec; not the card's fault.
        "exec relay closed",
        "sandbox is not ready",
        "the service is currently unavailable",
        "ssh tar extract exited",
        "ssh exited with status",
        "phase: deleting",
        "github app token sync failed (infrastructure)",
        // tonic/hyper on the OpenShell client: h2 reset, peer drop, or body
        // truncated mid-exec — must not burn the card's 3-strike run budget.
        "h2 protocol error",
        "broken pipe",
        "error reading a body from connection",
    ];
    let lower = err.to_ascii_lowercase();
    SIGNS.iter().any(|s| lower.contains(s))
}

/// What every run shares with the loop it belongs to.
///
/// Bundled rather than threaded through as six arguments, because dispatch and
/// adoption both need all of it and the bookkeeping around a run must not
/// differ by how the run started.
#[derive(Clone)]
struct Fleet {
    board: SharedBoard,
    in_flight: Arc<AtomicU64>,
    /// Which cards this process is actively supervising.
    ///
    /// Prevents dispatch from double-claiming a card whose watch is still
    /// alive. Paired with `with_board_cancel` during setup + watch (so a Halt /
    /// cut / deadline sweep frees `in_flight` even mid-clone). A sandbox label
    /// is *not* the right evidence here — failed sandboxes are kept for
    /// inspection, so the label outlives the run. `reconcile` is the one place
    /// that reads labels, and it cross-checks them against the card.
    active: Active,
    cooldown: Cooldown,
}

impl Fleet {
    /// Fresh client from current Settings (endpoint + sealed mTLS).
    fn os(&self) -> OpenShell {
        self.board.openshell_client()
    }

    /// Everything that has to happen around a run, whichever way it started.
    ///
    /// Adopted runs go through here too, so a run that survived a restart
    /// cannot quietly get different failure accounting from a fresh one.
    fn supervise<F>(&self, id: ItemId, agent_id: String, work: F)
    where
        F: std::future::Future<Output = anyhow::Result<()>> + Send + 'static,
    {
        self.in_flight.fetch_add(1, Ordering::Relaxed);
        self.active.lock().insert(id);
        let f = self.clone();
        tokio::spawn(async move {
            match work.await {
                Ok(()) => f.board.clear_run_failures(id),
                Err(e) => {
                    let msg = e.to_string();
                    if is_supervisor_cancel(&msg) {
                        // Deadline sweeper or Halt already moved the card. Do not
                        // burn retry budget or release again — just let
                        // finalize stop the agent and free the slot below.
                        tracing::info!("#{id}: {msg}");
                    } else if is_supervisor_detach(&msg) {
                        // Sandboard is going away; leave Claimed/Running + sandbox so
                        // the next process can adopt. Do not count a failure.
                        tracing::info!(
                            "#{id}: supervisor detached ({msg}); leaving run for re-adoption"
                        );
                    } else if is_infrastructure(&msg) {
                        // Not the card's fault. Give it back untouched and stop
                        // dispatching for a while rather than spending the
                        // card's retry budget on a broken machine.
                        tracing::warn!("#{id}: infrastructure failure, not counting it: {msg}");
                        *f.cooldown.lock() = Some(std::time::Instant::now() + INFRA_COOLDOWN);
                        let _ = f.board.release(id, &agent_id);
                    } else {
                        tracing::error!("#{id} failed: {msg}");
                        // Count it. After `max_attempts` this becomes a human's
                        // problem instead of an overnight loop.
                        if let Err(e2) = f.board.record_run_failure(
                            id,
                            &msg,
                            f.board.effective_agents().max_attempts,
                        ) {
                            tracing::error!("#{id}: could not record failure: {e2}");
                        }
                    }
                }
            }
            f.active.lock().remove(&id);
            f.in_flight.fetch_sub(1, Ordering::Relaxed);
        });
    }
}

async fn dispatch_loop(board: SharedBoard, cfg: ExecutionConfig) {
    let timeout_secs = cfg.agents.agent_timeout_secs as i64;
    let fleet = Fleet {
        board: board.clone(),
        in_flight: Arc::default(),
        active: Arc::default(),
        cooldown: Arc::default(),
    };

    // Pick up whatever survived the last process before anything else — the
    // sweeper included — gets a chance to act on those cards. Re-reads Settings
    // each poll so endpoint/mTLS pasted during the grace window still adopts.
    for a in reconcile_once_reachable(&board, timeout_secs, GATEWAY_GRACE).await {
        let (id, agent_id) = (a.item_id, a.agent_id.clone());
        fleet.supervise(id, agent_id, adopt_card(fleet.clone(), a));
    }

    tokio::spawn(sweeper_loop(
        board.clone(),
        cfg.clone(),
        fleet.active.clone(),
    ));

    let mut tick = tokio::time::interval(Duration::from_secs(3));
    loop {
        tick.tick().await;

        // Only awaiting_dispatch cards are claimed (Start / MCP dispatch / Project
        // auto mode). Adoption and in-flight runs are independent of that queue.

        // Live Settings → Agent runtime (engine / timeout / concurrency).
        let agents = board.effective_agents();

        if fleet.in_flight.load(Ordering::Relaxed) as usize >= agents.max_concurrent {
            continue;
        }
        if fleet
            .cooldown
            .lock()
            .is_some_and(|t| std::time::Instant::now() < t)
        {
            continue;
        }
        // The compute driver / gateway stop on their own. Claiming a card we
        // can't run would strand it until the lease lapsed. Live Settings
        // OpenShell client (endpoint + mTLS).
        if !board.openshell_client().healthy().await {
            tracing::warn!("openshell gateway unhealthy; not claiming");
            continue;
        }

        // Projects with auto mode queue their claimable Backlog leaves; others
        // stay inert until Start / MCP dispatch.
        board.auto_enqueue_all();

        let awaiting = board.list_awaiting_dispatch();
        let Some(item) = awaiting
            .into_iter()
            .find(|i| !fleet.active.lock().contains(&i.id) && board.may_claim(i.id))
        else {
            continue;
        };

        let agent_id = format!("sandbox-{}", item.id);
        let grant = match board.claim(item.id, &agent_id, None, agents.agent_timeout_secs as i64) {
            Ok(g) => g,
            Err(e) => {
                tracing::debug!("claim of #{} refused: {e}", item.id);
                continue;
            }
        };

        fleet.supervise(
            item.id,
            agent_id.clone(),
            run_card(fleet.clone(), agent_id, grant),
        );
    }
}

// ------------------------------------------------------ surviving a restart

/// A run that outlived the process supervising it.
///
/// sandboard is rebuilt constantly while sandboard is what's being built, so a restart
/// mid-run is the normal case, not an incident. Killing the sandbox was the
/// safe stopgap: correct, and it threw away a five-minute run every time.
/// Re-adopting keeps the run going and the card Running.
#[derive(Debug, Clone)]
struct Adoption {
    item_id: ItemId,
    agent_id: String,
    sandbox: String,
    /// First log line this process has not already streamed. Everything before
    /// it was handled by the process that died.
    from_line: u64,
}

/// The card this sandbox belongs to, if the sandbox is worth adopting.
///
/// The card decides, not the sandbox. A failed sandbox is deliberately *kept*
/// for inspection, so its existence proves nothing about whether a run is live;
/// and a retry leaves the previous attempt's sandbox behind under the same
/// `sandboard.item` label, so the label alone cannot say which one to watch.
/// `environment` names the current attempt, and that is the only thing that
/// can. Everything this rejects gets reaped.
fn adoptable<'a>(item: Option<&'a WorkItem>, sandbox: &str) -> Option<&'a WorkItem> {
    item.filter(|i| {
        matches!(i.state, State::Claimed | State::Running)
            && i.environment.as_deref() == Some(sandbox)
    })
}

/// Should reconcile keep this sandbox?
///
/// When the card has an `environment`, keep that name and any current or legacy
/// card-sandbox sibling (mid-create races / prior attempts). Matching
/// only `environment` reaped sandboxes mid-setup. Halt clears `environment` so
/// nothing is kept — park / Review / request-changes leave it set so caches survive.
fn should_keep_sandbox(item: Option<&WorkItem>, sandbox: &str) -> bool {
    let Some(i) = item else { return false };
    if i.state.is_terminal() {
        return false;
    }
    let Some(env) = i.environment.as_deref() else {
        return false;
    };
    let stem = crate::schema::card_sandbox_stem(i.id);
    let legacy_stem = crate::schema::legacy_card_sandbox_stem(i.id);
    env == sandbox || sandbox.starts_with(&stem) || sandbox.starts_with(&legacy_stem)
}

/// How long startup waits for the gateway before giving up on reconciling.
///
/// Generous, because the podman machine takes tens of seconds to come up and
/// sandboard and podman tend to start at the same time. Bounded, because a gateway
/// that is never coming back must not leave every Running card frozen.
const GATEWAY_GRACE: Duration = Duration::from_secs(180);
const GATEWAY_POLL: Duration = Duration::from_secs(5);

/// Reconcile, but only once the gateway can actually answer.
///
/// Skipping reconciliation is not the neutral choice it looks like. If sandboard
/// cannot enumerate sandboxes then it does not know which runs are still live,
/// and the sweeper — which starts immediately after this returns — requeues a
/// card whose agent is still working. Dispatch then claims it again and races a
/// second agent onto the branch the first one is already pushing to. That is
/// exactly the failure re-adoption exists to prevent, reached from the other
/// side, and "the podman machine stops on its own" makes it reachable.
///
/// Waiting costs nothing. Dispatch refuses to claim without a healthy gateway
/// anyway, so a sweep during an outage cannot produce work — it can only turn
/// live runs into lies about them.
async fn reconcile_once_reachable(
    board: &SharedBoard,
    timeout_secs: i64,
    grace: Duration,
) -> Vec<Adoption> {
    let deadline = std::time::Instant::now() + grace;
    let mut announced = false;
    // Re-read Settings each poll — endpoint/mTLS often land after process start.
    while !board.openshell_client().healthy().await {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            // Loud, because the board is about to be less trustworthy than
            // usual: anything that survived the restart is now invisible to us.
            tracing::error!(
                "gateway unreachable after {}s; starting without reconciling. A run that \
                 survived the restart will not be adopted and may be requeued.",
                grace.as_secs()
            );
            return Vec::new();
        }
        if !announced {
            tracing::warn!(
                "gateway unreachable; holding dispatch and the sweeper until it answers"
            );
            announced = true;
        }
        // Never sleep past the deadline — the point is a bounded wait, not a
        // wait rounded up to the poll interval.
        tokio::time::sleep(GATEWAY_POLL.min(left)).await;
    }
    // Startup: nothing is supervised yet, so empty `active` is correct — a
    // Claimed/Running card with a dead agent process should be requeued.
    // `reopen_backlog`: repair cards that old detach-as-failure left in Backlog
    // with a live sandbox (e.g. #145 after Ctrl-C).
    let os = board.openshell_client();
    reconcile(&os, board, &Active::default(), timeout_secs, true).await
}

/// Match live sandboxes back to the board, before anything else touches them.
///
/// When `active` already contains the card id, this process is mid-run (often
/// still creating the sandbox / starting the agent). Do **not** requeue those —
/// the periodic sweeper used to race dispatch and bounce cards every few
/// seconds with a misleading "sandboard restarted" reason.
///
/// `reopen_backlog` is startup-only. Reopening on every sweeper tick would
/// loop Backlog → Claimed → (dead agent) → Backlog forever.
async fn reconcile(
    os: &OpenShell,
    board: &SharedBoard,
    active: &Active,
    timeout_secs: i64,
    reopen_backlog: bool,
) -> Vec<Adoption> {
    let Ok(ours) = os.list_ours().await else {
        tracing::warn!("could not list sandboxes; skipping reconciliation");
        return Vec::new();
    };

    let mut adopted = Vec::new();
    for sb in ours {
        let Some(id) = sb.item_id() else { continue };
        let card = board.get(id);
        if !should_keep_sandbox(card.as_ref(), &sb.name) {
            tracing::info!(
                "reaping unneeded sandbox {} (card={:?} state={:?} env={:?})",
                sb.name,
                card.as_ref().map(|c| c.id),
                card.as_ref().map(|c| c.state),
                card.as_ref().and_then(|c| c.environment.as_deref()),
            );
            let _ = os.delete(&sb.name).await;
            continue;
        }

        if active.lock().contains(&id) {
            continue;
        }

        let card = if reopen_backlog {
            match prepare_for_adoption(board, card.as_ref(), &sb.name, timeout_secs) {
                Some(item) => Some(item),
                None => card,
            }
        } else {
            card
        };

        if let Some(item) = adoptable(card.as_ref(), &sb.name) {
            // Fixed deadline already passed — bounce without resetting the clock.
            if item.run_deadline_at.is_some_and(|d| chrono::Utc::now() > d) {
                tracing::warn!("#{id}: run deadline already past; requeueing");
                let _ = board.transition(
                    id,
                    State::Backlog,
                    "deadline-sweeper",
                    Some("run deadline exceeded".into()),
                );
                continue;
            }
            match adopt(os, board, item, &sb.name).await {
                Some(a) => {
                    tracing::info!(
                        "#{id}: re-attached to {} from line {}",
                        sb.name,
                        a.from_line
                    );
                    adopted.push(a);
                }
                None => {
                    // The sandbox is up but nothing is running in it — sandboard died
                    // during setup, or the agent exited and nothing cleaned up
                    // after it. There is no run to watch, so give the card back.
                    // A restart is not the card's fault, so it costs no retry
                    // budget; it just gets dispatched again from the top.
                    tracing::warn!("#{id}: {} has no live agent; requeueing", sb.name);
                    let _ = board.transition(
                        id,
                        State::Backlog,
                        "supervisor",
                        Some("sandboard restarted and found no live agent in the sandbox".into()),
                    );
                }
            }
        }
    }
    adopted
}

/// If a Backlog card still owns this sandbox (prior detach-as-failure), reopen
/// Claimed so [`adoptable`] accepts it. Returns the refreshed item when reopened.
fn prepare_for_adoption(
    board: &SharedBoard,
    item: Option<&WorkItem>,
    sandbox: &str,
    timeout_secs: i64,
) -> Option<WorkItem> {
    let item = item?;
    if item.state != State::Backlog || item.parked {
        return None;
    }
    if item.environment.as_deref() != Some(sandbox) {
        return None;
    }
    let agent_id = format!("sandbox-{}", item.id);
    match board.reopen_for_adoption(item.id, &agent_id, timeout_secs) {
        Ok(it) => {
            tracing::info!(
                "#{}: reopened from Backlog for adoption of {sandbox}",
                item.id
            );
            Some(it)
        }
        Err(e) => {
            tracing::warn!("#{}: could not reopen for adoption: {e}", item.id);
            None
        }
    }
}

/// Ask a sandbox what its agent is doing, and take over if there is one.
/// Does not reset `run_deadline_at` — that clock was set at the original claim.
async fn adopt(
    os: &OpenShell,
    board: &SharedBoard,
    item: &WorkItem,
    sandbox: &str,
) -> Option<Adoption> {
    let id = item.id;
    // A probe that hangs is a sandbox we cannot reason about, and this stack
    // fails as a hang. Treat it as "no live run" and give the card back rather
    // than watching something that may not be there.
    let out = match os
        .exec(sandbox, &probe_script(), Duration::from_secs(30))
        .await
    {
        Ok(o) if o.ok() => o,
        Ok(o) => {
            tracing::warn!("#{id}: probe of {sandbox} failed: {}", outerr(&o));
            return None;
        }
        Err(e) => {
            tracing::warn!("#{id}: could not probe {sandbox}: {e}");
            return None;
        }
    };
    let from_line = probe_of(&out.stdout)?;

    let agent_id = item
        .lease
        .as_ref()
        .map(|l| l.agent_id.clone())
        .unwrap_or_else(|| format!("sandbox-{id}"));

    // Promote Claimed → Running / refresh last_heartbeat for diagnostics.
    // Does not move the deadline — remaining time is what the original claim set.
    if let Err(e) = board.heartbeat(id, &agent_id, item.progress, 0) {
        tracing::error!("#{id}: adopted {sandbox} but could not mark it running: {e}");
    }
    board.story(
        id,
        format!("sandboard restarted; picked {sandbox} back up rather than killing it."),
    );

    Some(Adoption {
        item_id: id,
        agent_id,
        sandbox: sandbox.to_string(),
        from_line,
    })
}

/// Where a live run had got to, or `None` if nothing is running.
fn probe_of(stdout: &str) -> Option<u64> {
    if !stdout.contains(MARK_ALIVE) && !stdout.contains(MARK_EXITED) {
        return None;
    }
    let lines: u64 = stdout
        .lines()
        .find_map(|l| l.strip_prefix(MARK_LINES))?
        .trim()
        .parse()
        .ok()?;
    Some(lines + 1)
}

// ----------------------------------------------------------- the lifecycle

async fn is_sandbox_live(os: &OpenShell, name: &str) -> bool {
    match os.exec(name, "true", Duration::from_secs(10)).await {
        Ok(out) => out.ok(),
        Err(_) => false,
    }
}

fn cockpit_sandbox_name_is_present(sandboxes: &[crate::openshell::Sandbox], name: &str) -> bool {
    sandboxes.iter().any(|sandbox| sandbox.name == name)
}

async fn run_card(f: Fleet, agent_id: String, grant: ClaimGrant) -> anyhow::Result<()> {
    let board = &f.board;
    let os_owned = f.os();
    let os = &os_owned;
    let id = grant.item_id;
    // Live Settings → Agent runtime; per-card remotes from pull_request
    // (resolve_card_repo). Briefing tells the agent how to clone.
    let mut agents = board.effective_agents();
    match board.resolve_card_repo(id) {
        Ok(Some(repo)) => agents.repo = repo,
        Ok(None) => agents.repo = Default::default(),
        Err(e) => return Err(anyhow::anyhow!("{e}")),
    }
    let branch = crate::schema::card_branch_name(id);

    ensure_board_owns_run(board, id)?;

    let cfg = &agents;

    // Fail loud at claim/start before sandbox create/reuse — unknown engine ids
    // must never fall through to a silent claude default.
    let engine = grant.engine.as_deref().unwrap_or(&cfg.engine);
    crate::engine::lookup(engine).map_err(|e| anyhow::anyhow!("{e}"))?;

    let route = match crate::github_app::ensure_push_token(board, id, &agent_id, None, None).await {
        Ok(crate::github_app::EnsurePushToken::Parked) => {
            tracing::info!("#{id}: parked Needs You — GitHub App not installed for push target");
            return Ok(());
        }
        Ok(crate::github_app::EnsurePushToken::Skipped) => None,
        Ok(crate::github_app::EnsurePushToken::Ready(o)) => Some(o),
        Err(e) => anyhow::bail!("GitHub App repo routing failed: {e}"),
    };

    let existing_env = board.get(id).and_then(|i| i.environment);
    let (name, is_reused) = match existing_env {
        Some(ref env_name)
            if with_board_cancel(board, id, async { Ok(is_sandbox_live(os, env_name).await) })
                .await? =>
        {
            (env_name.clone(), true)
        }
        _ => {
            let (attempt, prev_env) = board
                .get(id)
                .map(|i| (i.run_failures + 1, i.environment.clone()))
                .unwrap_or((1, None));
            let new_name = crate::schema::card_sandbox_name(id, attempt);
            // Drop the previous attempt before renaming — reconcile used to
            // reap by exact environment match and raced the new create.
            if let Some(prev) = prev_env {
                if prev != new_name {
                    // Best-effort delete, but Halt must still free the slot.
                    if let Err(e) = with_board_cancel(board, id, async {
                        let _ = os.delete(&prev).await;
                        Ok(())
                    })
                    .await
                    {
                        if is_supervisor_cancel(&e.to_string()) {
                            return Err(e);
                        }
                    }
                }
            }
            ensure_board_owns_run(board, id)?;
            // Fresh sandbox ⇒ fresh conversation. Resume only applies when we
            // reuse the box that held the previous agy session.
            board.set_conversation_id(id, None);
            board.set_environment(id, Some(new_name.clone()));
            (new_name, false)
        }
    };

    // Keep OpenShell `github` provider stocked with a live App installation token.
    if let Err(e) = crate::github_app::ensure_github_provider(board).await {
        anyhow::bail!("GitHub App token sync failed (infrastructure): {e}");
    }

    let resolved = board.resolve_sandbox_create(id);
    let attach = board.attach_providers_for_resolved(&resolved);
    if let Err(e) = crate::api::reconcile_attached_providers(board, &attach).await {
        anyhow::bail!("OpenShell provider reconciliation failed: {e}");
    }
    let mut spec = sandbox_spec_for_card(id, &name, &resolved, &attach);
    if let Some(o) = &route {
        crate::github_app::overlay_routed_provider(&mut spec.providers, o);
    }
    if is_reused {
        if let Some(crate::github_app::RepoTokenOutcome::Ready {
            provider_name,
            routed: true,
            ..
        }) = &route
        {
            if let Err(e) =
                crate::github_app::attach_routed_provider_to_sandbox(board, &name, provider_name)
                    .await
            {
                anyhow::bail!("GitHub App routed token attach failed: {e}");
            }
        }
    }

    let result = run_inside(
        board, os, cfg, &agent_id, &grant, &name, &branch, &spec, is_reused,
    )
    .await;
    finalize(os, id, &name, &result).await;
    result
}

/// Build the OpenShell create spec from a resolved profile (or YAML fallback).
fn sandbox_spec_for_card(
    id: ItemId,
    name: &str,
    resolved: &crate::model::ResolvedSandboxCreate,
    attach_providers: &[String],
) -> SandboxSpec {
    let engine = resolved
        .engine
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("");
    SandboxSpec {
        name: name.to_string(),
        from: resolved.image.clone(),
        providers: attach_providers.to_vec(),
        policy: Some(resolved.policy.clone()),
        env: sandbox_create_env(engine, &resolved.env),
        labels: vec![(LABEL_ITEM.to_string(), id.to_string())],
        cpu: resolved.cpu.clone(),
        memory: resolved.memory.clone(),
    }
}

/// Take over a run this process did not start: join it at the watch step, with
/// the setup already done and the briefing already delivered.
async fn adopt_card(f: Fleet, a: Adoption) -> anyhow::Result<()> {
    let board = &f.board;
    let os_owned = f.os();
    let os = &os_owned;
    let id = a.item_id;
    let mut agents = board.effective_agents();
    match board.resolve_card_repo(id) {
        Ok(Some(repo)) => agents.repo = repo,
        Ok(None) => agents.repo = Default::default(),
        Err(e) => return Err(anyhow::anyhow!("{e}")),
    }
    let branch = crate::schema::card_branch_name(id);
    let cfg = &agents;
    match crate::github_app::ensure_push_token(
        board,
        id,
        &a.agent_id,
        Some(&a.sandbox),
        None,
    )
    .await
    {
        Ok(crate::github_app::EnsurePushToken::Parked) => {
            tracing::info!(
                "#{id}: adopted sandbox parked Needs You — App not installed for push target"
            );
            return Ok(());
        }
        Ok(_) => {}
        Err(e) => anyhow::bail!("GitHub App repo routing failed: {e}"),
    }
    let result = async {
        let run = watch_agent(board, os, cfg, &a.agent_id, id, &a.sandbox, a.from_line).await?;
        finish(board, os, cfg, &a.agent_id, id, &a.sandbox, &branch, &run).await
    }
    .await;
    finalize(os, id, &a.sandbox, &result).await;
    result
}

/// Dispose of the agent in the sandbox. Deliberately no `Board` here: the card keeps what
/// the run produced, including the sandbox name, and taking the board would
/// make it easy to clear that again on the way out.
async fn finalize(os: &OpenShell, id: ItemId, name: &str, result: &anyhow::Result<()>) {
    match result {
        Ok(_) => {
            stop_agent(os, name).await;
            tracing::info!("#{id}: keeping sandbox {name} for review/reclaim");
        }
        Err(e) if is_supervisor_detach(&e.to_string()) => {
            // Leave the setsid agent alone — restart will adopt it.
            tracing::info!("#{id}: detaching from {name} without stopping the agent: {e}");
        }
        Err(e) if is_supervisor_cancel(&e.to_string()) => {
            stop_agent(os, name).await;
            tracing::info!("#{id}: run cancelled; stopped agent in {name}");
        }
        Err(e) => {
            stop_agent(os, name).await;
            tracing::error!("#{id}: keeping sandbox {name} for inspection: {e}");
        }
    }
}

async fn exec_with_infra_retry(
    os: &OpenShell,
    name: &str,
    script: &str,
    timeout: Duration,
    what: &str,
) -> anyhow::Result<Output> {
    let mut last = String::new();
    for attempt in 1..=5 {
        match os.exec(name, script, timeout).await {
            Ok(out) if out.ok() => return Ok(out),
            Ok(out) => {
                last = format!("{what} failed: {}", outerr(&out));
                if !is_infrastructure(&last) {
                    anyhow::bail!("{last}");
                }
            }
            Err(e) => {
                last = format!("{what} failed: {e}");
                if !is_infrastructure(&last) {
                    anyhow::bail!("{last}");
                }
            }
        }
        tracing::warn!("{what} on {name} attempt {attempt}/5 failed (retrying): {last}");
        tokio::time::sleep(Duration::from_millis(500 * attempt as u64)).await;
        let _ = wait_until_sandbox_ready(os, name).await;
    }
    anyhow::bail!("{last}")
}

/// Create returns as soon as the bootstrap command exits; the supervisor relay
/// can still be bouncing. Poll until `list` reports Ready before upload/exec.
pub(crate) async fn wait_until_sandbox_ready(os: &OpenShell, name: &str) -> anyhow::Result<()> {
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        match os.list().await {
            Ok(list) => {
                if let Some(sb) = list.iter().find(|s| s.name == name) {
                    let phase = sb.phase.as_deref().unwrap_or("");
                    if phase.eq_ignore_ascii_case("Ready") {
                        // Brief settle — immediate upload right after Ready still
                        // flakes with "sandbox is not ready" / relay closed.
                        tokio::time::sleep(Duration::from_millis(750)).await;
                        return Ok(());
                    }
                    if phase.eq_ignore_ascii_case("Deleting")
                        || phase.eq_ignore_ascii_case("Failed")
                        || phase.eq_ignore_ascii_case("Error")
                    {
                        anyhow::bail!("sandbox {name} entered phase {phase} before setup finished");
                    }
                }
            }
            Err(e) => tracing::warn!("waiting for {name} ready: list failed: {e}"),
        }
        if std::time::Instant::now() >= deadline {
            anyhow::bail!("sandbox {name} not Ready within 60s");
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_inside(
    board: &SharedBoard,
    os: &OpenShell,
    cfg: &AgentConfig,
    agent_id: &str,
    grant: &ClaimGrant,
    name: &str,
    branch: &str,
    spec: &SandboxSpec,
    is_reused: bool,
) -> anyhow::Result<()> {
    let id = grant.item_id;
    let short = Duration::from_secs(180);
    board.clear_agent_logs(id);

    // Setup emits no agent output. Heartbeats here promote Claimed → Running
    // and record setup milestones; they do not extend the run deadline.
    // Also refuses to heartbeat a Halted card — setup must not outlive the
    // board's claim.
    let beat = |p: f32| -> anyhow::Result<()> {
        ensure_board_owns_run(board, id)?;
        let _ = board.heartbeat(id, agent_id, p, 0);
        Ok(())
    };

    // Workdir contract for `run_inside`:
    // - Cold start (`!is_reused`): clear `/sandbox/repo` so the agent clones
    //   into an empty tree. Supervisor never clones.
    // - Reuse (`is_reused`): keep the live sandbox workdir. Park unpark resume
    //   and Needs You answer reclaim both set this flag the same way. If a
    //   checkout exists, refresh in place; otherwise ensure the directory
    //   without wiping prior contents or caches. Still never supervisor-clone.
    let branch_state = if !is_reused {
        if let Err(e) = with_board_cancel(board, id, async {
            let _ = os.delete(&spec.name).await;
            Ok(())
        })
        .await
        {
            if is_supervisor_cancel(&e.to_string()) {
                return Err(e);
            }
        }
        with_board_cancel(board, id, async {
            os.create(spec).await.map_err(Into::into)
        })
        .await?;
        with_board_cancel(board, id, wait_until_sandbox_ready(os, name)).await?;
        beat(0.01)?;
        // Catalog MCP (stdio/HTTP remote) + empty Claude mcp-config stub.
        let resolved_mcp = board.resolve_sandbox_create(id);
        if let Err(e) = with_board_cancel(board, id, async {
            crate::cockpit_mcp::provision_worker_mcp(board, os, name, &resolved_mcp)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))
        })
        .await
        {
            if is_supervisor_cancel(&e.to_string()) {
                return Err(e);
            }
            tracing::warn!("#{id}: worker MCP inject failed (continuing): {e}");
        }

        let _ =
            with_board_cancel(board, id, ensure_report_schema_in_sandbox(os, name, short)).await;

        // Agent owns the clone. Supervisor only clears `/sandbox/repo`.
        let _ = with_board_cancel(
            board,
            id,
            exec_with_infra_retry(os, name, &empty_workdir_script(), short, "workdir"),
        )
        .await?;
        beat(0.03)?;
        BranchState::Fresh
    } else {
        beat(0.01)?;

        let _ =
            with_board_cancel(board, id, ensure_report_schema_in_sandbox(os, name, short)).await;

        // Reuse: refresh an existing checkout in place; otherwise ensure the
        // directory without wiping. Never empty_workdir_script here.
        let has_repo = with_board_cancel(board, id, async {
            os.exec(name, &format!("test -d {WORKDIR}/.git"), short)
                .await
                .map_err(Into::into)
        })
        .await
        .map(|o| o.ok())
        .unwrap_or(false);
        let branch_state = if has_repo {
            let refresh = with_board_cancel(
                board,
                id,
                exec_with_infra_retry(os, name, &refresh_script(cfg, branch), short, "refresh"),
            )
            .await?;
            beat(0.03)?;
            branch_state_of(&refresh.stdout)
        } else {
            let _ = with_board_cancel(
                board,
                id,
                exec_with_infra_retry(os, name, &ensure_workdir_script(), short, "workdir"),
            )
            .await?;
            beat(0.03)?;
            BranchState::Fresh
        };
        branch_state
    };

    // ---- the agent -------------------------------------------------------

    let engine = grant.engine.as_deref().unwrap_or(&cfg.engine);
    // Fail loud before sandbox launch work compounds a misconfigured profile.
    crate::engine::lookup(engine)?;
    let conversation_id = board.get(id).and_then(|i| i.conversation_id.clone());
    // Conversation resume flag is independent of briefing shape: a conflicted
    // branch still needs the cold CONFLICTS briefing even when a resumable
    // engine can continue the same session.
    let resume = is_reused && crate::engine::supports_resume(engine) && conversation_id.is_some();
    let briefing_text = choose_briefing(grant, branch_state, branch, &cfg.repo, resume);
    if resume {
        board.story(
            id,
            format!(
                "resuming {engine} conversation {}",
                conversation_id.as_deref().unwrap_or("?")
            ),
        );
    }
    if crate::engine::pre_start_auth(engine)? == crate::engine::PreStartAuth::Agy {
        with_board_cancel(board, id, setup_agy_auth(os, name, board)).await?;
    }
    let script = start_script(
        cfg,
        &briefing_text,
        engine,
        conversation_id.as_deref().filter(|_| resume),
        grant.model.as_deref(),
    )?;
    let start = with_board_cancel(board, id, async {
        os.exec(name, &script, short).await.map_err(Into::into)
    })
    .await?;
    anyhow::ensure!(start.ok(), "agent did not start: {}", outerr(&start));
    beat(0.04)?;

    // From the top of the log: this run is ours from its first line. An adopted
    // run joins here instead, further in.
    let run = watch_agent(board, os, cfg, agent_id, id, name, 1).await?;
    finish(board, os, cfg, agent_id, id, name, branch, &run).await
}

/// Watch a detached agent to completion and hand back its exit status.
///
/// `from_line` makes watching *resumable*: a supervisor that starts halfway
/// through a run skips the lines a previous one already streamed.
///
/// The run deadline is fixed at claim — stream updates must not push it out.
/// If the board leaves Claimed/Running (deadline sweep or Halt), the watch
/// ends immediately so `in_flight` frees and the agent is stopped.
#[allow(clippy::too_many_arguments)]
async fn watch_agent(
    board: &SharedBoard,
    os: &OpenShell,
    cfg: &AgentConfig,
    agent_id: &str,
    id: ItemId,
    name: &str,
    from_line: u64,
) -> anyhow::Result<Output> {
    // The agent carries its own deadline inside the sandbox; this one only has
    // to outlast it, so a hung *follower* still fails rather than waiting.
    let timeout = Duration::from_secs(cfg.agent_timeout_secs) + Duration::from_secs(120);

    let board2 = board.clone();
    let agent_owned = agent_id.to_string();
    // Heartbeat at most every 2s — Claimed→Running still happens on the first
    // due tick. Per-line heartbeats + SSE Upserts were locking the board hard
    // enough that the card drawer hung on detail fetches.
    let last_hb = std::sync::Mutex::new(None::<std::time::Instant>);

    let follow = follow_script(from_line);
    let stream = os.exec_streaming(name, &follow, timeout, move |line| {
        if let Some(cid) = parse_conversation_id(line) {
            board2.set_conversation_id(id, Some(cid));
        }

        // Buffer live agent output lines for UI stream view.
        board2.append_agent_log(id, line.to_string());

        let due = {
            let mut slot = last_hb.lock().unwrap_or_else(|e| e.into_inner());
            match *slot {
                Some(t) if t.elapsed() < Duration::from_secs(2) => false,
                _ => {
                    *slot = Some(std::time::Instant::now());
                    true
                }
            }
        };
        if !due {
            return;
        }

        // Liveness heartbeat — does not extend run_deadline_at.
        if let Some(item) = board2.get(id) {
            if board_still_owns_run(item.state) {
                let _ = board2.heartbeat(id, &agent_owned, item.progress, 0);
            }
        }
    });
    tokio::pin!(stream);

    let mut poll = tokio::time::interval(watch_board_poll());
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    poll.tick().await;

    loop {
        tokio::select! {
            res = &mut stream => return res.map_err(Into::into),
            _ = poll.tick() => {
                ensure_board_owns_run(board, id)?;
            }
        }
    }
}

// ---------------------------------------------------- verdict file protocol

#[derive(Debug, serde::Deserialize)]
struct EscalateFile {
    question: String,
    options: Vec<RawEscalationOption>,
    #[serde(default)]
    recommended: usize,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct RawEscalationOptionFields {
    /// Canonical keys are `label` / `detail`. Agents often write `title` / `body`.
    #[serde(alias = "title")]
    label: String,
    #[serde(default, alias = "body", alias = "description")]
    detail: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(untagged)]
enum RawEscalationOption {
    Struct(RawEscalationOptionFields),
    String(String),
}

impl RawEscalationOption {
    fn into_escalation_option(self) -> crate::model::EscalationOption {
        match self {
            RawEscalationOption::Struct(RawEscalationOptionFields { label, detail }) => {
                crate::model::EscalationOption {
                    detail: detail.unwrap_or_else(|| label.clone()),
                    label,
                }
            }
            RawEscalationOption::String(s) => crate::model::EscalationOption {
                label: s.clone(),
                detail: s,
            },
        }
    }
}

#[derive(Debug, serde::Deserialize)]
struct SplitFile {
    children: Vec<RawSplitChild>,
}

#[derive(Debug, serde::Deserialize)]
struct RawSplitChild {
    title: String,
    intent: String,
    #[serde(
        default,
        rename = "definition_of_done",
        alias = "dod",
        alias = "definitionOfDone"
    )]
    dod: Option<String>,
    #[serde(default)]
    key: Option<String>,
    #[serde(default)]
    blocked_by_keys: Vec<String>,
    /// Optional per-child remotes; Approve defaults from the splitting parent Task.
    #[serde(default)]
    repo: Option<crate::schema::RepoConfig>,
}

#[derive(Debug, serde::Deserialize)]
struct ReportEnd {
    #[serde(default)]
    repo: Option<String>,
    #[serde(default, rename = "ref")]
    git_ref: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
struct ReportFile {
    #[serde(default)]
    added: u32,
    #[serde(default)]
    removed: u32,
    #[serde(default)]
    gates: Vec<String>,
    /// Preferred: `url`. `pr_url` accepted as legacy alias.
    #[serde(default, alias = "pr_url")]
    url: Option<String>,
    #[serde(default)]
    base: Option<ReportEnd>,
    #[serde(default)]
    head: Option<ReportEnd>,
}

fn report_end_to_model(end: &ReportEnd) -> Option<crate::model::PullRequestEnd> {
    let repo = end.repo.as_deref()?.trim();
    let git_ref = end.git_ref.as_deref().unwrap_or("main").trim();
    if repo.is_empty() {
        return None;
    }
    Some(crate::model::PullRequestEnd::new(repo, git_ref))
}

fn report_to_pull_request(rep: &ReportFile) -> Option<crate::model::PullRequest> {
    let url = rep
        .url
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())?
        .to_string();
    let base = rep.base.as_ref().and_then(report_end_to_model);
    let head = rep.head.as_ref().and_then(report_end_to_model);
    Some(crate::model::PullRequest {
        url,
        base,
        head,
        ..Default::default()
    })
}

/// Sidecar written by Initial plan agents — becomes Project Plan awaiting approval.
#[derive(Debug, serde::Deserialize)]
struct PlanFile {
    #[serde(default)]
    summary: Option<String>,
    #[serde(default, alias = "children")]
    tasks: Vec<RawPlanTask>,
}

#[derive(Debug, serde::Deserialize)]
struct RawPlanTask {
    #[serde(default)]
    key: Option<String>,
    title: String,
    intent: String,
    #[serde(
        default,
        rename = "definition_of_done",
        alias = "dod",
        alias = "definitionOfDone"
    )]
    dod: Option<String>,
    #[serde(default)]
    blocked_by_keys: Vec<String>,
    #[serde(default)]
    capability: Option<String>,
    /// Optional wire field; clone targets are taken from intent/DoD.
    #[serde(default)]
    repo: Option<crate::schema::RepoConfig>,
}

fn plan_file_to_specs(plan: PlanFile) -> Result<(String, Vec<crate::model::PlanTaskSpec>), String> {
    if plan.tasks.is_empty() {
        return Err("plan.json has no tasks".into());
    }
    let summary = plan
        .summary
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "Initial plan".into());
    let mut specs = Vec::with_capacity(plan.tasks.len());
    for (idx, t) in plan.tasks.into_iter().enumerate() {
        let key = t
            .key
            .map(|k| k.trim().to_string())
            .filter(|k| !k.is_empty())
            .unwrap_or_else(|| format!("t{}", idx + 1));
        let dod = t
            .dod
            .filter(|d| !d.trim().is_empty())
            .unwrap_or_else(|| format!("{} completed.", t.title));
        specs.push(crate::model::PlanTaskSpec {
            key,
            title: t.title,
            intent: t.intent,
            definition_of_done: dod,
            blocked_by_keys: t.blocked_by_keys,
            capability: t.capability,
            repo: t.repo,
            item_id: None,
        });
    }
    Ok((summary, specs))
}

/// Download `plan.json` and store it as the Initial plan proposal.
/// Returns `Ok(true)` if the card was escalated instead of accepting the plan.
async fn apply_initial_plan_sidecar(
    board: &SharedBoard,
    os: &OpenShell,
    agent_id: &str,
    id: ItemId,
    name: &str,
    plan_remote: &str,
) -> anyhow::Result<bool> {
    let escalate_missing = |detail: String| -> anyhow::Result<bool> {
        board
            .escalate(
                id,
                agent_id,
                detail,
                vec![
                    crate::model::EscalationOption {
                        label: "Write plan.json".into(),
                        detail: format!(
                            "Write {VERDICT_DIR}/plan.json (tasks with key, intent, DoD naming \
                             the clone repo, blocked_by_keys). No PR. Then exit."
                        ),
                    },
                    crate::model::EscalationOption {
                        label: "Propose Plan on the board".into(),
                        detail: "Use propose_breakdown (writes the Initial plan proposal), then Approve that card.".into(),
                    },
                ],
                0,
            )
            .map_err(|e| anyhow::anyhow!("initial-plan plan.json escalate: {e}"))?;
        Ok(true)
    };

    let tmp_dir = scratch_dir("plan", id);
    let _ = std::fs::create_dir_all(&tmp_dir);
    let local_plan = tmp_dir.join("plan.json");
    let local_plan_str = local_plan.to_string_lossy().to_string();

    if let Err(e) = os.download(name, plan_remote, &local_plan_str).await {
        let _ = std::fs::remove_dir_all(&tmp_dir);
        tracing::warn!("#{id}: Initial plan missing plan.json ({e})");
        return escalate_missing(format!(
            "Initial plan must write {VERDICT_DIR}/plan.json (proposed Tasks; each names \
             the repo to clone in intent/DoD). Approve on that Review card creates the Tasks."
        ));
    }

    let content = match std::fs::read_to_string(&local_plan) {
        Ok(c) => c,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&tmp_dir);
            return escalate_missing(format!("could not read downloaded plan.json: {e}"));
        }
    };
    let _ = std::fs::remove_dir_all(&tmp_dir);

    let plan: PlanFile = match serde_json::from_str(&content) {
        Ok(p) => p,
        Err(e) => {
            return escalate_missing(format!("invalid plan.json: {e}"));
        }
    };
    let (summary, specs) = match plan_file_to_specs(plan) {
        Ok(v) => v,
        Err(e) => return escalate_missing(e),
    };

    match board.set_proposal(
        id,
        crate::model::TaskProposal {
            summary,
            tasks: specs,
        },
    ) {
        Ok(item) => {
            let n = item.proposal.as_ref().map(|p| p.tasks.len()).unwrap_or(0);
            tracing::info!("#{id}: stored TaskProposal on Initial plan ({n} tasks)");
            Ok(false)
        }
        Err(e) => escalate_missing(format!("set_proposal refused: {e}")),
    }
}

fn probe_verdict_script() -> String {
    // Prefer /sandbox/.sandboard (writable HOME). Also probe /work and /tmp;
    // /tmp often cannot be downloaded by OpenShell.
    // Initial plan completes when plan.json is present.
    r#"for dir in /sandbox/.sandboard /work/.sandboard /tmp/.sandboard; do
  if [ -f "$dir/escalate.json" ]; then
    echo "escalate:$dir/escalate.json"
    exit 0
  elif [ -f "$dir/split.json" ]; then
    echo "split:$dir/split.json"
    exit 0
  elif [ -f "$dir/plan.json" ]; then
    echo "plan:$dir/plan.json"
    exit 0
  elif [ -f "$dir/report.json" ]; then
    echo "report:$dir/report.json"
    exit 0
  fi
done"#
        .to_string()
}

async fn process_verdict(
    board: &SharedBoard,
    os: &OpenShell,
    cfg: &AgentConfig,
    agent_id: &str,
    id: ItemId,
    name: &str,
    branch: &str,
) -> anyhow::Result<bool> {
    let short = Duration::from_secs(30);
    let probe = match os.exec(name, &probe_verdict_script(), short).await {
        Ok(out) if out.ok() => out.stdout,
        _ => return Ok(false),
    };

    let line = probe.trim();
    if line.is_empty() {
        return Ok(false);
    }

    let Some((vtype, remote_path)) = line.split_once(':') else {
        return Ok(false);
    };

    let tmp_dir = scratch_dir("verdict", id);
    let _ = std::fs::create_dir_all(&tmp_dir);
    let local_dest = tmp_dir.join(format!("{vtype}.json"));
    let local_dest_str = local_dest.to_string_lossy().to_string();

    if let Err(e) = os.download(name, remote_path, &local_dest_str).await {
        let _ = std::fs::remove_dir_all(&tmp_dir);
        return Err(anyhow::anyhow!(
            "could not download verdict file {remote_path}: {e}"
        ));
    }

    let content = match std::fs::read_to_string(&local_dest) {
        Ok(c) => c,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&tmp_dir);
            return Err(anyhow::anyhow!(
                "could not read downloaded verdict file: {e}"
            ));
        }
    };
    let _ = std::fs::remove_dir_all(&tmp_dir);

    match vtype {
        "escalate" => {
            let esc: EscalateFile = serde_json::from_str(&content)
                .map_err(|e| anyhow::anyhow!("invalid escalate.json: {e}"))?;
            let options = esc
                .options
                .into_iter()
                .map(|o| o.into_escalation_option())
                .collect();
            board
                .escalate(id, agent_id, esc.question, options, esc.recommended)
                .map_err(|e| anyhow::anyhow!("escalate: {e}"))?;
            tracing::info!("#{id}: agent escalated via verdict file");
            Ok(true)
        }
        "split" => {
            if board.get(id).is_some_and(|i| i.is_initial_plan_task()) {
                tracing::warn!("#{id}: Initial plan tried to finish via split; refusing");
                board
                    .escalate(
                        id,
                        agent_id,
                        format!(
                            "Initial plan finishes by writing {VERDICT_DIR}/plan.json \
                             (moves to Review). Approve materializes sibling Tasks from that proposal."
                        ),
                        vec![
                            crate::model::EscalationOption {
                                label: "Write plan.json".into(),
                                detail: format!(
                                    "Write {VERDICT_DIR}/plan.json (tasks name clone repos in intent/DoD)."
                                ),
                            },
                            crate::model::EscalationOption {
                                label: "Propose Plan on the board".into(),
                                detail: "Use propose_breakdown on the Project, then Approve.".into(),
                            },
                        ],
                        0,
                    )
                    .map_err(|e| anyhow::anyhow!("initial-plan split refuse: {e}"))?;
                return Ok(true);
            }

            let split: SplitFile = serde_json::from_str(&content)
                .map_err(|e| anyhow::anyhow!("invalid split.json: {e}"))?;

            // Check whether a PR already exists for the card (pr_url set or PR detected)
            let existing_pr = if let Some(url) = board
                .get(id)
                .and_then(|i| i.pr_url().map(|s| s.to_string()))
                .filter(|s| !s.trim().is_empty())
            {
                Some(url)
            } else if let Ok(out) = os.exec(name, &pr_lookup_script(cfg, branch), short).await {
                if out.ok() {
                    parse_pr_url(&out.stdout)
                } else {
                    None
                }
            } else {
                None
            };

            if let Some(pr_url) = existing_pr {
                board.set_pr_url(id, Some(pr_url.clone()));
            }

            let children = split
                .children
                .into_iter()
                .map(|c| {
                    let dod = c.dod.unwrap_or_else(|| format!("{} completed.", c.title));
                    let mut spec = crate::model::SplitChildSpec::new(c.title, c.intent, dod);
                    spec.key = c.key;
                    spec.blocked_by_keys = c.blocked_by_keys;
                    spec.repo = c.repo;
                    spec
                })
                .collect();
            match board.propose_split(id, agent_id, children, 5) {
                Ok(card) => {
                    let n = card.proposal.as_ref().map(|p| p.tasks.len()).unwrap_or(0);
                    tracing::info!("#{id}: agent proposed {n} sibling Tasks — Review");
                    Ok(true)
                }
                Err(e) => {
                    tracing::warn!("#{id}: propose_split refused: {e}");
                    let state = board.get(id).map(|i| i.state);
                    if state != Some(State::NeedsHuman) {
                        board
                            .escalate(
                                id,
                                agent_id,
                                format!("Agent requested a split, but it was refused by governor: {e}"),
                                vec![
                                    crate::model::EscalationOption {
                                        label: "Decompose manually".into(),
                                        detail: "Add sibling Tasks under the Project with the right deps.".into(),
                                    },
                                    crate::model::EscalationOption {
                                        label: "Revise scope".into(),
                                        detail: "Narrow the definition of done so the work fits in one card.".into(),
                                    },
                                ],
                                0,
                            )
                            .map_err(|esc_err| anyhow::anyhow!("split refused ({e}) and failed to escalate: {esc_err}"))?;
                    }
                    Ok(true)
                }
            }
        }
        "plan" => {
            if !board.get(id).is_some_and(|i| i.is_initial_plan_task()) {
                board
                    .escalate(
                        id,
                        agent_id,
                        "plan.json is only for Initial plan cards. Impl cards use report.json \
                         (with a PR) or split.json."
                            .into(),
                        vec![
                            crate::model::EscalationOption {
                                label: "Write report.json".into(),
                                detail: "Open/update the PR, then write report.json with url/base/head."
                                    .into(),
                            },
                            crate::model::EscalationOption {
                                label: "Write split.json".into(),
                                detail: "If the work is too large, propose sibling Tasks via split.json."
                                    .into(),
                            },
                        ],
                        0,
                    )
                    .map_err(|e| anyhow::anyhow!("plan.json on non-initial: {e}"))?;
                return Ok(true);
            }
            if apply_initial_plan_sidecar(board, os, agent_id, id, name, remote_path).await? {
                return Ok(true);
            }
            board.report(id, agent_id, 0, 0, vec!["plan.json".into()])?;
            tracing::info!("#{id}: Initial plan accepted via plan.json");
            Ok(true)
        }
        "report" => {
            let rep: ReportFile = serde_json::from_str(&content)
                .map_err(|e| anyhow::anyhow!("invalid report.json: {e}"))?;

            // Stash PR early so an escalate on missing plan.json still surfaces it.
            if let Some(pr) = report_to_pull_request(&rep) {
                let _ = board.report_pull_request(id, pr);
            }

            let is_initial = board.get(id).is_some_and(|i| i.is_initial_plan_task());
            // Initial plan may also ship plan.json beside report.json.
            if is_initial {
                let plan_remote = {
                    let p = std::path::Path::new(remote_path);
                    p.parent()
                        .unwrap_or_else(|| std::path::Path::new("."))
                        .join("plan.json")
                        .to_string_lossy()
                        .into_owned()
                };
                if apply_initial_plan_sidecar(board, os, agent_id, id, name, &plan_remote).await? {
                    return Ok(true);
                }
                board.report(
                    id,
                    agent_id,
                    rep.added,
                    rep.removed,
                    if rep.gates.is_empty() {
                        vec!["plan.json".into()]
                    } else {
                        rep.gates
                    },
                )?;
                tracing::info!("#{id}: Initial plan accepted via report+plan.json");
                return Ok(true);
            }
            // Impl cards: proposal and publish are mutually exclusive.
            if board
                .get(id)
                .is_some_and(|i| i.proposal.as_ref().is_some_and(|p| !p.tasks.is_empty()))
            {
                board
                    .escalate(
                        id,
                        agent_id,
                        "This card already has a Task proposal in Review. Finish via Approve \
                         / PR merge (creates siblings) or request_changes — do not also \
                         report a second PR."
                            .into(),
                        vec![
                            crate::model::EscalationOption {
                                label: "Approve / merge the proposal".into(),
                                detail: "Approve acknowledges; PR merge (or Approve without a PR) \
                                         creates the sibling Tasks."
                                    .into(),
                            },
                            crate::model::EscalationOption {
                                label: "Request changes".into(),
                                detail: "Clear the proposal and return the card to Backlog.".into(),
                            },
                        ],
                        0,
                    )
                    .map_err(|e| anyhow::anyhow!("proposal/report exclusivity: {e}"))?;
                return Ok(true);
            }

            let mut pr = report_to_pull_request(&rep);
            if pr.as_ref().is_none_or(|p| !p.has_forge_ends()) {
                let url = pr
                    .as_ref()
                    .and_then(|p| p.url_str().map(|s| s.to_string()))
                    .or_else(|| {
                        board
                            .get(id)
                            .and_then(|i| i.pr_url().map(|s| s.to_string()))
                    });
                if let Some(url) = url {
                    if let Ok(out) = os.exec(name, &pr_view_binding_script(&url), short).await {
                        if let Some(filled) = parse_pr_binding_line(&out.stdout) {
                            pr = Some(filled);
                        }
                    }
                }
            }
            if pr.as_ref().is_none_or(|p| p.url_str().is_none()) {
                let looked = os.exec(name, &pr_lookup_script(cfg, branch), short).await?;
                let url = parse_pr_url(&looked.stdout).ok_or_else(|| {
                    anyhow::anyhow!("agent finished but opened no PR for {branch}")
                })?;
                if let Ok(out) = os.exec(name, &pr_view_binding_script(&url), short).await {
                    pr = parse_pr_binding_line(&out.stdout)
                        .or_else(|| Some(crate::model::PullRequest::from_url(url.clone())));
                } else {
                    pr = Some(crate::model::PullRequest::from_url(url));
                }
            }
            let pr =
                pr.ok_or_else(|| anyhow::anyhow!("agent finished but opened no PR for {branch}"))?;
            let pr_url = pr.url.clone();
            board
                .report_pull_request(id, pr.clone())
                .map_err(|e| anyhow::anyhow!("report_pull_request: {e}"))?;
            attach_token_for_reported_pr(board, id, &pr).await;
            let mut finish_cfg = cfg.clone();
            if let Ok(Some(repo)) = board.resolve_card_repo(id) {
                finish_cfg.repo = repo;
            }
            let gates = if rep.gates.is_empty() {
                vec!["agent-reported".into()]
            } else {
                rep.gates
            };
            let (added, removed) = if rep.added == 0 && rep.removed == 0 {
                if let Ok(out) = os.exec(name, &diffstat_script(&finish_cfg), short).await {
                    if out.ok() {
                        parse_diffstat(&out.stdout)
                    } else {
                        (rep.added, rep.removed)
                    }
                } else {
                    (rep.added, rep.removed)
                }
            } else {
                (rep.added, rep.removed)
            };
            // Hollow Review after a conflict bounce: refuse report while GitHub
            // still says CONFLICTING. UNKNOWN/null is not a hard fail.
            let mergeable = match os
                .exec(name, &pr_lookup_script(&finish_cfg, branch), short)
                .await
            {
                Ok(out) if out.ok() => parse_pr_mergeable(&out.stdout),
                _ => PrMergeable::Unknown,
            };
            if mergeable == PrMergeable::Conflicting {
                board
                    .release_with_reason(id, agent_id, Some(CONFLICTING_PR_BOUNCE_REASON))
                    .map_err(|e| anyhow::anyhow!("release conflicting PR: {e}"))?;
                tracing::info!(
                    "#{id}: refused report — PR mergeable CONFLICTING; returned to Backlog"
                );
                return Ok(true);
            }
            board.report(id, agent_id, added, removed, gates)?;
            tracing::info!("#{id}: agent reported via verdict file; pr={pr_url}");
            Ok(true)
        }
        _ => Ok(false),
    }
}

/// Settle a finished run: check it succeeded and put the PR on the board.
#[allow(clippy::too_many_arguments)]
async fn finish(
    board: &SharedBoard,
    os: &OpenShell,
    cfg: &AgentConfig,
    agent_id: &str,
    id: ItemId,
    name: &str,
    branch: &str,
    run: &Output,
) -> anyhow::Result<()> {
    let short = Duration::from_secs(180);

    if process_verdict(board, os, cfg, agent_id, id, name, branch).await? {
        return Ok(());
    }

    if board.get(id).is_some_and(|item| item.is_initial_plan_task()) {
        let detail = if run.ok() {
            format!(
                "Initial plan finished without writing {VERDICT_DIR}/plan.json."
            )
        } else {
            format!(
                "Initial plan exited before writing {VERDICT_DIR}/plan.json."
            )
        };
        board
            .escalate(
                id,
                agent_id,
                detail,
                vec![
                    crate::model::EscalationOption {
                        label: "Write plan.json".into(),
                        detail: format!(
                            "Write {VERDICT_DIR}/plan.json with the proposed Tasks, then exit."
                        ),
                    },
                    crate::model::EscalationOption {
                        label: "Propose Plan on the board".into(),
                        detail: "Use propose_breakdown on the Project, then Approve that card."
                            .into(),
                    },
                ],
                0,
            )
            .map_err(|e| anyhow::anyhow!("Initial plan escalation: {e}"))?;
        tracing::warn!("#{id}: Initial plan produced no verdict file");
        return Ok(());
    }

    anyhow::ensure!(run.ok(), "agent exited {}: {}", run.code, outerr(run));

    // ---- verify what the agent published ---------------------------------
    //
    // The agent pushes and opens the PR; the supervisor only asks GitHub what
    // happened. Containment is the owner-only default-branch ruleset plus human
    // merge — not a supervisor shell that re-implements `gh`.
    let pr = os.exec(name, &pr_lookup_script(cfg, branch), short).await?;
    anyhow::ensure!(
        pr.ok(),
        "could not ask GitHub about the PR: {}",
        outerr(&pr)
    );
    let url = parse_pr_url(&pr.stdout)
        // A Review card with no PR is a card you cannot action, so this is a
        // failure rather than a quietly empty field.
        .ok_or_else(|| anyhow::anyhow!("agent finished but opened no PR for {branch}"))?;
    let recorded = crate::model::PullRequest::from_url(url.clone());
    board
        .report_pull_request(id, recorded.clone())
        .map_err(|e| anyhow::anyhow!("report_pull_request: {e}"))?;
    attach_token_for_reported_pr(board, id, &recorded).await;

    // Refuse hollow Review while GitHub still reports CONFLICTING. UNKNOWN
    // (and null/missing) must not hard-fail — the API is eventually consistent.
    if parse_pr_mergeable(&pr.stdout) == PrMergeable::Conflicting {
        board.release_with_reason(id, agent_id, Some(CONFLICTING_PR_BOUNCE_REASON))?;
        tracing::info!("#{id}: refused report — PR mergeable CONFLICTING; returned to Backlog");
        return Ok(());
    }

    // Mechanical checks are CI on the PR. sandboard only records the PR + diffstat.
    let (added, removed) = match os.exec(name, &diffstat_script(cfg), short).await {
        Ok(out) if out.ok() => parse_diffstat(&out.stdout),
        _ => (0, 0),
    };
    board.report(id, agent_id, added, removed, vec!["ci-on-pr".into()])?;
    tracing::info!("#{id} reported; pr={url}");
    Ok(())
}

/// Attach a cache-routed GH_TOKEN after a PR is recorded. Does not park:
/// the agent already opened the PR. Uncovered clone targets park at dispatch
/// / `report_pull_request` instead.
async fn attach_token_for_reported_pr(
    board: &SharedBoard,
    id: ItemId,
    pr: &crate::model::PullRequest,
) {
    let Some(repo) = pr.push_owner_repo() else {
        return;
    };
    let sandbox = board.get(id).and_then(|i| i.environment);
    match crate::github_app::sync_sandbox_token_for_repo(board, sandbox.as_deref(), &repo).await {
        Ok(crate::github_app::RepoTokenOutcome::Ready { installation_id, .. }) => {
            tracing::info!("#{id}: attached GH_TOKEN for {repo} via installation {installation_id}");
        }
        Ok(crate::github_app::RepoTokenOutcome::Uncovered { .. }) => {
            tracing::warn!(
                "#{id}: {repo} is not in the GitHub App repo-access cache; singleton GH_TOKEN unchanged"
            );
        }
        Err(e) => tracing::warn!("#{id}: repo routing after PR {repo} failed: {e}"),
    }
}

// --------------------------------------------------------------- scripts

/// Start the agent **detached**, so it outlives the exec that launched it —
/// and therefore outlives sandboard.
///
/// This is what makes re-adoption possible at all. As a child of the exec
/// session the agent died whenever the process watching it died, so every
/// `cargo run` threw away a live run; the supervisor had no honest option but
/// to delete the sandbox. Detached, the supervisor is a *reader of a log*
/// rather than the owner of a process, and a reader can be replaced.
///
/// Two consequences are deliberate:
///
/// - `timeout` runs inside the sandbox. Nothing out here can bound a process it
///   does not own, and an agent nobody is watching still spends money.
///   `--foreground` is load-bearing: without it `timeout` puts the command in a
///   process group of its own, so signalling the wrapper's group leaves
///   `claude` orphaned and still billing. Observed, not assumed.
/// - The briefing travels in an exported variable rather than inline. It is
///   already single-quoted for the outer shell, and quoting it a second time
///   for the inner `bash -c` is exactly the sort of thing that works until a
///   card description contains an apostrophe.
fn start_script(
    cfg: &AgentConfig,
    briefing: &str,
    engine: &str,
    conversation_id: Option<&str>,
    model: Option<&str>,
) -> Result<String, crate::engine::UnknownEngine> {
    let secs = cfg.agent_timeout_secs;
    // Engine argv (including Cursor --force/--trust/--sandbox disabled) lives
    // in `crate::engine` — unknown ids fail here instead of falling through to
    // claude.
    let cmd = crate::engine::command_line(
        engine,
        crate::engine::PromptEnv::Briefing,
        conversation_id,
        model,
    )?;
    let conv_export = conversation_id
        .map(|c| format!("export SANDBOARD_CONVERSATION={}\n", shell_quote(c)))
        .unwrap_or_default();
    let inference_exports = crate::engine::anthropic_inference_exports(engine);
    let hermes_inference_exports = crate::engine::hermes_inference_exports(engine);
    let hermes_query_setup = if engine.trim() == "hermes" {
        format!(
            "printf '%s' \"$SANDBOARD_BRIEFING\" > {}\n",
            crate::engine::HERMES_QUERY_FILE
        )
    } else {
        String::new()
    };
    Ok(format!(
        r#"set -e
rm -f {AGENT_PID} {AGENT_STATUS}
: > {AGENT_LOG}
export SANDBOARD_BRIEFING={brief}
{hermes_query_setup}{inference_exports}{hermes_inference_exports}{conv_export}setsid nohup bash -c 'echo $$ > {AGENT_PID}; cd {WORKDIR} && timeout --foreground {secs} {cmd} >> {AGENT_LOG} 2>&1; echo $? > {AGENT_STATUS}' </dev/null >/dev/null 2>&1 &
for i in $(seq 1 40); do
  if [ -s {AGENT_PID} ]; then exit 0; fi
  sleep 0.25
done
echo agent-did-not-start >&2; exit 1"#,
        brief = shell_quote(briefing)
    ))
}

/// Follow the agent's output from `from_line`, then exit with the agent's own
/// status.
///
/// A pure reader: running it twice, or from a different sandboard process, does not
/// disturb the run. The pid it waits on is the wrapper's, and the wrapper
/// writes the status file before exiting, so by the time `tail` notices the
/// process is gone the exit code is already on disk.
fn follow_script(from_line: u64) -> String {
    format!(
        r#"if [ -f {AGENT_STATUS} ]; then
  tail -n +{from_line} {AGENT_LOG} 2>/dev/null || true
  exit "$(cat {AGENT_STATUS})"
fi
tail -n +{from_line} -f --pid="$(cat {AGENT_PID})" {AGENT_LOG}
for i in $(seq 1 40); do
  if [ -f {AGENT_STATUS} ]; then break; fi
  sleep 0.25
done
exit "$(cat {AGENT_STATUS} 2>/dev/null || echo 1)""#
    )
}

pub const MARK_ALIVE: &str = "SANDBOARD-AGENT-ALIVE";
pub const MARK_EXITED: &str = "SANDBOARD-AGENT-EXITED";
pub const MARK_GONE: &str = "SANDBOARD-AGENT-GONE";
pub const MARK_LINES: &str = "SANDBOARD-LOG-LINES=";

/// Ask a sandbox whether its agent is still going, and how far its log got.
///
/// The line count is what a new supervisor resumes from — everything before it
/// was already streamed by the previous process.
fn probe_script() -> String {
    format!(
        r#"if [ -f {AGENT_STATUS} ]; then echo {MARK_EXITED}
elif [ -s {AGENT_PID} ] && kill -0 "$(cat {AGENT_PID})" 2>/dev/null; then echo {MARK_ALIVE}
else echo {MARK_GONE}
fi
printf '%s%s\n' '{MARK_LINES}' "$(wc -l < {AGENT_LOG} 2>/dev/null || echo 0)""#
    )
}

/// Stop a detached agent, best effort.
///
/// Only the failure path needs this. The sandbox is kept for inspection, and
/// the agent is no longer a child of anything we hold — so without this a run
/// we have already given up on keeps burning Vertex spend until its own
/// timeout. `setsid` made the wrapper a process-group leader, so negating the
/// pid takes `claude` with it.
///
/// Also used by Cockpit attach before starting an interactive `agent` so the
/// headless seat and the TTY do not fight over the same conversation.
pub(crate) async fn stop_agent(os: &OpenShell, name: &str) {
    let script = format!(
        r#"if [ -s {AGENT_PID} ]; then kill -TERM -"$(cat {AGENT_PID})" 2>/dev/null || true; fi"#
    );
    let _ = os.exec(name, &script, Duration::from_secs(30)).await;
}

/// Write agy token file + settings — never upload a host OAuth/settings file.
///
/// Requires `ANTIGRAVITY_ACCESS_TOKEN` (OpenShell placeholder from the attached
/// `antigravity` provider). The file stores that placeholder only; the gateway
/// resolves Bearer on Cloud Code endpoints declared by the provider type.
/// `gcp.project` / `location` come from Board provider config
/// (`ANTIGRAVITY_GCP_PROJECT` / `ANTIGRAVITY_GCP_LOCATION`) — set via API/UI.
pub(crate) async fn setup_agy_auth(
    os: &OpenShell,
    name: &str,
    board: &crate::store::SharedBoard,
) -> anyhow::Result<()> {
    let (project, location) = match crate::antigravity::gcp_from_board(board) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(sandbox = %name, error = %e, "agy auth: Board config incomplete");
            // Still write the token file so login shape is ready once config exists.
            let token_only = r#"set -e
TOKEN="${ANTIGRAVITY_ACCESS_TOKEN:-}"
if [ -z "$TOKEN" ]; then
  echo 'agy auth: ANTIGRAVITY_ACCESS_TOKEN missing (antigravity provider not attached)' >&2
  exit 0
fi
mkdir -p /sandbox/.gemini/antigravity-cli
python3 - <<'PY'
import json, os
cli = "/sandbox/.gemini/antigravity-cli"
token = os.environ["ANTIGRAVITY_ACCESS_TOKEN"]
with open(f"{cli}/antigravity-oauth-token", "w", encoding="utf-8") as f:
    json.dump(
        {
            "auth_method": "gcp",
            "token": {
                "access_token": token,
                "token_type": "Bearer",
                "expiry": "2099-01-01T00:00:00Z",
            },
        },
        f,
    )
os.chmod(f"{cli}/antigravity-oauth-token", 0o600)
PY
"#;
            let out = os.exec(name, token_only, Duration::from_secs(20)).await?;
            if !out.ok() {
                anyhow::bail!(
                    "agy auth inject failed (exit {}): {}",
                    out.code,
                    out.stderr.trim()
                );
            }
            return Ok(());
        }
    };
    let project_q = shell_quote(&project);
    let location_q = shell_quote(&location);
    let script = format!(
        r#"set -e
TOKEN="${{ANTIGRAVITY_ACCESS_TOKEN:-}}"
if [ -z "$TOKEN" ]; then
  echo 'agy auth: ANTIGRAVITY_ACCESS_TOKEN missing (antigravity provider not attached)' >&2
  exit 0
fi
export SANDBOARD_AGY_PROJECT={project_q}
export SANDBOARD_AGY_LOCATION={location_q}
# Override Vertex's GOOGLE_CLOUD_PROJECT so Code Assist quota/model resolve
# against the antigravity provider project (settings.json gcp.project alone
# does not fill agy's quotaProject).
export GOOGLE_CLOUD_PROJECT={project_q}
export GOOGLE_CLOUD_QUOTA_PROJECT={project_q}
export GCP_PROJECT_ID={project_q}
mkdir -p /sandbox/.gemini/antigravity-cli
python3 - <<'PY'
import json, os
cli = "/sandbox/.gemini/antigravity-cli"
token = os.environ["ANTIGRAVITY_ACCESS_TOKEN"]
project = os.environ["SANDBOARD_AGY_PROJECT"]
location = os.environ["SANDBOARD_AGY_LOCATION"]
# Nested shape matches Antigravity CLI oauth file — flat access_token is ignored.
with open(f"{{cli}}/antigravity-oauth-token", "w", encoding="utf-8") as f:
    json.dump(
        {{
            "auth_method": "gcp",
            "token": {{
                "access_token": token,
                "token_type": "Bearer",
                "expiry": "2099-01-01T00:00:00Z",
            }},
        }},
        f,
    )
os.chmod(f"{{cli}}/antigravity-oauth-token", 0o600)
with open(f"{{cli}}/settings.json", "w", encoding="utf-8") as f:
    json.dump(
        {{
            "enableTelemetry": False,
            "allowNonWorkspaceAccess": True,
            "gcp": {{"project": project, "location": location}},
        }},
        f,
    )
# Sourced by attach / print wrappers when Vertex has already set the wrong project.
with open(f"{{cli}}/sandboard-cloud.env", "w", encoding="utf-8") as f:
    f.write(f"GOOGLE_CLOUD_PROJECT={{project}}\n")
    f.write(f"GOOGLE_CLOUD_QUOTA_PROJECT={{project}}\n")
    f.write(f"GCP_PROJECT_ID={{project}}\n")
    f.write(f"GCP_LOCATION={{location}}\n")
    f.write(f"CLOUD_ML_REGION={{location}}\n")
    f.write(f"VERTEX_LOCATION={{location}}\n")
PY
"#
    );
    let out = os.exec(name, &script, Duration::from_secs(20)).await?;
    if !out.ok() {
        anyhow::bail!(
            "agy auth inject failed (exit {}): {}",
            out.code,
            out.stderr.trim()
        );
    }
    if out.stderr.contains("ANTIGRAVITY_ACCESS_TOKEN missing") {
        tracing::warn!(
            sandbox = %name,
            "agy auth: provider placeholder missing; attach antigravity and re-sync"
        );
    }
    Ok(())
}

/// Create-time env: `agent_env(engine)` then profile overlay (profile wins on key clash).
fn sandbox_create_env(engine: &str, profile_env: &BTreeMap<String, String>) -> Vec<(String, String)> {
    let mut env = agent_env(engine);
    for (k, v) in profile_env {
        if let Some(entry) = env.iter_mut().find(|(key, _)| key == k) {
            entry.1 = v.clone();
        } else {
            env.push((k.clone(), v.clone()));
        }
    }
    env
}

fn agent_env(engine: &str) -> Vec<(String, String)> {
    let mut env = vec![
        ("DISABLE_TELEMETRY".into(), "1".into()),
        ("DISABLE_ERROR_REPORTING".into(), "1".into()),
        ("DISABLE_AUTOUPDATER".into(), "1".into()),
        ("GIT_TERMINAL_PROMPT".into(), "0".into()),
        // The image's own ENV does NOT reach `sandbox exec` — PATH arrives as
        // the base image's default and CARGO_HOME arrives empty, so cargo is
        // invisible and rustup cannot pick a toolchain. Baking ENV into the
        // Containerfile is not sufficient; it has to be passed explicitly.
        ("RUSTUP_HOME".into(), "/opt/rust".into()),
        ("CARGO_HOME".into(), "/opt/cargo".into()),
        // Shared with the image warm step so agents reuse precompiled debug deps.
        ("CARGO_TARGET_DIR".into(), "/opt/cargo-target".into()),
        ("NPM_CONFIG_CACHE".into(), "/opt/npm-cache".into()),
        // Force HOME for Cursor MCP discovery (`~/.cursor/mcp.json`). OpenShell
        // usually derives this from passwd when run_as=sandbox, but exec paths
        // that inherit the supervisor's HOME=/root look for /root/.cursor and
        // miss the injected config (MCP server "sandboard" not found).
        ("HOME".into(), "/sandbox".into()),
        ("USER".into(), "sandbox".into()),
        (
            "PATH".into(),
            // No /sandbox/.venv — that was the old Ubuntu community image.
            // Include /usr/sbin for UBI tools (ss from iproute lives there).
            "/opt/cargo/bin:/usr/local/bin:/usr/bin:/usr/sbin:/bin:/sbin".into(),
        ),
        // Cursor Agent CLI compile cache (defaults to $HOME/Library/... on darwin
        // host builds of the wrapper; keep it under the writable sandbox tree).
        (
            "NODE_COMPILE_CACHE".into(),
            "/tmp/cursor-compile-cache".into(),
        ),
    ];
    env.extend(crate::engine::anthropic_inference_env(engine));
    env.extend(crate::engine::hermes_inference_env(engine));
    env
}

/// A credential helper that echoes the injected token. The value is OpenShell's
/// opaque placeholder; the egress proxy substitutes the real one.
const GIT_CRED: &str =
    r#"credential.helper=!f(){ echo username=x-access-token; echo password=$GH_TOKEN; };f"#;

/// Marker lines the supervisor reads back out of the clone step, so the
/// briefing can tell the agent what it is walking into.
pub const MARK_FRESH: &str = "SANDBOARD-BRANCH-FRESH";
pub const MARK_REBASED: &str = "SANDBOARD-BRANCH-REBASED";
pub const MARK_CONFLICT: &str = "SANDBOARD-BRANCH-CONFLICT";
pub const MARK_CONFLICT_FILES: &str = "SANDBOARD-CONFLICT-FILES=";

/// Cold-start only: wipe `/sandbox/repo` so the agent clones into an empty tree.
/// Reuse paths (park resume / Needs You reclaim) must not call this.
fn empty_workdir_script() -> String {
    format!(
        r#"set -e
rm -rf {WORKDIR}
mkdir -p {WORKDIR}
echo {MARK_FRESH}"#
    )
}

/// Reuse without a checkout: ensure `/sandbox/repo` exists without wiping
/// prior contents or caches. Agent still owns any clone.
fn ensure_workdir_script() -> String {
    format!(
        r#"set -e
mkdir -p {WORKDIR}
echo {MARK_FRESH}"#
    )
}

async fn ensure_report_schema_in_sandbox(
    os: &OpenShell,
    name: &str,
    timeout: Duration,
) -> anyhow::Result<()> {
    let host = std::path::Path::new("docs/schemas/report.schema.json");
    if !host.is_file() {
        tracing::warn!("report.schema.json missing on host; agent will rely on briefing prose");
        return Ok(());
    }
    // A prior upload bug could leave `{VERDICT_DIR}` as a *file* (schema
    // contents). Clear that so mkdir/upload always get a real directory.
    let prep = format!(
        r#"set -e
if [ -e {VERDICT_DIR} ] && [ ! -d {VERDICT_DIR} ]; then
  rm -f {VERDICT_DIR}
fi
mkdir -p {VERDICT_DIR}
test -d {VERDICT_DIR}"#
    );
    let out = os.exec(name, &prep, timeout).await?;
    anyhow::ensure!(out.ok(), "prepare {VERDICT_DIR}: {}", outerr(&out));
    // upload destination is a directory; file lands as report.schema.json inside.
    os.upload(name, host.to_str().unwrap_or_default(), VERDICT_DIR)
        .await?;
    let check = format!("test -f {VERDICT_DIR}/report.schema.json");
    let out = os.exec(name, &check, timeout).await?;
    anyhow::ensure!(
        out.ok(),
        "report.schema.json missing under {VERDICT_DIR} after upload: {}",
        outerr(&out)
    );
    Ok(())
}

/// Refresh an existing sandbox repository in-place (git fetch & optional rebase)
/// without wiping the workdir, build caches, or mid-run edits.
///
/// Park / MainAdvanced reclaim used to `reset --hard` + `clean -fd` and prefer
/// `origin/{branch}` over the live local branch — that discarded uncommitted
/// (and unpushed) work before the agent resumed. Prefer the local card branch;
/// only rebase when the tree is clean. Dirty trees stay put; steer notes tell
/// the agent to rebase. Cold runs still leave clone to the agent.
fn refresh_script(cfg: &AgentConfig, branch: &str) -> String {
    let upstream = cfg.repo.upstream.trim();
    let base = cfg.repo.base.trim();
    let base_ref = cfg.repo.base_ref();
    let fetch_base = if cfg.repo.uses_cross_fork() {
        format!(
            r#"git remote add upstream https://github.com/{upstream}.git 2>/dev/null || true
git -c '{GIT_CRED}' fetch -q upstream {base}
"#
        )
    } else {
        format!("git -c '{GIT_CRED}' fetch -q origin {base}\n")
    };
    format!(
        r#"set -e
export GIT_TERMINAL_PROMPT=0
if [ ! -d {WORKDIR}/.git ]; then
  echo "repository missing in workdir" >&2
  exit 1
fi
cd {WORKDIR}
git config user.email "agent@sandboard.local"
git config user.name "sandboard agent"
{fetch_base}# Keep local card-branch commits and the dirty tree. Do not reset --hard,
# clean -fd, or force checkout -B from origin/{{branch}} — those wiped park resumes.
if git rev-parse --verify {branch} >/dev/null 2>&1; then
  git checkout -q {branch}
elif git -c '{GIT_CRED}' ls-remote --exit-code --heads origin {branch} >/dev/null 2>&1; then
  git -c '{GIT_CRED}' fetch -q origin {branch}
  git checkout -q -B {branch} origin/{branch}
else
  git checkout -q -B {branch} {base_ref}
  echo {MARK_FRESH}
  exit 0
fi
if [ -n "$(git status --porcelain 2>/dev/null)" ]; then
  # Mid-run edits: leave them; agent rebases per steer / resume briefing.
  echo {MARK_REBASED}
  exit 0
fi
if git rebase -q {base_ref} >/dev/null 2>&1; then
  echo {MARK_REBASED}
else
  files=$(git diff --name-only --diff-filter=U 2>/dev/null | tr '\n' ',' | sed 's/,$//')
  git rebase --abort >/dev/null 2>&1 || true
  echo {MARK_CONFLICT}
  if [ -n "$files" ]; then
    echo "{MARK_CONFLICT_FILES}$files"
  fi
fi"#
    )
}

type MergeableFetchFut<'a> = std::pin::Pin<
    Box<
        dyn std::future::Future<Output = Result<Option<crate::github_app::PrConflictCheck>, String>>
            + Send
            + 'a,
    >,
>;

/// Observe GitHub `mergeable` for Review catch-up candidates.
///
/// MERGEABLE: stay in Review with no catch-up work signal (clear retry flags if
/// any). CONFLICTING: bounce to Backlog with a binding note. UNKNOWN (or a
/// deferred fetch): queue `rebase_requested` for the next sweep. No sandbox
/// recreate, no host `git rebase`.
pub async fn process_awaiting_mergeable_checks(
    board: &SharedBoard,
    cfg: &AgentConfig,
) -> Vec<Result<crate::model::WorkItem, String>> {
    let awaiting = board.list_awaiting_rebase();
    observe_review_catch_up_with(board, cfg, awaiting, |board, pr_url| {
        let board = board.clone();
        let pr_url = pr_url.to_string();
        Box::pin(async move {
            crate::github_app::fetch_pr_conflict_check(&board, &pr_url)
                .await
                .map_err(|e| e.to_string())
        })
    })
    .await
}

/// Tip-driven Review catch-up after `MainAdvanced`: observe scoped open Review PRs
/// via GitHub `mergeable`. Returns card ids bounced to Backlog on CONFLICTING.
pub async fn process_main_advanced_review_catch_up(
    board: &SharedBoard,
    advanced_repo: &str,
) -> Vec<crate::model::ItemId> {
    use crate::model::State;

    let agents = board.effective_agents();
    let candidates = board.identify_review_prs_for_main_advanced(advanced_repo);
    let results = observe_review_catch_up_with(board, &agents, candidates, |board, pr_url| {
        let board = board.clone();
        let pr_url = pr_url.to_string();
        Box::pin(async move {
            crate::github_app::fetch_pr_conflict_check(&board, &pr_url)
                .await
                .map_err(|e| e.to_string())
        })
    })
    .await;
    results
        .into_iter()
        .filter_map(|r| r.ok())
        .filter(|item| item.state == State::Backlog)
        .map(|item| item.id)
        .collect()
}

/// Same-parent sibling Review catch-up after a merge→Done.
pub async fn process_sibling_review_catch_up(
    board: &SharedBoard,
    near_id: crate::model::ItemId,
) -> Vec<Result<crate::model::WorkItem, String>> {
    let agents = board.effective_agents();
    let candidates = board.identify_behind_sibling_prs(near_id);
    observe_review_catch_up_with(board, &agents, candidates, |board, pr_url| {
        let board = board.clone();
        let pr_url = pr_url.to_string();
        Box::pin(async move {
            crate::github_app::fetch_pr_conflict_check(&board, &pr_url)
                .await
                .map_err(|e| e.to_string())
        })
    })
    .await
}

async fn observe_review_catch_up_with<F>(
    board: &SharedBoard,
    cfg: &AgentConfig,
    candidates: Vec<crate::model::WorkItem>,
    mut fetch: F,
) -> Vec<Result<crate::model::WorkItem, String>>
where
    F: for<'a> FnMut(&'a SharedBoard, &'a str) -> MergeableFetchFut<'a>,
{
    use crate::github_app::PrMergeableState;

    let mut results = Vec::new();
    for item in candidates {
        let Some(pr_url) = item.pr_url().filter(|u| !u.trim().is_empty()) else {
            tracing::warn!("mergeable check skipped for card #{}: no pr_url", item.id);
            continue;
        };

        let expected_base = match board.resolve_card_repo(item.id) {
            Ok(Some(repo)) => repo.base,
            Ok(None) => cfg.repo.base.clone(),
            Err(e) => {
                tracing::warn!(
                    "mergeable check skipped for card #{}: cannot resolve remotes: {e}",
                    item.id
                );
                continue;
            }
        };

        let check = match fetch(board, pr_url).await {
            Ok(Some(c)) => c,
            Ok(None) => {
                // App not configured, or PR 404 — queue retry for the next sweep.
                tracing::warn!(
                    "mergeable check deferred for card #{}: no App token or PR not found",
                    item.id
                );
                if !item.rebase_requested {
                    let _ = board.dispatch_rebase(item.id);
                }
                continue;
            }
            Err(e) => {
                tracing::warn!(
                    "mergeable check failed for card #{}: {e}; will retry",
                    item.id
                );
                if !item.rebase_requested {
                    let _ = board.dispatch_rebase(item.id);
                }
                continue;
            }
        };

        if let Some(base) = check.base_ref.as_deref() {
            if !base.eq_ignore_ascii_case(expected_base.trim()) {
                tracing::info!(
                    "mergeable check skipped for card #{}: PR base {base} != {expected_base}",
                    item.id
                );
                // Not a same-base catch-up target — clear any retry queue.
                if item.rebase_requested || item.awaiting_dispatch {
                    results.push(board.complete_rebase_clean(item.id));
                }
                continue;
            }
        }

        match check.mergeable {
            PrMergeableState::Mergeable => {
                if item.rebase_requested || item.awaiting_dispatch {
                    results.push(board.complete_rebase_clean(item.id));
                }
                // else: silent no-op — no catch-up work signal
            }
            PrMergeableState::Conflicting => {
                results.push(board.complete_rebase_conflict(
                    item.id,
                    &[],
                    Some("GitHub PR mergeable is CONFLICTING"),
                ));
            }
            PrMergeableState::Unknown => {
                // GitHub computes mergeable asynchronously — queue retry.
                if !item.rebase_requested {
                    match board.dispatch_rebase(item.id) {
                        Ok(_) => {}
                        Err(e) => results.push(Err(e)),
                    }
                } else {
                    tracing::debug!(
                        "mergeable UNKNOWN for card #{}; retry next sweep",
                        item.id
                    );
                }
            }
        }
    }
    results
}

/// Bounce reason when finish refuses Review because the PR cannot merge.
pub const CONFLICTING_PR_BOUNCE_REASON: &str =
    "PR mergeable is CONFLICTING; resolve rebase conflicts before Review";

/// Ask GitHub whether the agent actually opened a PR.
///
/// Not "create a PR" — the agent does that. This is the supervisor checking a
/// fact it is going to put on the board, which is the one thing it must not
/// take on trust. A query keeps working when tool output changes; a script
/// that creates things has to be right about flags, idempotency and failure
/// modes, and ours repeatedly was not.
///
/// Also emits mergeable state (`MERGEABLE` / `CONFLICTING` / `UNKNOWN`). A
/// hollow Review after a conflict bounce is worse than bouncing again: the
/// card looks finished while GitHub still cannot merge.
fn pr_lookup_script(cfg: &AgentConfig, branch: &str) -> String {
    let upstream = cfg.repo.upstream.trim();
    // `gh pr list --head` no longer accepts owner:branch. Keep the branch
    // filter there and use GitHub's search qualifier for cross-fork ownership.
    let head = if cfg.repo.uses_cross_fork() {
        let fork_owner = cfg.repo.fork.split('/').next().unwrap_or("").trim();
        format!("--head {branch} --search 'head:{fork_owner}:{branch}'")
    } else {
        format!("--head {branch}")
    };
    format!(
        r#"set -e
row=$(gh pr list --repo {upstream} {head} --state open --json url,mergeable --jq '.[0] // empty')
if [ -n "$row" ]; then
  url=$(printf '%s' "$row" | jq -r '.url // empty')
  mergeable=$(printf '%s' "$row" | jq -r '.mergeable // empty')
  if [ -n "$url" ]; then echo "{PR_URL_MARK}$url"; fi
  if [ -n "$mergeable" ]; then echo "{PR_MERGEABLE_MARK}$mergeable"; fi
fi"#
    )
}

/// Backfill base/head from GitHub when report only had a URL.
fn pr_view_binding_script(pr_url: &str) -> String {
    let url = pr_url.replace('\'', r#"'\''"#);
    format!(
        r#"set -e
gh pr view '{url}' --json url,baseRefName,headRefName,baseRepository,headRepository \
  --jq '"SANDBOARD-PR-BIND="+(.url//"")+"|"+(.baseRepository.nameWithOwner//"")+"|"+(.baseRefName//"")+"|"+(.headRepository.nameWithOwner//"")+"|"+(.headRefName//"")'"#
    )
}

fn parse_pr_binding_line(stdout: &str) -> Option<crate::model::PullRequest> {
    for line in stdout.lines() {
        let Some(rest) = line.trim().strip_prefix("SANDBOARD-PR-BIND=") else {
            continue;
        };
        let mut parts = rest.split('|');
        let url = parts.next()?.trim();
        let base_repo = parts.next()?.trim();
        let base_ref = parts.next()?.trim();
        let head_repo = parts.next()?.trim();
        let head_ref = parts.next()?.trim();
        if url.is_empty() || base_repo.is_empty() {
            return None;
        }
        let head_repo = if head_repo.is_empty() {
            base_repo
        } else {
            head_repo
        };
        let head_ref = if head_ref.is_empty() {
            "main"
        } else {
            head_ref
        };
        return Some(crate::model::PullRequest {
            url: url.to_string(),
            base: Some(crate::model::PullRequestEnd::new(base_repo, base_ref)),
            head: Some(crate::model::PullRequestEnd::new(head_repo, head_ref)),
            ..Default::default()
        });
    }
    None
}

/// Prefix so the URL is read from a line we chose, not guessed at.
pub const PR_URL_MARK: &str = "SANDBOARD-PR-URL=";
/// GitHub `mergeable` enum from `gh pr list --json mergeable`.
pub const PR_MERGEABLE_MARK: &str = "SANDBOARD-PR-MERGEABLE=";

/// GitHub PR mergeability as reported by the API.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrMergeable {
    Mergeable,
    Conflicting,
    /// API returned UNKNOWN, null, empty, or an unrecognised value — do not
    /// hard-fail a finish on a flaky signal.
    Unknown,
}

/// Read `SANDBOARD-PR-MERGEABLE=` from a pr_lookup stdout. Missing/empty → Unknown.
pub fn parse_pr_mergeable(stdout: &str) -> PrMergeable {
    for line in stdout.lines() {
        if let Some(rest) = line.trim().strip_prefix(PR_MERGEABLE_MARK) {
            return match rest.trim().to_ascii_uppercase().as_str() {
                "MERGEABLE" => PrMergeable::Mergeable,
                "CONFLICTING" => PrMergeable::Conflicting,
                _ => PrMergeable::Unknown,
            };
        }
    }
    PrMergeable::Unknown
}

fn parse_pr_url(stdout: &str) -> Option<String> {
    stdout
        .lines()
        .find_map(|l| l.strip_prefix(PR_URL_MARK))
        .map(str::to_string)
}

/// Script to run `git diff --numstat` against the base branch.
fn diffstat_script(cfg: &AgentConfig) -> String {
    let base_ref = cfg.repo.base_ref();
    format!(
        r#"set -e
cd {WORKDIR}
git diff --numstat {base_ref}"#
    )
}

/// Parse `git diff --numstat` output into (added, removed) line counts.
pub fn parse_diffstat(stdout: &str) -> (u32, u32) {
    let mut added = 0u32;
    let mut removed = 0u32;
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 2 {
            if let Ok(a) = parts[0].parse::<u32>() {
                added += a;
            }
            if let Ok(r) = parts[1].parse::<u32>() {
                removed += r;
            }
        }
    }
    (added, removed)
}

/// What the agent is walking into, read back from the clone step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BranchState {
    /// New branch off base — nothing done on this card yet.
    Fresh,
    /// The card's branch already existed and rebased cleanly onto base.
    Rebased,
    /// The branch exists but conflicts with base. Resolving that is the agent's
    /// job, not the supervisor's — it needs the semantics to do it safely.
    Conflicted,
}

fn branch_state_of(stdout: &str) -> BranchState {
    if stdout.contains(MARK_CONFLICT) {
        BranchState::Conflicted
    } else if stdout.contains(MARK_REBASED) {
        BranchState::Rebased
    } else {
        BranchState::Fresh
    }
}

/// Pick cold vs parked-resume briefing.
///
/// Park mid-run keeps the short resume prompt (session memory already has the
/// cold text). A conflicted branch is different: the agent must be told to
/// resolve conflicts even when the conversation id is reused — otherwise a
/// hollow resume walks past CONFLICTS and reports into Review again.
/// Pull a human-decided clone target out of steering notes.
///
/// `answer_escalation` stores `Decision: Clone owner/name …`. Without this,
/// unbound Remotes text still says "escalate if the Project prompt is silent"
/// and the agent re-asks the same Needs You after every answer.
fn clone_target_from_notes(notes: &[String]) -> Option<String> {
    for n in notes.iter().rev() {
        let raw = n.trim();
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
        if token.contains('/') || token.starts_with("http") {
            return Some(token.to_string());
        }
    }
    None
}

/// Host-completed proof facts pasted into Decision / steer notes.
///
/// Meta cards (e.g. second-repo proof) cannot operate the host Board from inside
/// a sandbox. Without this, answering "host runs the probe; re-claim to document"
/// auto-reclaims the agent into the same Needs You because no evidence arrived.
///
/// Accepted shapes (newest note wins):
/// - `Proof: card=#175 pr_url=https://github.com/owner/repo/pull/2 upstream=owner/repo fork=owner/repo`
/// - `Decision: … pr_url=https://…` (must include `pr_url=`)
fn proof_facts_from_notes(notes: &[String]) -> Option<String> {
    for n in notes.iter().rev() {
        let raw = n.trim();
        let body = raw
            .strip_prefix("Decision:")
            .or_else(|| raw.strip_prefix("decision:"))
            .map(str::trim)
            .unwrap_or(raw);
        if let Some(rest) = body
            .strip_prefix("Proof:")
            .or_else(|| body.strip_prefix("proof:"))
            .map(str::trim)
        {
            if rest.contains("pr_url=") {
                return Some(rest.to_string());
            }
        }
        if body.contains("pr_url=") {
            return Some(body.to_string());
        }
    }
    None
}

/// Remotes / clone lines for the agent briefing.
///
/// `preserved` is the reclaim contract: park unpark resume and Needs You answer
/// reclaim both reuse a live sandbox, so copy must not claim a blank
/// `/sandbox/repo` or order a wipe-and-clone. Cold start (`!preserved`) still
/// describes an empty workdir the agent clones into.
fn remotes_briefing_lines(
    repo: &crate::schema::RepoConfig,
    notes: &[String],
    preserved: bool,
) -> String {
    if let Some(facts) = proof_facts_from_notes(notes) {
        // Proof facts win over unbound/clone guidance — the card's job is documentation.
        return format!(
            "\nHost proof facts are already on this card (`{facts}`). Remotes for *this* \
card follow the Project prompt / Clone decision as usual; the operational proof DoD is \
satisfied by the cited Board card + `pr_url`. Do **not** re-escalate for another probe.\n"
        );
    }
    if !repo.is_complete() {
        if let Some(target) = clone_target_from_notes(notes) {
            if preserved {
                return format!(
                    "\n`/sandbox/repo` was preserved on this reclaim (park resume and Needs You \
answer reclaim share this path). The human already decided the clone target: `{target}`. \
Use the existing checkout if present; otherwise clone `{target}` into `/sandbox/repo`. \
Do **not** wipe-and-clone. Do **not** re-escalate asking which repository to clone.\n"
                );
            }
            return format!(
                "\nNo card pull_request yet (first run). The human already decided the clone \
target: `{target}`. Clone that into `/sandbox/repo` and continue the card. Do **not** \
re-escalate asking which repository to clone.\n"
            );
        }
        // Clone target comes from card intent/DoD/notes. Escalate when unnamed.
        if preserved {
            return "\n`/sandbox/repo` was preserved on this reclaim (park resume and Needs You \
answer reclaim share this path). Inspect and continue — clone only if the checkout is \
missing and this card's intent, definition of done, or notes name an exact repository \
(`owner/name` or git URL), including distinct push vs PR-target remotes when both are \
named. Do **not** wipe-and-clone. Do **not** guess from context, history, or the card \
title. If you must clone and the target is missing or ambiguous, write \
`/sandbox/.sandboard/escalate.json` with a short question, at least two concrete options, \
and a recommended index, then exit — do not open a PR. When you finish, write \
`/sandbox/.sandboard/report.json` with `url`, `base`, and `head` (schema: \
`/sandbox/.sandboard/report.schema.json`).\n"
                .into();
        }
        return "\nNo card pull_request yet (first run). Clone into `/sandbox/repo` **only** \
when this card's intent, definition of done, or notes name an exact repository \
(`owner/name` or git URL), including distinct push vs PR-target remotes when both \
are named. Do **not** guess from context, history, or the card title. If the clone \
target is missing or ambiguous, write `/sandbox/.sandboard/escalate.json` with a short \
question, at least two concrete options, and a recommended index, then exit — do not \
clone and do not open a PR. When you do clone and finish, write \
`/sandbox/.sandboard/report.json` with `url`, `base`, and `head` (schema: \
`/sandbox/.sandboard/report.schema.json`).\n"
            .into();
    }
    let base = repo.base.trim();
    let upstream = repo.upstream.trim();
    let clone = repo.clone_target();
    let workdir = if preserved {
        "`/sandbox/repo` was preserved on this reclaim (park resume and Needs You answer \
reclaim share this path). Inspect and continue — clone only if the checkout is missing. \
Do not wipe-and-clone."
    } else {
        "Clone into `/sandbox/repo` yourself (empty workspace on claim)."
    };
    if repo.uses_cross_fork() {
        format!(
            "\n{workdir} \
Remotes for this run: `origin` is `{clone}` (push); add `upstream` = `{upstream}` and \
rebase onto `upstream/{base}` (PR target). Never treat `origin/{base}` alone as the \
merge base when head and base repos differ.\n"
        )
    } else {
        format!(
            "\n{workdir} \
Remotes for this run: `origin` is `{upstream}` (clone and push). Rebase onto \
`origin/{base}`. Open the PR against the same repo, base `{base}`.\n"
        )
    }
}

fn choose_briefing(
    grant: &ClaimGrant,
    branch: BranchState,
    branch_name: &str,
    repo: &crate::schema::RepoConfig,
    resume: bool,
) -> String {
    if resume && branch != BranchState::Conflicted {
        resume_briefing(grant, repo)
    } else {
        briefing(grant, branch, branch_name, repo)
    }
}

/// Short prompt for an agy `--conversation` resume after park.
///
/// The model already has the cold briefing in session memory; re-dumping it
/// would burn tokens and invite the agent to restart the card from scratch.
fn resume_briefing(grant: &ClaimGrant, repo: &crate::schema::RepoConfig) -> String {
    let mut b = String::new();
    b.push_str(
        "You were parked mid-run. The agent process was stopped; any in-flight tools \
         may have been killed. Inspect the repo state and continue this card — do not \
         start over unless the notes below say so.\n\n",
    );
    b.push_str(&format!("Your card: {}\n", grant.title));
    if let Some(key) = &grant.plan_task_key {
        b.push_str(&format!("Plan key: {key}\n"));
    }
    if !grant.intent.trim().is_empty() {
        b.push_str(&format!("Intent: {}\n", grant.intent.trim()));
    }
    if let Some(dod) = &grant.definition_of_done {
        b.push_str(&format!("Definition of done: {dod}\n"));
    }
    if !grant.notes.is_empty() {
        b.push_str(
            "\nNotes from the human steering this (BINDING — if these conflict with \
             earlier instructions, follow the notes):\n",
        );
        for n in &grant.notes {
            b.push_str(&format!("  - {n}\n"));
        }
    }
    // Park resume reclaims a kept sandbox — same preserve remotes contract as
    // Needs You answer reclaim (Rebased / Conflicted cold briefing).
    b.push_str(&remotes_briefing_lines(repo, &grant.notes, true));
    b.push_str(
        "\nWhen the work is done, write `/sandbox/.sandboard/report.json` (url/base/head per \
         report.schema.json) and publish the PR on this card's branch.\n",
    );
    b
}

/// Plan + standing prompts are the primary inputs; a fresh `claude -p` has none
/// of them unless we put them here. Plan (breakdown) precedes protocol /
/// board / project standing text so card context comes before standing rules.
fn briefing(
    grant: &ClaimGrant,
    branch: BranchState,
    branch_name: &str,
    repo: &crate::schema::RepoConfig,
) -> String {
    let mut b = String::new();
    b.push_str("You are working on one card. Do exactly this card.\n\n");

    if let Some(pt) = &grant.project_title {
        b.push_str(&format!("Project: {pt}\n"));
    }

    if grant.plan_summary.is_some() || !grant.plan_tasks.is_empty() {
        b.push_str("\nProject Plan (source of truth for the breakdown):\n");
        if let Some(sum) = &grant.plan_summary {
            b.push_str(&format!("Summary: {sum}\n"));
        }
        for t in &grant.plan_tasks {
            let mark = if t.current { " ← YOUR CARD" } else { "" };
            let deps = if t.blocked_by_keys.is_empty() {
                String::new()
            } else {
                format!(" [blocked_by: {}]", t.blocked_by_keys.join(", "))
            };
            b.push_str(&format!(
                "  - {key}: {title} — {intent} (DoD: {dod}){deps}{mark}\n",
                key = t.key,
                title = t.title,
                intent = t.intent,
                dod = t.definition_of_done,
                deps = deps,
                mark = mark,
            ));
        }
        b.push('\n');
    }

    b.push_str("Protocol (hardwired):\n");
    b.push_str(crate::model::PROTOCOL_MINIMUM.trim());
    b.push_str("\n\n");

    if let Some(prompt) = &grant.board_prompt {
        if !prompt.trim().is_empty() {
            b.push_str("Board prompt (standing agent policy):\n");
            b.push_str(prompt.trim());
            b.push('\n');
        }
    }

    if let Some(prompt) = &grant.project_prompt {
        if !prompt.trim().is_empty() {
            b.push_str("Project prompt (Project standing extras):\n");
            b.push_str(prompt.trim());
            b.push('\n');
        }
    }

    push_sandbox_prompt_section(&mut b, grant.sandbox_prompt.as_deref());

    b.push_str(&format!("Your card: {}\n", grant.title));
    if let Some(key) = &grant.plan_task_key {
        b.push_str(&format!("Plan key: {key}\n"));
    }
    if !grant.intent.trim().is_empty() {
        b.push_str(&format!("Intent: {}\n", grant.intent.trim()));
    }
    if let Some(dod) = &grant.definition_of_done {
        b.push_str(&format!("Definition of done: {dod}\n"));
    }

    if !grant.notes.is_empty() {
        // Notes are the human's live correction. When they conflict with title
        // or definition of done (common after Request changes), notes win —
        // otherwise agents re-satisfy a stale DoD and ignore the steer.
        b.push_str(
            "\nNotes from the human steering this (BINDING — if these conflict with \
             the title or definition of done above, follow the notes):\n",
        );
        for n in &grant.notes {
            b.push_str(&format!("  - {n}\n"));
        }
    }

    let base_ref = repo.base_ref();
    // Fresh = cold-start empty workdir. Rebased / Conflicted = reclaim of a
    // kept sandbox (Needs You answer reclaim; park resume without conversation
    // resume uses the same states). Park resume with conversation resume uses
    // `resume_briefing` instead — same preserve remotes contract.
    let preserved = matches!(branch, BranchState::Rebased | BranchState::Conflicted);
    match branch {
        BranchState::Fresh => {
            if repo.is_complete() {
                b.push_str(&format!(
                    "\nFirst run: `/sandbox/repo` is empty. \
Clone using the Remotes below, create or resume branch `{branch_name}` off the base, \
then do the card. Nothing has been done on this card yet.\n"
                ));
            } else if proof_facts_from_notes(&grant.notes).is_some() {
                b.push_str(
                    "\nFirst run with host Proof facts already on the card (see Remotes / notes). \
Document from those facts — do **not** re-escalate asking for another Board probe.\n",
                );
            } else if clone_target_from_notes(&grant.notes).is_some() {
                b.push_str(
                    "\nFirst run: `/sandbox/repo` is empty. Clone the human-decided target \
(see Remotes / notes) — do not re-ask which repository.\n",
                );
            } else {
                b.push_str(
                    "\nFirst run: `/sandbox/repo` is empty. Clone only if this card's intent/DoD \
names an exact product repo; otherwise escalate (see Remotes) — do not guess.\n",
                );
            }
        }
        BranchState::Rebased => b.push_str(&format!(
            "\nThis card has been worked before. `/sandbox/repo` was preserved on this reclaim \
             (park resume and Needs You answer reclaim share this path). You are on its \
             existing branch, already rebased onto `{base_ref}` — inspect and continue; \
             clone only if the checkout is missing. Do not wipe-and-clone. Address the \
             notes above rather than starting over.\n"
        )),
        BranchState::Conflicted => {
            b.push_str(&format!(
                "\nThis card has been worked before and its branch CONFLICTS with the base. \
             `/sandbox/repo` was preserved on this reclaim (park resume and Needs You \
             answer reclaim share this path). The rebase was left un-applied, so you are \
             on the branch as it was. Rebase onto `{base_ref}` yourself and resolve the \
             conflicts, keeping the intent of both sides. Do this before any other work. \
             Do not wipe-and-clone.\n"
            ));
        }
    }

    b.push_str(&remotes_briefing_lines(repo, &grant.notes, preserved));

    b.push_str(
        "\nIf you hit network connectivity problems (denied egress, hangs on fetch/clone/API, \
         blocked hosts), do **not** hack around them — no alternate mirrors, proxy tricks, \
         bundling deps from unexpected hosts, or rewriting URLs to dodge the allow-list. \
         Escalate via `/sandbox/.sandboard/escalate.json` and stop; a human decides whether the \
         sandbox network policy should change.\n",
    );

    let is_initial_plan = crate::model::title_is_initial_plan(&grant.title);

    if is_initial_plan {
        b.push_str(
            "\nThis is the Project's **Initial plan** card.\n\
             Propose the sibling Tasks that should be created: write `/sandbox/.sandboard/plan.json` \
             with a `summary` and `tasks` (each: `key`, `title`, `intent`, \
             `definition_of_done`, optional `blocked_by_keys`). In **each** task's intent \
             and/or definition_of_done, name the exact repository to clone \
             (`owner/name`, and push remote when it differs). Then exit — the supervisor \
             picks up plan.json and moves this card to Review. **Approve** creates those Tasks.\n\
             Finish this card with `plan.json` (use `escalate.json` only for a real human \
             decision). Skip `split.json` and `report.json` here.\n",
        );
    } else {
        b.push_str(
            "\nIf you hit a real decision or ambiguity that requires human input, do not guess. \
             Write `/sandbox/.sandboard/escalate.json` with your question, options \
             (`label`+`detail`, or `title`+`body`), and recommended choice index, then exit. \
             Options must supply at least two concrete choices.\n\
             \nIf work is discovered to be bigger than one card, do not overrun. \
             Write `/sandbox/.sandboard/split.json` with `children` each having `title`, `intent`, \
             optional `definition_of_done`, optional `key`, and optional `blocked_by_keys` \
             (Plan-style deps), then exit. The card goes to **Review** with that proposal — a human \
             **Approve** (or PR merge when a PR exists) creates the sibling Tasks under the same Project. \
             Splits may only carve this card's definition of done into smaller slices of the same outcome. \
             Do not invent work that belongs to another Project — escalate instead. \
             If a PR already exists for the card, do not split — finish via report. \
             Split and publish are mutually exclusive for one run.\n",
        );

        b.push_str(
            "\nWhen the work is done, write `/sandbox/.sandboard/report.json` with `url`, `base`, \
`head` (see `/sandbox/.sandboard/report.schema.json`), diffstat (`added`/`removed`), and optional \
`gates`. That PR is appended to the card — it does not replace PRs already recorded.\n\
Whenever you open a PR during the run (including an additional repo), record it immediately \
via sandboard MCP `report_pull_request` (url + base/head) so the card keeps a list. Review does \
not leave until every listed PR is merged.\n",
        );
        b.push_str(
            "\nRun the project's own checks before you finish — board-wide quality gates live in \
the Board prompt above; Project-specific gates live in the Project prompt; card-specific gates \
live in this card's definition of done. Do not assume cargo or any other toolchain unless those \
instructions name it.\n",
        );
        b.push_str(&format!(
            "\nWhen the work is done, publish it yourself:\n\
             \n  1. Commit on `{branch}`. Do not commit to any other branch.\n\
               2. Push to `origin`. Force-push is fine on your own branch.\n\
               3. Open or update a pull request against the product base (see Remotes above).\n\
             \nThe PR is how a human reviews this, so it is part of the work, not an afterthought.\n",
            branch = branch_name,
        ));
    }
    b
}

// ----------------------------------------------------- durable cockpit
//
// Board `cockpit_session` is the only lifecycle. This loop materializes the cockpit
// sandbox + detached agent and reconciles across sandboard restart. It must not
// call claim / heartbeat / report / split or touch the card-dispatch queue.

const COCKPIT_CANCEL_PARKED: &str = "cockpit parked";
const COCKPIT_CANCEL_STOPPED: &str = "cockpit stopped";
const COCKPIT_CANCEL_SUPERSEDED: &str = "cockpit session superseded";
const COCKPIT_CANCEL_UNUSABLE: &str = "cockpit sandbox unusable";

fn is_cockpit_parked(err: &str) -> bool {
    err.contains(COCKPIT_CANCEL_PARKED)
}

fn is_cockpit_stopped(err: &str) -> bool {
    err.contains(COCKPIT_CANCEL_STOPPED)
}

fn is_cockpit_superseded(err: &str) -> bool {
    err.contains(COCKPIT_CANCEL_SUPERSEDED)
}

fn is_cockpit_unusable(err: &str) -> bool {
    err.contains(COCKPIT_CANCEL_UNUSABLE)
}

/// Should reconcile keep this cockpit sandbox?
///
/// Running and Parked keep the Board-named environment (and the stable
/// `sandboard-cockpit` singleton). No session → reap.
fn should_keep_cockpit_sandbox(session: Option<&CockpitSession>, sandbox: &str) -> bool {
    let Some(s) = session else {
        return false;
    };
    let stem = crate::schema::cockpit_sandbox_name();
    if let Some(env) = s.environment.as_deref() {
        if env == sandbox {
            return true;
        }
    }
    sandbox == stem
}

fn sandbox_spec_for_cockpit(
    name: &str,
    resolved: &crate::model::ResolvedSandboxCreate,
    attach_providers: &[String],
    engine: &str,
) -> SandboxSpec {
    let env = sandbox_create_env(engine, &resolved.env);
    // Cockpit's sandboard MCP entry is stdio over a local Unix socket
    // (cockpit_mcp_tunnel::AGENT_SOCK_PATH baked into mcp.json) — no URL,
    // no env var to inject.
    SandboxSpec {
        name: name.to_string(),
        from: resolved.image.clone(),
        providers: attach_providers.to_vec(),
        policy: Some(resolved.policy.clone()),
        env,
        labels: vec![(LABEL_COCKPIT.to_string(), "1".to_string())],
        cpu: resolved.cpu.clone(),
        memory: resolved.memory.clone(),
    }
}

fn cockpit_engine(board: &SharedBoard, _resolved: &crate::model::ResolvedSandboxCreate) -> String {
    board.resolve_cockpit_engine()
}

fn ensure_cockpit_running(board: &SharedBoard) -> anyhow::Result<()> {
    ensure_cockpit_session_running(board, None)
}

fn ensure_cockpit_session_running(
    board: &SharedBoard,
    expected_created_at: Option<DateTime<Utc>>,
) -> anyhow::Result<()> {
    let Some(session) = board.cockpit_session() else {
        anyhow::bail!("{COCKPIT_CANCEL_STOPPED}");
    };
    if expected_created_at
        .map(|expected| session.created_at != expected)
        .unwrap_or(false)
    {
        anyhow::bail!("{COCKPIT_CANCEL_SUPERSEDED}");
    }
    match session.status {
        CockpitSessionStatus::Running => Ok(()),
        CockpitSessionStatus::Parked => anyhow::bail!("{COCKPIT_CANCEL_PARKED}"),
    }
}

async fn with_cockpit_cancel<F, T>(
    board: &SharedBoard,
    expected_created_at: DateTime<Utc>,
    fut: F,
) -> anyhow::Result<T>
where
    F: Future<Output = anyhow::Result<T>>,
{
    tokio::pin!(fut);
    let mut poll = tokio::time::interval(watch_board_poll());
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    poll.tick().await;
    loop {
        tokio::select! {
            res = &mut fut => return res,
            _ = poll.tick() => {
                ensure_cockpit_session_running(board, Some(expected_created_at))?
            },
        }
    }
}

/// In-flight cockpit sandbox deletes. OpenShell delete is slow (~tens of
/// seconds); awaiting it on the seat loop blocked Start. Dedup so inventory
/// ticks do not stack concurrent deletes for the same name.
fn cockpit_delete_inflight() -> &'static parking_lot::Mutex<HashSet<String>> {
    static INFLIGHT: OnceLock<parking_lot::Mutex<HashSet<String>>> = OnceLock::new();
    INFLIGHT.get_or_init(|| parking_lot::Mutex::new(HashSet::new()))
}

/// True when Board still wants this cockpit sandbox (Running/Parked session).
fn cockpit_session_wants_sandbox(board: &SharedBoard, name: &str) -> bool {
    let session = board.cockpit_session();
    should_keep_cockpit_sandbox(session.as_ref(), name)
}

/// Delete a cockpit sandbox in the background, skipping if Start raced back in.
fn spawn_reap_cockpit_sandbox(os: OpenShell, board: SharedBoard, name: String) {
    spawn_reap_cockpit_sandbox_inner(os, board, name, false);
}

fn spawn_reap_cockpit_sandbox_force(os: OpenShell, board: SharedBoard, name: String) {
    spawn_reap_cockpit_sandbox_inner(os, board, name, true);
}

fn spawn_reap_cockpit_sandbox_inner(
    os: OpenShell,
    board: SharedBoard,
    name: String,
    force: bool,
) {
    {
        let mut g = cockpit_delete_inflight().lock();
        if !g.insert(name.clone()) {
            return;
        }
    }
    tokio::spawn(async move {
        // Stop → Start can land while we were queued; never delete under a live session.
        if !force && cockpit_session_wants_sandbox(&board, &name) {
            tracing::info!("cockpit: skip reap of {name}; session wants sandbox again");
        } else {
            tracing::info!("cockpit: deleting sandbox {name}");
            let _ = os.delete(&name).await;
            if cockpit_session_wants_sandbox(&board, &name) {
                tracing::warn!(
                    "cockpit: deleted {name} while session wanted it; seat loop will recreate"
                );
            }
        }
        cockpit_delete_inflight().lock().remove(&name);
    });
}

/// Reap or keep cockpit sandboxes from inventory. Does not start/adopt agents.
///
/// Deletes are spawned — awaiting gateway delete here used to stall the seat
/// loop for ~45s, so a Start clicked during Stop never got a turn until teardown
/// finished.
async fn reconcile_cockpit_inventory(os: &OpenShell, board: &SharedBoard) {
    let Ok(cockpit_boxes) = os.list_cockpit().await else {
        tracing::warn!("could not list cockpit sandboxes; skipping cockpit inventory");
        return;
    };
    let session = board.cockpit_session();
    for sb in cockpit_boxes {
        if !should_keep_cockpit_sandbox(session.as_ref(), &sb.name) {
            tracing::info!(
                "reaping unneeded cockpit sandbox {} (session={:?})",
                sb.name,
                session.as_ref().map(|s| s.status),
            );
            spawn_reap_cockpit_sandbox(os.clone(), board.clone(), sb.name);
        }
    }
}

/// Materialize / adopt the cockpit when Board says Running.
///
/// Returns `(sandbox_name, hold_result)`. Caller must release the seat-loop
/// `supervising` flag **before** [`finalize_cockpit`] — finalize may wait on a
/// slow gateway delete, and Stop→Start must be able to spawn a new seat meanwhile.
async fn run_cockpit_seat(board: SharedBoard) -> anyhow::Result<(String, anyhow::Result<()>)> {
    let os = board.openshell_client();
    ensure_cockpit_running(&board)?;

    let agents = board.effective_agents();
    let resolved = board.resolve_cockpit_sandbox_create();
    let attach = board.attach_providers_for_resolved(&resolved);
    let engine = cockpit_engine(&board, &resolved);

    let session = board
        .cockpit_session()
        .ok_or_else(|| anyhow::anyhow!("{COCKPIT_CANCEL_STOPPED}"))?;
    let session_created_at = session.created_at;
    let existing = session.environment.clone();
    let default_name = crate::schema::cockpit_sandbox_name();

    let (name, is_reused) = match existing {
        Some(ref env_name)
            if with_cockpit_cancel(
                &board,
                session_created_at,
                async { Ok(is_sandbox_live(&os, env_name).await) },
            )
            .await? =>
        {
            (env_name.clone(), true)
        }
        other => {
            let new_name = other
                .as_ref()
                .filter(|n| !n.is_empty())
                .cloned()
                .unwrap_or(default_name);
            if let Some(prev) = other {
                if prev != new_name {
                    // Background: do not block seat cancel on gateway delete.
                    spawn_reap_cockpit_sandbox(os.clone(), board.clone(), prev.clone());
                }
            }
            ensure_cockpit_session_running(&board, Some(session_created_at))?;
            let live =
                with_cockpit_cancel(
                    &board,
                    session_created_at,
                    async { Ok(is_sandbox_live(&os, &new_name).await) },
                )
                .await?;
            if live {
                // Reuse: sandbox already answers exec — safe to publish env now.
                board
                    .update_cockpit_session(Some(new_name.clone()), None)
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                let _ = board.set_cockpit_sandbox_phase(
                    CockpitSandboxPhase::Provisioning,
                    Some("Reconnecting to existing sandbox".into()),
                );
                (new_name, true)
            } else {
                // Fresh create: clear stale env + conversation now, but do **not**
                // publish `environment` until Ready — Cockpit attach keys off that
                // field and will hammer exec with "sandbox is not ready" otherwise.
                board
                    .update_cockpit_session(Some(String::new()), Some(String::new()))
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                let _ = board.set_cockpit_sandbox_phase(
                    CockpitSandboxPhase::Starting,
                    Some("Preparing cockpit sandbox".into()),
                );
                (new_name, false)
            }
        }
    };

    let spec = sandbox_spec_for_cockpit(&name, &resolved, &attach, &engine);
    let result = run_cockpit_inside(
        &board,
        &os,
        &agents,
        &name,
        &spec,
        &engine,
        is_reused,
        session_created_at,
    )
    .await;
    Ok((name, result))
}

/// Wait until a cockpit sandbox name is free to create.
///
/// Stop reaps asynchronously; Start must not race `create` against an in-flight
/// gateway delete (or a still-live survivor).
async fn wait_cockpit_name_free(
    board: &SharedBoard,
    os: &OpenShell,
    name: &str,
    expected_created_at: DateTime<Utc>,
) -> anyhow::Result<()> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(180);
    let mut kicked = false;
    let mut announced = false;
    loop {
        ensure_cockpit_session_running(board, Some(expected_created_at))?;
        let inflight = cockpit_delete_inflight().lock().contains(name);
        let present = match os.list_cockpit().await {
            Ok(sandboxes) => cockpit_sandbox_name_is_present(&sandboxes, name),
            Err(err) => {
                tracing::debug!(error = %err, "could not inspect cockpit sandbox name");
                if tokio::time::Instant::now() >= deadline {
                    anyhow::bail!("could not inspect cockpit sandbox {name} before timeout");
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
                continue;
            }
        };
        if !inflight && !present {
            return Ok(());
        }
        if !announced {
            announced = true;
            let _ = board.set_cockpit_sandbox_phase(
                CockpitSandboxPhase::WaitingForDelete,
                Some("Waiting for previous sandbox to finish deleting".into()),
            );
        }
        if tokio::time::Instant::now() >= deadline {
            anyhow::bail!("cockpit sandbox {name} still present after stop");
        }
        if present && !inflight && !kicked {
            // Orphan or failed sandbox left after a previous attempt — kick one delete.
            spawn_reap_cockpit_sandbox_force(os.clone(), board.clone(), name.to_string());
            kicked = true;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_cockpit_inside(
    board: &SharedBoard,
    os: &OpenShell,
    _cfg: &AgentConfig,
    name: &str,
    spec: &SandboxSpec,
    _engine: &str,
    is_reused: bool,
    session_created_at: DateTime<Utc>,
) -> anyhow::Result<()> {
    let short = Duration::from_secs(180);

    if !is_reused {
        with_cockpit_cancel(
            board,
            session_created_at,
            wait_cockpit_name_free(board, os, name, session_created_at),
        )
        .await?;
        let _ = board.set_cockpit_sandbox_phase(
            CockpitSandboxPhase::Provisioning,
            Some("Creating cockpit sandbox".into()),
        );
        with_cockpit_cancel(
            board,
            session_created_at,
            async { os.create(spec).await.map_err(Into::into) },
        )
        .await?;
        let _ = board.set_cockpit_sandbox_phase(
            CockpitSandboxPhase::Provisioning,
            Some("Waiting for sandbox to become Ready".into()),
        );
        with_cockpit_cancel(
            board,
            session_created_at,
            wait_until_sandbox_ready(os, name),
        )
        .await?;
        let _ = with_cockpit_cancel(
            board,
            session_created_at,
            exec_with_infra_retry(os, name, &empty_workdir_script(), short, "cockpit workdir"),
        )
        .await?;
        // Publish environment only once the box can take attach/MCP exec.
        board
            .update_cockpit_session(Some(name.to_string()), None)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        tracing::info!("cockpit: environment {name} published (sandbox Ready)");
    } else {
        // Reused boxes can still be mid-relay settle after a quick Stop/Start.
        let _ = board.set_cockpit_sandbox_phase(
            CockpitSandboxPhase::Provisioning,
            Some("Reusing cockpit sandbox".into()),
        );
        with_cockpit_cancel(
            board,
            session_created_at,
            wait_until_sandbox_ready(os, name),
        )
        .await?;
    }

    // Start the cockpit MCP relay (nc -lU over exec_interactive) before
    // minting/injecting mcp.json so the agent's stdio config has somewhere
    // to connect.
    let _ = board.set_cockpit_sandbox_phase(
        CockpitSandboxPhase::Provisioning,
        Some("Starting MCP relay".into()),
    );
    with_cockpit_cancel(
        board,
        session_created_at,
        async {
            crate::cockpit_mcp_tunnel::ensure_cockpit_mcp_tunnel(os, board, name)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))
        },
    )
    .await?;

    // Inject MCP Bearer + mcp.json so Cockpit's interactive `agent` (and any
    // manual host attach) can call host /mcp without browser OAuth. Subject
    // `cockpit` is the supervisor fallback when no human cookie is in play.
    if let Err(e) = with_cockpit_cancel(
        board,
        session_created_at,
        async {
            crate::cockpit_mcp::provision_cockpit_mcp(
                board,
                os,
                name,
                crate::cockpit_mcp::COCKPIT_FALLBACK_SUB,
            )
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))
        },
    )
    .await
    {
        tracing::warn!("cockpit: MCP provision failed (continuing): {e}");
    }

    // Cockpit attach owns the interactive agent. Do not start a competing
    // headless seat — stop any leftover detached process from older builds.
    stop_agent(os, name).await;
    let _ = board.set_cockpit_sandbox_phase(CockpitSandboxPhase::Ready, None);
    tracing::info!("cockpit: sandbox {name} ready for Cockpit attach");

    // Hold while Board says Running so the outer loop does not re-materialize
    // every few seconds. Park / Stop cancel via ensure_cockpit_running.
    loop {
        tokio::time::sleep(Duration::from_secs(2)).await;
        ensure_cockpit_session_running(board, Some(session_created_at))?;
        if !is_sandbox_live(os, name).await {
            anyhow::bail!("{COCKPIT_CANCEL_UNUSABLE}");
        }
    }
}

/// Former headless cockpit watcher (conversation scrape). Cockpit attach owns the
/// interactive agent now; kept for reference/tests of stream-json parsing.
#[allow(dead_code)]
async fn watch_cockpit_agent(
    board: &SharedBoard,
    os: &OpenShell,
    cfg: &AgentConfig,
    name: &str,
    from_line: u64,
) -> anyhow::Result<Output> {
    let timeout = Duration::from_secs(cfg.agent_timeout_secs) + Duration::from_secs(120);
    let board2 = board.clone();
    let follow = follow_script(from_line);
    let stream = os.exec_streaming(name, &follow, timeout, move |line| {
        if let Some(cid) = parse_conversation_id(line) {
            let _ = board2.update_cockpit_session(None, Some(cid));
        }
    });
    tokio::pin!(stream);

    let mut poll = tokio::time::interval(watch_board_poll());
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    poll.tick().await;

    loop {
        tokio::select! {
            res = &mut stream => return res.map_err(Into::into),
            _ = poll.tick() => ensure_cockpit_running(board)?,
        }
    }
}

async fn finalize_cockpit(
    os: &OpenShell,
    board: &SharedBoard,
    name: &str,
    result: &anyhow::Result<()>,
) {
    match result {
        Ok(_) => {
            crate::cockpit_mcp_tunnel::stop_cockpit_mcp_tunnel(os).await;
            stop_agent(os, name).await;
            tracing::info!("cockpit: agent finished in {name}; sandbox kept");
        }
        Err(e) if is_supervisor_detach(&e.to_string()) => {
            tracing::info!("cockpit: detaching from {name} without stopping the agent: {e}");
        }
        Err(e) if is_cockpit_parked(&e.to_string()) => {
            crate::cockpit_mcp_tunnel::stop_cockpit_mcp_tunnel(os).await;
            stop_agent(os, name).await;
            tracing::info!("cockpit: parked; stopped agent in {name}, sandbox kept");
        }
        Err(e) if is_cockpit_superseded(&e.to_string()) => {
            tracing::info!("cockpit: superseded seat exited without cleanup: {e}");
        }
        Err(e) if is_cockpit_unusable(&e.to_string()) => {
            crate::cockpit_mcp_tunnel::stop_cockpit_mcp_tunnel(os).await;
            stop_agent(os, name).await;
            let _ = board.set_cockpit_sandbox_phase(
                CockpitSandboxPhase::Starting,
                Some("Recreating unusable cockpit sandbox".into()),
            );
            spawn_reap_cockpit_sandbox_force(os.clone(), board.clone(), name.to_string());
            tracing::warn!("cockpit: sandbox {name} became unusable; reaping for recreation: {e}");
        }
        Err(e) if is_cockpit_stopped(&e.to_string()) => {
            // Stop cleared the session; Start may have already created a new one.
            // Do not stop_agent/delete under a session that wants this box.
            if cockpit_session_wants_sandbox(board, name) {
                tracing::info!(
                    "cockpit: stop cleanup skipped for {name}; session active again"
                );
                return;
            }
            crate::cockpit_mcp_tunnel::stop_cockpit_mcp_tunnel(os).await;
            stop_agent(os, name).await;
            if cockpit_session_wants_sandbox(board, name) {
                tracing::info!(
                    "cockpit: stop cleanup skipped for {name} after stop_agent; session active again"
                );
                return;
            }
            spawn_reap_cockpit_sandbox(os.clone(), board.clone(), name.to_string());
            tracing::info!("cockpit: stopped; reaping sandbox {name}");
        }
        Err(e) => {
            // Always tear the uplink pool — readiness failures used to leave it
            // retrying `sandbox not found` forever after a later Stop/reap.
            crate::cockpit_mcp_tunnel::stop_cockpit_mcp_tunnel(os).await;
            stop_agent(os, name).await;
            tracing::error!("cockpit: keeping sandbox {name} for inspection: {e}");
        }
    }
}

fn push_sandbox_prompt_section(b: &mut String, prompt: Option<&str>) {
    if let Some(p) = prompt.filter(|s| !s.trim().is_empty()) {
        b.push_str("Sandbox prompt (seat notes):\n");
        b.push_str(p.trim());
        b.push('\n');
    }
}

/// Briefing for a fresh Cockpit interactive `agent` (and tests).
pub(crate) fn cockpit_briefing(sandbox_prompt: Option<&str>) -> String {
    let mut b = String::new();
    b.push_str(
        "You are the privileged control-plane cockpit for sandboard — the human's liaison \
         over operator MCP tools. You are not a card worker.\n\n",
    );
    b.push_str(
        "Host sandboard MCP is preconfigured in `mcp.json`/`claude_mcp.json` — stdio, no login, \
         no Bearer. Do **not** run browser OAuth inside this sandbox. That endpoint is \
         operator tools only: board_snapshot, dispatch, park, steer, approve_*, \
         answer_escalation, and related triage tools. Do not call worker verbs (claim, \
         heartbeat, report, report_pull_request, split, escalate, release, list_ready) — they are denied on \
         this seat.\n\n",
    );
    b.push_str(
        "Every mutation goes through the Board. Chat is a face; do not invent a second \
         lifecycle. Prefer escalating ambiguous irreversibles; approving merges stays human.\n\n",
    );
    b.push_str(
        "Start with board_snapshot. Triage Needs You first, then Review. Interrupt the \
         human only for irreversible actions, ambiguity blocking several items, or repeated \
         failure on the same card.\n\n",
    );
    b.push_str(
        "When creating a Project, `create_project` requires `clone_repo` (`owner/name`) — \
         the repository Initial plan clones for planning. Optional `project_prompt` is \
         Project-only standing extras; board-wide policy is Settings → Agent runtime \
         standing prompt. Do not dispatch Initial plan until that clone target is set.\n\n",
    );
    b.push_str(
        "Configuration stacks in layers: process boot and board Settings (Policies, sandbox \
         specs, agent runtime including standing prompt, Forge) are host/operator setup; \
         Project fields (`clone_repo`, optional sandbox override) seed the Initial plan; \
         `project_prompt` carries Project-only standing extras; per-card intent/DoD names \
         clone targets and card-specific gates. Boot, Settings, and Project fields do not \
         belong in `project_prompt`. Name test/lint commands in the board standing prompt or \
         `project_prompt` when they apply broadly; sandboard does not assume cargo or any \
         toolchain unless those instructions or a card's DoD name it.\n",
    );
    push_sandbox_prompt_section(&mut b, sandbox_prompt);
    b
}

/// Legacy park-resume copy; Cockpit uses `--resume` instead of a cold briefing.
#[allow(dead_code)]
fn cockpit_resume_briefing() -> String {
    "You were parked mid-session. The agent process was stopped; the sandbox and \
     conversation were kept. Continue as the cockpit over host sandboard MCP \
     (stdio, preconfigured in mcp.json) — operator tools only, no worker verbs, \
     no browser OAuth. Start with board_snapshot.\n"
        .into()
}

/// Durable cockpit loop: start / reconcile / park-stop. Independent of
/// `dispatch_loop` and the card claim queue.
async fn cockpit_seat_loop(board: SharedBoard, _cfg: ExecutionConfig) {
    let supervising = Arc::new(AtomicBool::new(false));

    // Bounded wait for the gateway — same grace as card reconcile.
    let deadline = std::time::Instant::now() + GATEWAY_GRACE;
    let mut announced = false;
    while !board.openshell_client().healthy().await {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            tracing::error!(
                "cockpit: gateway unreachable after {}s; continuing without startup adopt",
                GATEWAY_GRACE.as_secs()
            );
            break;
        }
        if !announced {
            tracing::warn!("cockpit: gateway unreachable; holding until it answers");
            announced = true;
        }
        tokio::time::sleep(GATEWAY_POLL.min(left)).await;
    }

    {
        let os = board.openshell_client();
        reconcile_cockpit_inventory(&os, &board).await;
    }

    let mut tick = tokio::time::interval(Duration::from_secs(3));
    loop {
        tick.tick().await;
        let os = board.openshell_client();
        reconcile_cockpit_inventory(&os, &board).await;

        let Some(session) = board.cockpit_session() else {
            continue;
        };
        if session.status != CockpitSessionStatus::Running {
            // Parked: ensure agent is stopped when we are not mid-watch.
            if !supervising.load(Ordering::Relaxed) {
                if let Some(env) = session.environment.as_deref() {
                    stop_agent(&os, env).await;
                }
            }
            continue;
        }
        if supervising.load(Ordering::Relaxed) {
            continue;
        }
        if !os.healthy().await {
            tracing::warn!("cockpit: gateway unhealthy; not starting");
            continue;
        }

        supervising.store(true, Ordering::Relaxed);
        let board2 = board.clone();
        let flag = supervising.clone();
        tokio::spawn(async move {
            let os = board2.openshell_client();
            match run_cockpit_seat(board2.clone()).await {
                Ok((name, result)) => {
                    match &result {
                        Ok(()) => tracing::info!("cockpit: run completed"),
                        Err(e)
                            if is_cockpit_parked(&e.to_string())
                                || is_cockpit_stopped(&e.to_string())
                                || is_cockpit_superseded(&e.to_string())
                                || is_cockpit_unusable(&e.to_string()) =>
                        {
                            tracing::info!("cockpit: {e}");
                        }
                        Err(e) if is_supervisor_detach(&e.to_string()) => {
                            tracing::info!("cockpit: supervisor detached ({e})");
                        }
                        Err(e) if is_infrastructure(&e.to_string()) => {
                            tracing::warn!("cockpit: infrastructure failure: {e}");
                            let _ = board2.set_cockpit_sandbox_phase(
                                CockpitSandboxPhase::Error,
                                Some(format!("Infrastructure: {e}")),
                            );
                        }
                        Err(e) => {
                            tracing::error!("cockpit failed: {e}");
                            let _ = board2.set_cockpit_sandbox_phase(
                                CockpitSandboxPhase::Error,
                                Some(e.to_string()),
                            );
                        }
                    }
                    // Release before finalize: gateway delete must not block Stop→Start.
                    flag.store(false, Ordering::Relaxed);
                    finalize_cockpit(&os, &board2, &name, &result).await;
                }
                Err(e)
                    if is_cockpit_parked(&e.to_string())
                        || is_cockpit_stopped(&e.to_string())
                        || is_cockpit_superseded(&e.to_string()) =>
                {
                    tracing::info!("cockpit: {e}");
                    flag.store(false, Ordering::Relaxed);
                }
                Err(e) if is_supervisor_detach(&e.to_string()) => {
                    tracing::info!("cockpit: supervisor detached ({e})");
                    flag.store(false, Ordering::Relaxed);
                }
                Err(e) if is_infrastructure(&e.to_string()) => {
                    tracing::warn!("cockpit: infrastructure failure: {e}");
                    let _ = board2.set_cockpit_sandbox_phase(
                        CockpitSandboxPhase::Error,
                        Some(format!("Infrastructure: {e}")),
                    );
                    flag.store(false, Ordering::Relaxed);
                }
                Err(e) => {
                    tracing::error!("cockpit failed: {e}");
                    let _ = board2.set_cockpit_sandbox_phase(
                        CockpitSandboxPhase::Error,
                        Some(e.to_string()),
                    );
                    flag.store(false, Ordering::Relaxed);
                }
            }
        });
    }
}

// ----------------------------------------------------------------- helpers

/// Single-quote for `bash -lc`. A briefing is untrusted text as far as the
/// shell is concerned — it contains human prose, quotes and newlines.
pub(crate) fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Pull a resume handle out of an engine output line, if present.
///
/// Pointer keys come from the engine registry (agy `conversation_id`, Cursor
/// `session_id`, …). Hermes emits its session id as a plain `session_id:` footer
/// on stderr, so accept that shape too. Tolerant across engines so one parser
/// serves supervisor and cockpit chat.
pub(crate) fn parse_conversation_id(line: &str) -> Option<String> {
    let trimmed = line.trim();
    if let Some(id) = trimmed.strip_prefix("session_id:") {
        let id = id.trim();
        if !id.is_empty() {
            return Some(id.to_string());
        }
    }

    let v: serde_json::Value = serde_json::from_str(trimmed).ok()?;
    for key in crate::engine::conversation_id_pointers() {
        if let Some(s) = v.pointer(key).and_then(|x| x.as_str()) {
            let t = s.trim();
            if !t.is_empty() {
                return Some(t.to_string());
            }
        }
    }
    None
}

/// Both streams. git writes its actual error to stderr, so reporting only
/// stdout produced `push failed:` with nothing after the colon — a failure
/// message that says less than no message at all.
fn outerr(o: &crate::openshell::Output) -> String {
    let mut s = o.stderr.trim().to_string();
    if !o.stdout.trim().is_empty() {
        if !s.is_empty() {
            s.push('\n');
        }
        s.push_str(o.stdout.trim());
    }
    tail(&s, 500)
}

fn tail(s: &str, n: usize) -> String {
    let t = s.trim();
    if t.len() <= n {
        return t.to_string();
    }
    t[t.len() - n..].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Origin;

    #[test]
    fn board_poll_is_frequent_enough_to_notice_halt() {
        assert!(watch_board_poll() <= Duration::from_secs(5));
        assert!(watch_board_poll() <= Duration::from_millis(50));
    }

    #[test]
    fn only_claimed_or_running_keeps_the_watch() {
        assert!(board_still_owns_run(State::Claimed));
        assert!(board_still_owns_run(State::Running));
        assert!(!board_still_owns_run(State::Backlog));
        assert!(!board_still_owns_run(State::NeedsHuman));
        assert!(!board_still_owns_run(State::Done));
    }

    #[test]
    fn reconcile_keeps_compact_and_legacy_card_sandbox_names() {
        let board = test_board();
        let project = board
            .create(None, "project", "why", None, Origin::Human, true, None)
            .unwrap();
        let task = board
            .create(
                Some(project.id),
                "task",
                "intent",
                Some("dod".into()),
                Origin::Human,
                false,
                None,
            )
            .unwrap();
        let _ = board.transition(task.id, State::Shaping, "test", None);
        let _ = board.transition(task.id, State::Backlog, "test", None);
        let _ = board.claim(task.id, "agent-1", None, 60).unwrap();
        board.set_environment(
            task.id,
            Some(crate::schema::card_sandbox_name(task.id, 1)),
        );
        let item = board.get(task.id).unwrap();

        assert!(should_keep_sandbox(
            Some(&item),
            &crate::schema::card_sandbox_name(task.id, 1)
        ));
        assert!(should_keep_sandbox(
            Some(&item),
            &crate::schema::legacy_card_sandbox_stem(task.id)
        ));
    }

    #[test]
    fn board_release_is_not_a_card_failure() {
        assert!(is_supervisor_cancel(
            "run cancelled: card left Backlog (deadline exceeded or halted)"
        ));
        assert!(!is_supervisor_cancel("clone failed: CONNECT tunnel 403"));
        assert!(!is_infrastructure(
            "run cancelled: card left Backlog (deadline exceeded or halted)"
        ));
    }

    #[test]
    fn ctrl_c_follower_exit_is_detach_not_failure() {
        assert!(is_supervisor_detach(
            "agent exited -1: dbox_policies_to_inline(), 0);\\n..."
        ));
        assert!(is_supervisor_detach("agent exited 130: interrupted"));
        assert!(is_supervisor_detach("agent exited 143: killed"));
        assert!(!is_supervisor_detach("agent exited 1: panic"));
        assert!(!is_supervisor_detach("clone failed: CONNECT tunnel 403"));
        assert!(!is_supervisor_cancel("agent exited -1: noise"));
        assert!(!is_infrastructure("agent exited -1: noise"));
    }

    /// Halt mid-setup used to leave `in_flight` stuck: clone/create ignored the
    /// board, so `max_concurrent` never freed and Backlog cards sat forever.
    #[tokio::test]
    async fn setup_await_cancels_when_card_is_halted() {
        let board = test_board();
        let project = board
            .create(None, "project", "why", None, Origin::Human, true, None)
            .unwrap();
        let task = board
            .create(
                Some(project.id),
                "task",
                "intent",
                Some("dod".into()),
                Origin::Human,
                false,
                None,
            )
            .unwrap();
        let _ = board.transition(task.id, State::Shaping, "test", None);
        let _ = board.transition(task.id, State::Backlog, "test", None);
        let _ = board.claim(task.id, "agent-1", None, 60).unwrap();

        let board_halt = board.clone();
        let id = task.id;
        let halt = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(80)).await;
            board_halt.halt(id, Some("test halt".into())).expect("halt");
        });

        let began = std::time::Instant::now();
        let err = with_board_cancel(&board, id, async {
            // Never completes on its own — cancel must win when halt lands.
            std::future::pending::<Result<(), anyhow::Error>>().await
        })
        .await
        .expect_err("must cancel when halted");
        halt.await.expect("halt task");

        assert!(
            began.elapsed() < Duration::from_secs(2),
            "cancel must not wait out the setup future"
        );
        assert!(
            is_supervisor_cancel(&err.to_string()),
            "expected supervisor cancel, got {err}"
        );
        assert_eq!(board.get(id).unwrap().state, State::Backlog);
    }

    #[tokio::test]
    async fn setup_await_cancels_when_card_is_parked() {
        let board = test_board();
        let project = board
            .create(None, "project", "why", None, Origin::Human, true, None)
            .unwrap();
        let task = board
            .create(
                Some(project.id),
                "task",
                "intent",
                Some("dod".into()),
                Origin::Human,
                false,
                None,
            )
            .unwrap();
        let _ = board.transition(task.id, State::Shaping, "test", None);
        let _ = board.transition(task.id, State::Backlog, "test", None);
        let _ = board.claim(task.id, "agent-1", None, 60).unwrap();
        board.set_conversation_id(task.id, Some("conv-keep".into()));
        board.set_environment(task.id, Some("sandboard-card-park-a1".into()));

        let board_park = board.clone();
        let id = task.id;
        let park = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(80)).await;
            board_park.park(id, Some("test park".into())).expect("park");
        });

        let err = with_board_cancel(&board, id, async {
            std::future::pending::<Result<(), anyhow::Error>>().await
        })
        .await
        .expect_err("must cancel when parked");
        park.await.expect("park task");

        assert!(is_supervisor_cancel(&err.to_string()), "got {err}");
        let it = board.get(id).unwrap();
        assert_eq!(it.state, State::Backlog);
        assert_eq!(it.conversation_id.as_deref(), Some("conv-keep"));
        assert_eq!(it.environment.as_deref(), Some("sandboard-card-park-a1"));
    }

    #[tokio::test]
    async fn setup_await_cancels_when_card_is_retired() {
        let board = test_board();
        let project = board
            .create(None, "project", "why", None, Origin::Human, true, None)
            .unwrap();
        let task = board
            .create(
                Some(project.id),
                "task",
                "intent",
                Some("dod".into()),
                Origin::Human,
                false,
                None,
            )
            .unwrap();
        let _ = board.transition(task.id, State::Shaping, "test", None);
        let _ = board.transition(task.id, State::Backlog, "test", None);
        let _ = board.claim(task.id, "agent-1", None, 60).unwrap();

        let board_cut = board.clone();
        let id = task.id;
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(80)).await;
            let _ = board_cut.cut_scope(id, Some("reshape".into()));
        });

        let err = with_board_cancel(&board, id, async {
            std::future::pending::<Result<(), anyhow::Error>>().await
        })
        .await
        .expect_err("must cancel when retired");
        assert!(is_supervisor_cancel(&err.to_string()), "{err}");
        assert_eq!(board.get(id).unwrap().state, State::Retired);
    }

    #[test]
    fn briefings_quote_safely_for_bash() {
        let nasty = "it's \"quoted\"; rm -rf /";
        let q = shell_quote(nasty);
        assert!(q.starts_with('\'') && q.ends_with('\''));
        assert!(q.contains(r"'\''"), "single quotes must be escaped: {q}");
    }

    /// Reusing a sandbox must resume the card branch, not start over. Always
    /// branching from base meant the push was rejected as non-fast-forward
    /// against the card's own earlier work — which is exactly the
    /// "changes requested, go fix it" path.
    #[test]
    fn refresh_resumes_an_existing_branch() {
        let cfg = repo_cfg();
        let s = refresh_script(&cfg, "sandboard/card-8");
        assert!(
            s.contains("ls-remote --exit-code --heads origin sandboard/card-8"),
            "{s}"
        );
        assert!(
            s.contains("checkout -q -B sandboard/card-8 origin/sandboard/card-8"),
            "{s}"
        );
        assert!(s.contains("rebase -q upstream/main"), "{s}");
        // A conflict is the agent's problem to resolve, so the supervisor must
        // back out rather than leave a half-applied rebase behind.
        assert!(s.contains("rebase --abort"), "{s}");
    }

    /// Nothing syncs the fork, so its own base freezes at the moment it was
    /// created while upstream moves on — 6 commits, within a day, in practice.
    /// Rebasing onto the fork's base would tell the agent it was current when
    /// it was not, and produce a PR that conflicts with what it targets.
    #[test]
    fn refresh_base_comes_from_upstream_not_the_fork() {
        let s = refresh_script(&repo_cfg(), "sandboard/card-8");
        assert!(
            s.contains("git remote add upstream https://github.com/sandboard-app/sandboard.git"),
            "{s}"
        );
        assert!(s.contains("fetch -q upstream main"), "{s}");
        assert!(s.contains("rebase -q upstream/main"), "{s}");
        // Missing local branch still starts from upstream, not the stale fork.
        assert!(
            s.contains("checkout -q -B sandboard/card-8 upstream/main"),
            "{s}"
        );
        assert!(
            !s.contains("rebase -q origin/main"),
            "must not rebase onto the fork: {s}"
        );
    }

    /// The supervisor asks GitHub a question; it does not create anything.
    /// Every publish failure today came from our shell being wrong about a
    /// tool the agent already knows how to drive.
    #[test]
    fn the_supervisor_only_looks_up_the_pr() {
        let s = pr_lookup_script(&repo_cfg(), "sandboard/card-8");
        assert!(s.contains("gh pr list"), "{s}");
        assert!(
            s.contains("--head sandboard/card-8 --search 'head:clankrshq:sandboard/card-8'"),
            "cross-fork needs a supported head search qualifier: {s}"
        );
        assert!(
            s.contains(PR_URL_MARK),
            "url must come from a marked line: {s}"
        );
        assert!(
            !s.contains("gh pr create"),
            "supervisor looks up PRs; agent creates them: {s}"
        );
        assert!(
            !s.contains("push"),
            "supervisor looks up PRs; agent pushes: {s}"
        );
    }

    #[test]
    fn cross_fork_pr_lookup_uses_gh_supported_head_search() {
        let s = pr_lookup_script(&repo_cfg(), "sandboard/card-8");
        assert!(
            s.contains("--head sandboard/card-8 --search 'head:clankrshq:sandboard/card-8'"),
            "cross-fork lookup must use a separate head search qualifier: {s}"
        );
        assert!(
            !s.contains("--head clankrshq:sandboard/card-8"),
            "gh no longer accepts owner:branch in --head: {s}"
        );
    }

    /// If the agent did not open a PR, the card must not reach Review looking
    /// finished — a Review card you cannot open is not a review.
    #[test]
    fn no_pr_means_no_url_to_report() {
        let s = pr_lookup_script(&repo_cfg(), "sandboard/card-8");
        assert!(
            s.contains("// empty"),
            "must yield nothing rather than error: {s}"
        );
    }

    /// Publishing moved into the agent's job, so the briefing is now the only
    /// place that says how. If it stops saying it, nothing pushes at all.
    #[test]
    fn the_briefing_tells_the_agent_to_publish() {
        let b = briefing(
            &grant(),
            BranchState::Fresh,
            "sandboard/card-7",
            &cross_fork_repo(),
        );
        assert!(b.contains("sandboard/card-7"), "must name the branch: {b}");
        assert!(b.contains("sandboard-app/sandboard"), "must name the PR target: {b}");
        assert!(b.to_lowercase().contains("push"), "{b}");
        assert!(b.to_lowercase().contains("pull request"), "{b}");
    }

    /// Fresh + complete repo: agent clones into an empty `/sandbox/repo`.
    #[test]
    fn complete_repo_fresh_briefing_makes_agent_clone() {
        let b = briefing(
            &grant(),
            BranchState::Fresh,
            "sandboard/card-7",
            &cross_fork_repo(),
        );
        assert!(
            b.contains("`/sandbox/repo` is empty")
                || b.contains("Clone into `/sandbox/repo`")
                || b.contains("Clone using the Remotes"),
            "must tell the agent to clone into an empty workdir: {b}"
        );
        assert!(
            b.contains("empty workspace on claim"),
            "cold-start Remotes must claim empty workdir: {b}"
        );
        assert!(
            !b.contains("You are on a new branch off the base"),
            "must not imply a pre-populated checkout: {b}"
        );
    }

    /// Cold-start Remotes claim an empty workdir; reclaim Remotes (park resume
    /// via `resume_briefing`, Needs You answer via Rebased) preserve it.
    #[test]
    fn briefing_remotes_split_cold_start_empty_vs_reuse_preserve() {
        let repo = cross_fork_repo();
        let cold = briefing(&grant(), BranchState::Fresh, "sandboard/card-7", &repo);
        assert!(
            cold.contains("empty workspace on claim") || cold.contains("`/sandbox/repo` is empty"),
            "Fresh cold-start must describe empty `/sandbox/repo`: {cold}"
        );
        assert!(
            !cold.contains("was preserved on this reclaim"),
            "Fresh must not use reclaim preserve copy: {cold}"
        );

        // Needs You answer reclaim: full briefing with Rebased (shared is_reused).
        let reclaim = briefing(&grant(), BranchState::Rebased, "sandboard/card-7", &repo);
        assert!(
            reclaim.contains("was preserved on this reclaim"),
            "Rebased reclaim must describe preserved workdir: {reclaim}"
        );
        assert!(
            reclaim.contains("clone only if the checkout is missing")
                || reclaim.contains("clone only if"),
            "reclaim must not order a blanket clone: {reclaim}"
        );
        assert!(
            reclaim.contains("Do not wipe-and-clone") || reclaim.contains("do not wipe-and-clone"),
            "reclaim must forbid wipe-and-clone: {reclaim}"
        );
        assert!(
            !reclaim.contains("empty workspace on claim"),
            "reclaim Remotes must not claim empty workspace: {reclaim}"
        );
        assert!(
            !reclaim.contains("`/sandbox/repo` is empty"),
            "reclaim must not claim blank workdir: {reclaim}"
        );

        // Park resume: short resume_briefing — same preserve Remotes contract.
        let parked = resume_briefing(&grant(), &repo);
        assert!(
            parked.contains("was preserved on this reclaim"),
            "park resume Remotes must preserve workdir: {parked}"
        );
        assert!(
            !parked.contains("empty workspace on claim"),
            "park resume must not claim empty workspace: {parked}"
        );
        assert!(
            parked.contains("park resume and Needs You answer reclaim share this path"),
            "park and Needs You must share one reuse Remotes contract: {parked}"
        );
        assert!(
            reclaim.contains("park resume and Needs You answer reclaim share this path"),
            "Needs You reclaim must name the shared path: {reclaim}"
        );
    }

    /// Conflicted reclaim still gets CONFLICTS copy, but Remotes stay preserve
    /// (not empty-on-claim) — same is_reused path as park / Needs You.
    #[test]
    fn conflicted_reclaim_remotes_preserve_not_empty() {
        let b = briefing(
            &grant(),
            BranchState::Conflicted,
            "sandboard/card-7",
            &cross_fork_repo(),
        );
        assert!(b.contains("CONFLICTS"), "{b}");
        assert!(
            b.contains("was preserved on this reclaim"),
            "Conflicted reclaim must preserve workdir: {b}"
        );
        assert!(
            !b.contains("empty workspace on claim"),
            "Conflicted must not claim empty workspace: {b}"
        );
    }

    /// Briefing must not invent cargo — gates live in the Project prompt.
    #[test]
    fn briefing_does_not_invent_cargo_gates() {
        let b = briefing(
            &grant(),
            BranchState::Fresh,
            "acme/card-7",
            &cross_fork_repo(),
        );
        assert!(
            !b.contains("cargo test --offline"),
            "must not mandate cargo test: {b}"
        );
        assert!(
            !b.contains("cargo clippy"),
            "must not mandate cargo clippy: {b}"
        );
        assert!(
            b.contains("Do not assume cargo"),
            "must point at Board prompt for gates: {b}"
        );
        assert!(
            b.contains("/sandbox/.sandboard/report.json"),
            "verdict path invariant: {b}"
        );
    }

    #[test]
    fn briefing_presents_plan_before_standing_prompts() {
        let b = briefing(
            &grant(),
            BranchState::Fresh,
            "sandboard/card-7",
            &cross_fork_repo(),
        );
        let plan_pos = b.find("Project Plan").expect("must have Plan section");
        let protocol_pos = b
            .find("Protocol (hardwired):")
            .expect("must have protocol heading");
        let board_pos = b
            .find("Board prompt (standing agent policy)")
            .expect("must have board_prompt heading");
        let prompt_pos = b
            .find("Project prompt (Project standing extras)")
            .expect("must have project_prompt heading");
        assert!(
            plan_pos < protocol_pos && protocol_pos < board_pos && board_pos < prompt_pos,
            "Plan then protocol then board then project: plan={plan_pos} protocol={protocol_pos} board={board_pos} project={prompt_pos}"
        );
        assert!(
            b.contains("board-wide quality gates live in the Board prompt"),
            "must point at board prompt for gates: {b}"
        );
        assert!(
            b.contains("card-specific gates live in this card's definition of done"),
            "must point at DoD for card gates: {b}"
        );
    }

    #[test]
    fn card_names_are_fixed_sandboard_stem() {
        assert_eq!(crate::schema::card_branch_name(173), "sandboard/card-173");
        assert_eq!(
            crate::schema::card_sandbox_name(173, 2),
            "sb-card-173-a2"
        );
        let b = briefing(
            &grant(),
            BranchState::Fresh,
            &crate::schema::card_branch_name(7),
            &cross_fork_repo(),
        );
        assert!(
            b.contains("sandboard/card-7"),
            "briefing must name the card branch: {b}"
        );
    }

    #[test]
    fn branch_state_is_read_from_the_clone_output() {
        assert_eq!(branch_state_of("SANDBOARD-BRANCH-FRESH\n"), BranchState::Fresh);
        assert_eq!(
            branch_state_of("noise\nSANDBOARD-BRANCH-REBASED\n"),
            BranchState::Rebased
        );
        assert_eq!(
            branch_state_of("SANDBOARD-BRANCH-CONFLICT\n"),
            BranchState::Conflicted
        );
        // Unrecognised output must not silently claim a clean rebase.
        assert_eq!(branch_state_of("something else"), BranchState::Fresh);
    }

    /// An agent resuming a conflicted branch has to be told, or it will build
    /// on top of a branch that cannot merge.
    #[test]
    fn the_briefing_tells_the_agent_about_a_conflict() {
        let conflicted = briefing(
            &grant(),
            BranchState::Conflicted,
            "sandboard/card-7",
            &cross_fork_repo(),
        );
        assert!(conflicted.contains("CONFLICTS"), "{conflicted}");
        assert!(
            conflicted.to_lowercase().contains("resolve"),
            "{conflicted}"
        );
        // Fork base freezes; rebase onto upstream, never origin/<base> as the target.
        assert!(
            conflicted.contains("upstream/main"),
            "must say rebase onto upstream/<base>: {conflicted}"
        );
        assert!(
            conflicted.to_lowercase().contains("never") && conflicted.contains("origin/main"),
            "must warn against rebasing onto the fork base: {conflicted}"
        );
        assert!(
            !conflicted.contains("rebase onto `origin/main`")
                && !conflicted.contains("rebase onto origin/main"),
            "must not instruct rebase onto the fork base: {conflicted}"
        );

        let fresh = briefing(
            &grant(),
            BranchState::Fresh,
            "sandboard/card-7",
            &cross_fork_repo(),
        );
        assert!(!fresh.contains("CONFLICTS"));
        assert!(
            !fresh.contains("You are on a new branch off the base"),
            "Fresh is empty-workdir clone, not pre-branched: {fresh}"
        );
    }

    /// Conflicted + conversation resume still gets the cold CONFLICTS briefing;
    /// only a clean park mid-run uses the short resume prompt. The resume flag
    /// (conversation id passed to start_script) is independent.
    #[test]
    fn conflicted_branch_uses_cold_briefing_even_when_resuming_conversation() {
        let mut g = grant();
        g.notes = vec!["Parked: cargo test deadlocked.".into()];
        let conflicted = choose_briefing(
            &g,
            BranchState::Conflicted,
            "sandboard/card-7",
            &cross_fork_repo(),
            true,
        );
        assert!(conflicted.contains("CONFLICTS"), "{conflicted}");
        assert!(
            conflicted.to_lowercase().contains("resolve"),
            "{conflicted}"
        );
        assert!(
            !conflicted.contains("parked mid-run"),
            "must not use the short park resume prompt on CONFLICTS: {conflicted}"
        );

        let parked = choose_briefing(
            &g,
            BranchState::Rebased,
            "sandboard/card-7",
            &cross_fork_repo(),
            true,
        );
        assert!(parked.contains("parked mid-run"), "{parked}");
        assert!(!parked.contains("CONFLICTS"), "{parked}");
    }

    #[test]
    fn parse_pr_mergeable_reads_mark_and_tolerates_unknown() {
        assert_eq!(
            parse_pr_mergeable("SANDBOARD-PR-URL=https://x\nSANDBOARD-PR-MERGEABLE=CONFLICTING\n"),
            PrMergeable::Conflicting
        );
        assert_eq!(
            parse_pr_mergeable("SANDBOARD-PR-MERGEABLE=MERGEABLE\n"),
            PrMergeable::Mergeable
        );
        assert_eq!(
            parse_pr_mergeable("SANDBOARD-PR-MERGEABLE=UNKNOWN\n"),
            PrMergeable::Unknown
        );
        assert_eq!(
            parse_pr_mergeable("SANDBOARD-PR-URL=https://x\n"),
            PrMergeable::Unknown
        );
        assert_eq!(parse_pr_mergeable(""), PrMergeable::Unknown);
        assert_eq!(
            parse_pr_mergeable("SANDBOARD-PR-MERGEABLE=\n"),
            PrMergeable::Unknown
        );
    }

    #[test]
    fn pr_lookup_script_asks_for_mergeable() {
        let s = pr_lookup_script(&repo_cfg(), "sandboard/card-8");
        assert!(s.contains("mergeable"), "{s}");
        assert!(s.contains(PR_MERGEABLE_MARK), "{s}");
        assert!(
            s.contains("// empty"),
            "must yield nothing rather than error: {s}"
        );
    }

    /// Changes-requested notes are the whole steering mechanism: they reach the
    /// next run only by way of the briefing.
    #[test]
    fn steering_notes_reach_the_briefing() {
        let mut g = grant();
        g.notes = vec!["Changes requested: rebase onto latest, api.rs only.".into()];
        let b = briefing(&g, BranchState::Rebased, "sandboard/card-7", &cross_fork_repo());
        assert!(b.contains("rebase onto latest, api.rs only."), "{b}");
        assert!(
            b.contains("BINDING"),
            "notes must be framed as overrides of title/DoD: {b}"
        );
    }

    // ---- surviving a restart ------------------------------------------

    /// The agent must not be a child of the exec that starts it. As a child it
    /// died whenever sandboard did, which made every rebuild throw away a live run
    /// and left deleting the sandbox as the only honest option.
    #[test]
    fn the_agent_outlives_the_exec_that_starts_it() {
        let s = start_script(&repo_cfg(), "do the thing", "claude", None, None).unwrap();
        assert!(s.contains("setsid nohup"), "must be detached: {s}");
        assert!(
            s.trim_end().contains("&\n") || s.contains("2>&1 &"),
            "must background it: {s}"
        );
        // The three files are the whole contract with whoever watches next.
        assert!(
            s.contains(AGENT_LOG) && s.contains(AGENT_PID) && s.contains(AGENT_STATUS),
            "{s}"
        );
        // Starting must return once the run is up, not hold the exec open.
        assert!(
            s.contains("exit 0"),
            "must return as soon as the pid lands: {s}"
        );
    }

    /// The deadline has to live inside the sandbox. Once the agent is detached
    /// nothing on this side owns the process, and an agent nobody is watching
    /// still spends money.
    ///
    /// `--foreground` is not cosmetic: without it `timeout` moves the command
    /// into its own process group, and `stop_agent` then signals a group the
    /// agent is not in.
    #[test]
    fn the_agent_carries_its_own_deadline() {
        let mut cfg = repo_cfg();
        cfg.agent_timeout_secs = 900;
        let s = start_script(&cfg, "b", "claude", None, None).unwrap();
        assert!(s.contains("timeout --foreground 900 claude"), "{s}");
    }

    /// The briefing is quoted once, for the outer shell, and reaches the inner
    /// shell as an environment variable. Interpolating it into a second layer
    /// of single quotes breaks on the first card description with an
    /// apostrophe in it — which is most of them.
    #[test]
    fn the_briefing_crosses_the_inner_shell_intact() {
        let s = start_script(&repo_cfg(), "it's a card; rm -rf /", "claude", None, None).unwrap();
        assert!(
            s.contains(r"it'\''s a card; rm -rf /"),
            "must be escaped once: {s}"
        );
        assert!(
            s.contains(r#"$SANDBOARD_BRIEFING"#),
            "inner shell reads the var: {s}"
        );
    }

    #[test]
    fn start_script_materializes_hermes_query_file_before_detach() {
        let s = start_script(&repo_cfg(), "it's a card; $(not shell)", "hermes", None, None)
            .unwrap();
        assert!(
            s.contains("printf '%s' \"$SANDBOARD_BRIEFING\" > /tmp/sandboard-hermes-query"),
            "{s}"
        );
        assert!(
            s.contains("hermes --yolo --accept-hooks --provider openrouter chat --query-file /tmp/sandboard-hermes-query"),
            "{s}"
        );
        assert!(
            s.contains(&format!(
                "SANDBOARD_BRIEFING={}",
                shell_quote("it's a card; $(not shell)")
            )),
            "{s}"
        );
    }

    #[test]
    fn agy_resume_passes_conversation_flag() {
        let s = start_script(
            &repo_cfg(),
            "continue",
            "agy",
            Some("8f9c6cee-964a-44ce-8698-c92a4ea473ef"),
            None,
        )
        .unwrap();
        assert!(s.contains("--conversation \"$SANDBOARD_CONVERSATION\""), "{s}");
        assert!(
            s.contains("SANDBOARD_CONVERSATION='8f9c6cee-964a-44ce-8698-c92a4ea473ef'"),
            "{s}"
        );
        let fresh = start_script(&repo_cfg(), "start", "agy", None, None).unwrap();
        assert!(!fresh.contains("--conversation"), "{fresh}");
        assert!(!fresh.contains("SANDBOARD_CONVERSATION="), "{fresh}");
    }

    #[test]
    fn start_script_agy_uses_resolved_model() {
        let s = start_script(&repo_cfg(), "brief", "agy", None, Some("custom-seat")).unwrap();
        assert!(s.contains("--model 'custom-seat'"), "{s}");
        assert!(!s.contains(crate::antigravity::DEFAULT_SEAT_MODEL), "{s}");
    }

    #[test]
    fn start_script_cursor_uses_resolved_model() {
        let s = start_script(&repo_cfg(), "brief", "cursor", None, Some("gpt-5")).unwrap();
        assert!(s.contains("--model 'gpt-5'"), "{s}");
    }

    #[test]
    fn start_script_cursor_omits_model_when_unset() {
        let s = start_script(&repo_cfg(), "brief", "cursor", None, None).unwrap();
        assert!(!s.contains("--model"), "{s}");
    }

    #[test]
    fn parse_conversation_id_from_stream_shapes() {
        assert_eq!(
            parse_conversation_id(
                r#"{"event":"step_update","step_update":{"conversation_id":"abc-123","step_index":1}}"#
            )
            .as_deref(),
            Some("abc-123")
        );
        assert_eq!(
            parse_conversation_id(r#"{"conversation_id":"top-level"}"#).as_deref(),
            Some("top-level")
        );
        assert_eq!(
            parse_conversation_id(
                r#"{"type":"system","subtype":"init","session_id":"c6b62c6f-7ead-4fd6-9922-e952131177ff"}"#
            )
            .as_deref(),
            Some("c6b62c6f-7ead-4fd6-9922-e952131177ff")
        );
        assert_eq!(
            parse_conversation_id(
                r#"{"type":"step_start","sessionID":"ses_494719016ffe85dkDMj0FPRbHK","timestamp":1}"#
            )
            .as_deref(),
            Some("ses_494719016ffe85dkDMj0FPRbHK")
        );
        assert_eq!(parse_conversation_id(r#"{"type":"assistant"}"#), None);
        assert_eq!(parse_conversation_id("not json"), None);
    }

    #[test]
    fn cursor_engine_uses_agent_cli_flags() {
        let s = start_script(&repo_cfg(), "do the thing", "cursor", None, None).unwrap();
        assert!(s.contains("timeout --foreground"), "{s}");
        assert!(
            s.contains("agent -p --force --trust --approve-mcps --sandbox disabled"),
            "{s}"
        );
        assert!(s.contains("--output-format stream-json"), "{s}");
        assert!(!s.contains("--resume"), "{s}");
        let resume = start_script(
            &repo_cfg(),
            "continue",
            "cursor",
            Some("c6b62c6f-7ead-4fd6-9922-e952131177ff"),
            None,
        )
        .unwrap();
        assert!(
            resume.contains("--resume \"$SANDBOARD_CONVERSATION\""),
            "{resume}"
        );
        assert!(
            resume.contains("SANDBOARD_CONVERSATION='c6b62c6f-7ead-4fd6-9922-e952131177ff'"),
            "{resume}"
        );
    }

    #[test]
    fn start_script_rejects_unknown_engine() {
        let err = start_script(&repo_cfg(), "x", "nope", None, None).unwrap_err();
        assert!(
            err.to_string().contains("unknown agent engine"),
            "{err}"
        );
        // Must not silently emit a claude command.
        assert!(!err.to_string().contains("claude -p"), "{err}");
    }

    #[test]
    fn opencode_engine_uses_run_json_auto_and_session() {
        let s = start_script(&repo_cfg(), "do the thing", "opencode", None, None).unwrap();
        assert!(s.contains("timeout --foreground"), "{s}");
        assert!(
            s.contains("opencode run --format json --auto \"$SANDBOARD_BRIEFING\""),
            "{s}"
        );
        assert!(!s.contains("--session"), "{s}");
        let resume = start_script(
            &repo_cfg(),
            "continue",
            "opencode",
            Some("ses_494719016ffe85dkDMj0FPRbHK"),
            None,
        )
        .unwrap();
        assert!(
            resume.contains("--session \"$SANDBOARD_CONVERSATION\""),
            "{resume}"
        );
        assert!(
            resume.contains("SANDBOARD_CONVERSATION='ses_494719016ffe85dkDMj0FPRbHK'"),
            "{resume}"
        );
    }

    #[test]
    fn resume_briefing_is_short_and_carries_notes() {
        let mut g = grant();
        g.notes = vec!["Parked: cargo test deadlocked on Board RwLock.".into()];
        let b = resume_briefing(
            &g,
            &crate::schema::RepoConfig {
                upstream: "acme/widgets".into(),
                fork: "acme/widgets".into(),
                base: "main".into(),
            },
        );
        assert!(b.contains("parked mid-run"), "{b}");
        assert!(b.contains("Board RwLock"), "{b}");
        assert!(b.contains("BINDING"), "{b}");
        assert!(b.contains("origin/main"), "{b}");
        assert!(b.contains("acme/widgets"), "{b}");
        assert!(b.contains("report.schema.json"), "{b}");
        assert!(
            !b.contains("Standing constraints"),
            "must not dump the cold briefing: {b}"
        );
    }

    /// Card intent must ride in the grant and briefing; agents must not be told
    /// to run `bd show` / `bd prime` for card context (sandbox beads cut, t1).
    #[test]
    fn briefing_injects_intent_without_bd_workflow() {
        let g = grant();
        assert!(
            !g.intent.is_empty(),
            "ClaimGrant must carry card intent from WorkItem"
        );
        let cold = briefing(&g, BranchState::Fresh, "sandboard/card-7", &cross_fork_repo());
        assert!(
            cold.contains("Intent: why the card exists"),
            "cold briefing must emit card intent: {cold}"
        );
        assert!(
            !cold.contains("bd show") && !cold.contains("bd prime"),
            "cold briefing must not instruct bd: {cold}"
        );
        assert!(
            !cold.contains("Beads id:"),
            "cold briefing must not emit beads id line: {cold}"
        );
        assert!(
            !cold.contains("read snapshot"),
            "cold briefing must not describe beads read snapshot: {cold}"
        );

        let resume = resume_briefing(&g, &cross_fork_repo());
        assert!(
            resume.contains("Intent: why the card exists"),
            "resume briefing must emit card intent: {resume}"
        );
        assert!(
            !resume.contains("bd show") && !resume.contains("bd prime"),
            "resume briefing must not instruct bd: {resume}"
        );
        assert!(
            !resume.contains("Beads id:"),
            "resume briefing must not emit beads id line: {resume}"
        );
    }

    #[test]
    fn claim_populates_sandbox_prompt_from_resolved_profile() {
        use crate::model::{Origin, SandboxProfile};

        let board = Arc::new(crate::store::Board::new(
            crate::schema::Schema::default(),
            std::env::temp_dir().join(format!(
                "sandboard-test-claim-sbx-prompt-{}.json",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            )),
        ));
        board
            .upsert_openshell_policy(crate::model::OpenShellPolicy {
                id: "default-pol".into(),
                name: "Default policy".into(),
                yaml: crate::seed_policies::MINIMAL_SANDBOX_POLICY.into(),
            })
            .unwrap();
        board
            .upsert_sandbox_profile(SandboxProfile {
                id: "default".into(),
                name: "Default".into(),
                image: "default-image:1".into(),
                policy_id: "default-pol".into(),
                policy_inline_legacy: None,
                cpu: Some("2".into()),
                memory: Some("4Gi".into()),
                engine: None,
                model: None,
                provider_names: Vec::new(),
                mcp_server_ids: Vec::new(),
                env: Default::default(),
                prompt: Some("Profile seat notes.".into()),
                shipped: false,
            })
            .unwrap();
        board.set_default_sandbox_profile("default").unwrap();

        let project = board
            .create(None, "Sbx Proj", "why", None, Origin::Human, true, None)
            .unwrap();
        let task = board
            .create(
                Some(project.id),
                "task",
                "do it",
                Some("done".into()),
                Origin::Human,
                false,
                None,
            )
            .unwrap();
        board
            .set_task_repo(task.id, Some(cross_fork_repo()))
            .unwrap();
        let _ = board.transition(task.id, State::Shaping, "t", None);
        let _ = board.transition(task.id, State::Backlog, "t", None);
        let grant = board.claim(task.id, "agent", None, 60).unwrap();
        assert_eq!(
            grant.sandbox_prompt.as_deref(),
            Some("Profile seat notes.")
        );
    }

    #[test]
    fn sandbox_prompt_section_in_cold_briefing_after_project_prompt() {
        let mut g = grant();
        g.sandbox_prompt = Some("Use oc with API_URL.".into());
        let b = briefing(&g, BranchState::Fresh, "sandboard/card-7", &cross_fork_repo());
        assert!(b.contains("Sandbox prompt (seat notes):"), "{b}");
        assert!(b.contains("Use oc with API_URL."), "{b}");
        let proj_pos = b
            .find("Project prompt (Project standing extras):")
            .expect("project prompt");
        let sbx_pos = b.find("Sandbox prompt (seat notes):").expect("sandbox prompt");
        let card_pos = b.find("Your card:").expect("your card");
        assert!(
            proj_pos < sbx_pos,
            "sandbox prompt must follow project prompt: {b}"
        );
        assert!(
            sbx_pos < card_pos,
            "sandbox prompt must precede card header: {b}"
        );
    }

    #[test]
    fn resume_briefing_omits_sandbox_prompt() {
        let mut g = grant();
        g.sandbox_prompt = Some("Use oc with API_URL.".into());
        let b = resume_briefing(&g, &cross_fork_repo());
        assert!(
            !b.contains("Sandbox prompt (seat notes):"),
            "resume must not re-dump sandbox prompt: {b}"
        );
        assert!(
            !b.contains("Use oc with API_URL."),
            "resume must not repeat seat notes: {b}"
        );
    }

    #[test]
    fn cockpit_briefing_includes_sandbox_prompt_section() {
        let cold = cockpit_briefing(Some("Cockpit seat notes for oc."));
        assert!(cold.contains("Sandbox prompt (seat notes):"), "{cold}");
        assert!(cold.contains("Cockpit seat notes for oc."), "{cold}");
        assert!(
            !cockpit_briefing(None).contains("Sandbox prompt (seat notes):"),
            "empty prompt must omit section"
        );
        assert!(
            !cockpit_briefing(Some("  ")).contains("Sandbox prompt (seat notes):"),
            "blank prompt must omit section"
        );
    }

    /// Sandbox image/policy/runtime must not reintroduce beads (t3 regression).
    #[test]
    fn sandbox_assets_have_no_beads_surface() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let containerfile =
            std::fs::read_to_string(root.join("sandbox/Containerfile")).expect("Containerfile");
        let policy = crate::seed_policies::MINIMAL_SANDBOX_POLICY;
        let supervisor_src =
            std::fs::read_to_string(root.join("src/supervisor.rs")).expect("supervisor.rs");
        // Only the production module — this test's own source mentions the
        // forbidden identifiers as negative assertions.
        let prod = supervisor_src
            .split("#[cfg(test)]")
            .next()
            .expect("supervisor has a test module");

        assert!(
            !containerfile.contains("/usr/local/bin/bd")
                && !containerfile.to_lowercase().contains("beads"),
            "Containerfile must not bake bd/beads"
        );
        assert!(
            !policy.contains("/usr/local/bin/bd") && !policy.to_lowercase().contains("beads"),
            "minimal sandbox policy must not allowlist bd/beads"
        );
        assert!(
            !prod.contains("sync_beads_into_sandbox") && !prod.contains("BEADS_SANDBOX_DIR"),
            "supervisor must not upload beads DB or set BEADS_SANDBOX_DIR"
        );

        let env = agent_env("claude");
        assert!(
            env.iter().all(|(k, _)| k != "BEADS_DIR"),
            "agent_env must not export BEADS_DIR: {env:?}"
        );
        assert!(
            env.iter()
                .any(|(k, v)| k == "CARGO_TARGET_DIR" && v == "/opt/cargo-target"),
            "agent_env must point at the image precompile dir: {env:?}"
        );
        assert!(
            env.iter().any(|(k, v)| k == "HOME" && v == "/sandbox"),
            "agent_env must force HOME=/sandbox for Cursor MCP discovery: {env:?}"
        );
        let path = env
            .iter()
            .find(|(k, _)| k == "PATH")
            .map(|(_, v)| v.as_str())
            .unwrap_or("");
        assert!(
            !path.contains("/sandbox/.venv"),
            "agent_env PATH must not reference removed Ubuntu venv: {path}"
        );
        let script = start_script(&repo_cfg(), "briefing", "claude", None, None).unwrap();
        assert!(
            !script.contains("BEADS_DIR"),
            "start script must not export BEADS_DIR: {script}"
        );
    }

    #[test]
    fn anthropic_engines_point_at_inference_local() {
        let claude_env = agent_env("claude");
        assert!(
            claude_env.iter().any(|(k, v)| {
                k == "ANTHROPIC_BASE_URL" && v == "https://inference.local"
            }),
            "{claude_env:?}"
        );
        let oc_env = agent_env("opencode");
        assert!(
            oc_env.iter().any(|(k, v)| {
                k == "ANTHROPIC_BASE_URL" && v == "https://inference.local/v1"
            }),
            "{oc_env:?}"
        );
        assert!(
            agent_env("cursor")
                .iter()
                .all(|(k, _)| k != "ANTHROPIC_BASE_URL"),
            "cursor must not force Anthropic base URL"
        );
        let script = start_script(&repo_cfg(), "briefing", "opencode", None, None).unwrap();
        assert!(
            script.contains("ANTHROPIC_BASE_URL=https://inference.local/v1"),
            "{script}"
        );
        assert!(script.contains("unset CLAUDE_CODE_USE_VERTEX"), "{script}");
    }

    #[test]
    fn hermes_session_footer_is_a_resume_handle() {
        assert_eq!(
            parse_conversation_id("\nsession_id: 20260828_120000_a1b2c3\n"),
            Some("20260828_120000_a1b2c3".into())
        );
        assert_eq!(parse_conversation_id("session_id:   "), None);
    }

    /// Following is a *reader*. It can start part-way through, which is what
    /// lets a restarted sandboard take over a run instead of killing it.
    #[test]
    fn following_can_start_part_way_through() {
        let s = follow_script(118);
        assert!(s.contains("tail -n +118"), "{s}");
        assert!(s.contains("--pid="), "must stop when the agent does: {s}");
        assert!(
            s.contains(AGENT_STATUS),
            "must exit with the agent's own code: {s}"
        );
        assert!(
            !s.contains("claude"),
            "following must not start anything: {s}"
        );
    }

    /// A run can finish while sandboard is down. Waiting on a pid that is already
    /// gone would hang, so the finished case is handled before the wait.
    #[test]
    fn a_finished_run_is_not_waited_on() {
        let s = follow_script(1);
        let wait = s.find("--pid=").expect("waits somewhere");
        let done = s
            .find(&format!("if [ -f {AGENT_STATUS} ]"))
            .expect("checks for the status");
        assert!(
            done < wait,
            "the already-finished case must come first: {s}"
        );
    }

    /// The card decides what happens to a sandbox, not the sandbox.
    #[test]
    fn only_the_cards_own_live_sandbox_is_adopted() {
        let mut item = WorkItem::new(9, "t", "i");
        item.state = State::Running;
        item.environment = Some("sandboard-card-9-a2".into());
        assert!(adoptable(Some(&item), "sandboard-card-9-a2").is_some());

        // The previous attempt's sandbox is kept for inspection and carries the
        // same `sandboard.item` label. Adopting it would attach to a dead log while
        // the real run went unwatched.
        assert!(
            adoptable(Some(&item), "sandboard-card-9-a1").is_none(),
            "reap the old attempt"
        );

        // Not running: cannot adopt, but sandbox is kept by reconcile for review/reclaim
        item.state = State::Review;
        assert!(adoptable(Some(&item), "sandboard-card-9-a2").is_none());

        // A sandbox for a card that no longer exists.
        assert!(adoptable(None, "sandboard-card-9-a2").is_none());
    }

    #[test]
    fn reconcile_keeps_sandboxes_for_cards_short_of_done() {
        let mut item = WorkItem::new(9, "t", "i");
        item.state = State::Review;
        item.environment = Some("sandboard-card-9-a2".into());

        assert!(should_keep_sandbox(Some(&item), "sandboard-card-9-a2"));

        // Backlog with environment set (e.g. Request changes)
        item.state = State::Backlog;
        assert!(should_keep_sandbox(Some(&item), "sandboard-card-9-a2"));

        // Prior attempt for the same card is kept (prefix match) so create
        // cannot race reconcile; run_card deletes the previous name explicitly.
        assert!(should_keep_sandbox(Some(&item), "sandboard-card-9-a1"));

        // Halt clears environment — sweeper must not preserve the box.
        item.environment = None;
        assert!(!should_keep_sandbox(Some(&item), "sandboard-card-9-a2"));
        assert!(!should_keep_sandbox(Some(&item), "sandboard-card-9-a1"));
        item.environment = Some("sandboard-card-9-a2".into());

        item.state = State::NeedsHuman;
        assert!(should_keep_sandbox(Some(&item), "sandboard-card-9-a2"));
        assert!(should_keep_sandbox(Some(&item), "sandboard-card-9-a3"));

        // Terminal card sandbox is not kept (reaped)
        item.state = State::Done;
        assert!(!should_keep_sandbox(Some(&item), "sandboard-card-9-a2"));

        item.state = State::Retired;
        assert!(!should_keep_sandbox(Some(&item), "sandboard-card-9-a2"));

        // Deleted item sandbox is not kept
        assert!(!should_keep_sandbox(None, "sandboard-card-9-a2"));

        // Other cards' sandboxes are not kept
        item.state = State::Backlog;
        assert!(!should_keep_sandbox(Some(&item), "sandboard-card-8-a1"));

        // Fixed stem is sandboard — foreign prefixes are not a keep match for this card.
        item.environment = Some("sandboard-card-9-a2".into());
        assert!(should_keep_sandbox(Some(&item), "sandboard-card-9-a1"));
        assert!(!should_keep_sandbox(Some(&item), "widgets-card-9-a1"));
    }

    #[test]
    fn should_keep_cockpit_sandbox_follows_board_session() {
        let mut session = CockpitSession::new(Some("sandboard-cockpit".into()), Some("conv-1".into()));
        assert!(should_keep_cockpit_sandbox(Some(&session), "sandboard-cockpit"));
        // Stable singleton name kept even before Board records environment.
        let bare = CockpitSession::new(None, None);
        assert!(should_keep_cockpit_sandbox(Some(&bare), "sandboard-cockpit"));
        assert!(!should_keep_cockpit_sandbox(Some(&bare), "sandboard-card-1-a1"));

        session.status = CockpitSessionStatus::Parked;
        assert!(
            should_keep_cockpit_sandbox(Some(&session), "sandboard-cockpit"),
            "park keeps sandbox"
        );

        assert!(
            !should_keep_cockpit_sandbox(None, "sandboard-cockpit"),
            "stop/absent session reaps"
        );
    }

    #[test]
    fn cockpit_session_wants_sandbox_true_after_stop_start_race() {
        // Mirrors finalize_cockpit: Stop cleared session, Start created again
        // before the slow gateway delete ran — must not reap under the new session.
        let b = test_board();
        assert!(
            !cockpit_session_wants_sandbox(&b, "sandboard-cockpit"),
            "absent session does not want sandbox"
        );
        b.create_cockpit_session(None, None).expect("start");
        assert!(
            cockpit_session_wants_sandbox(&b, "sandboard-cockpit"),
            "fresh Start keeps singleton name even before environment is set"
        );
        b.stop_cockpit_session().expect("stop");
        assert!(!cockpit_session_wants_sandbox(&b, "sandboard-cockpit"));
        b.create_cockpit_session(Some("sandboard-cockpit".into()), None)
            .expect("start again");
        assert!(cockpit_session_wants_sandbox(&b, "sandboard-cockpit"));
    }

    #[test]
    fn cockpit_seat_rejects_a_session_replaced_during_a_fast_restart() {
        let b = test_board();
        let first = b.create_cockpit_session(None, None).expect("start");
        b.stop_cockpit_session().expect("stop");
        b.create_cockpit_session(None, None).expect("start again");

        let err = ensure_cockpit_session_running(&b, Some(first.created_at))
            .expect_err("the old seat must cancel after Start replaces its session");
        assert_eq!(err.to_string(), COCKPIT_CANCEL_SUPERSEDED);
    }

    #[test]
    fn an_error_phase_sandbox_still_occupies_the_cockpit_name() {
        let sandbox = crate::openshell::Sandbox {
            name: "sandboard-cockpit".into(),
            id: Some("sandbox-id".into()),
            phase: Some("Error".into()),
            labels: Default::default(),
        };

        assert!(cockpit_sandbox_name_is_present(
            std::slice::from_ref(&sandbox),
            "sandboard-cockpit"
        ));
        assert!(!cockpit_sandbox_name_is_present(
            std::slice::from_ref(&sandbox),
            "other-sandbox"
        ));
    }

    #[test]
    fn an_unusable_cockpit_failure_is_recoverable_not_a_session_cancel() {
        assert!(is_cockpit_unusable(COCKPIT_CANCEL_UNUSABLE));
        assert!(!is_cockpit_superseded(COCKPIT_CANCEL_UNUSABLE));
    }

    #[test]
    fn cockpit_sandbox_spec_uses_cockpit_label_not_card_item() {
        let resolved = crate::model::ResolvedSandboxCreate {
            image: "sandboard-sandbox:latest".into(),
            policy: "version: 1\n".into(),
            cpu: Some("1".into()),
            memory: Some("2Gi".into()),
            engine: Some("agy".into()),
            model: None,
            profile_id: Some("cockpit".into()),
            providers: Vec::new(),
            mcp_server_ids: Vec::new(),
            env: Default::default(),
            prompt: None,
        };
        let spec = sandbox_spec_for_cockpit("sandboard-cockpit", &resolved, &[], "agy");
        assert_eq!(spec.name, "sandboard-cockpit");
        assert_eq!(spec.cpu.as_deref(), Some("1"));
        assert_eq!(spec.memory.as_deref(), Some("2Gi"));
        assert!(
            spec.labels
                .iter()
                .any(|(k, v)| k == LABEL_COCKPIT && v == "1"),
            "cockpit label required: {:?}",
            spec.labels
        );
        assert!(
            !spec.labels.iter().any(|(k, _)| k == LABEL_ITEM),
            "must not use card sandboard.item label: {:?}",
            spec.labels
        );
        // Host MCP is stdio over a local Unix socket now (mcp.json bakes in
        // `socat - UNIX-CONNECT:<AGENT_SOCK_PATH>`) — no env var to point at it.
        assert!(
            !spec.env.iter().any(|(k, _)| k == "SANDBOARD_MCP_URL"),
            "SANDBOARD_MCP_URL is stale; cockpit MCP is stdio now: {:?}",
            spec.env
        );
        assert!(spec.providers.is_empty(), "test passes empty providers");
    }

    #[test]
    fn sandbox_create_env_starts_with_agent_env_then_overlays_profile() {
        let mut profile = BTreeMap::new();
        profile.insert("CUSTOM_VAR".into(), "from-profile".into());
        let env = sandbox_create_env("claude", &profile);
        let agent = agent_env("claude");
        for (k, v) in &agent {
            let got = env.iter().find(|(key, _)| key == k).map(|(_, v)| v.as_str());
            assert_eq!(got, Some(v.as_str()), "agent_env key {k} missing or wrong");
        }
        assert_eq!(
            env.iter()
                .find(|(k, _)| k == "CUSTOM_VAR")
                .map(|(_, v)| v.as_str()),
            Some("from-profile")
        );
    }

    #[test]
    fn sandbox_create_env_profile_wins_on_key_clash() {
        let mut profile = BTreeMap::new();
        profile.insert("HOME".into(), "/custom-home".into());
        profile.insert("EXTRA".into(), "added".into());
        let env = sandbox_create_env("claude", &profile);
        assert_eq!(
            env.iter().find(|(k, _)| k == "HOME").map(|(_, v)| v.as_str()),
            Some("/custom-home"),
            "profile must override agent_env on clash: {env:?}"
        );
        assert_eq!(
            env.iter().find(|(k, _)| k == "EXTRA").map(|(_, v)| v.as_str()),
            Some("added")
        );
        assert!(
            env.iter().any(|(k, v)| k == "DISABLE_TELEMETRY" && v == "1"),
            "non-clashing agent_env keys must remain: {env:?}"
        );
    }

    #[test]
    fn card_sandbox_spec_passes_merged_env_to_openshell() {
        let mut profile_env = BTreeMap::new();
        profile_env.insert("HOME".into(), "/profile-home".into());
        profile_env.insert("API_URL".into(), "https://api.example".into());
        let resolved = crate::model::ResolvedSandboxCreate {
            image: "img:1".into(),
            policy: "version: 1\n".into(),
            cpu: None,
            memory: None,
            engine: Some("claude".into()),
            model: None,
            profile_id: Some("test".into()),
            providers: Vec::new(),
            mcp_server_ids: Vec::new(),
            env: profile_env,
            prompt: None,
        };
        let spec = sandbox_spec_for_card(42, "sandboard-card-42", &resolved, &[]);
        assert_eq!(
            spec.env.iter().find(|(k, _)| k == "HOME").map(|(_, v)| v.as_str()),
            Some("/profile-home")
        );
        assert_eq!(
            spec.env
                .iter()
                .find(|(k, _)| k == "API_URL")
                .map(|(_, v)| v.as_str()),
            Some("https://api.example")
        );
        assert!(
            spec.env.iter().any(|(k, v)| k == "CARGO_HOME" && v == "/opt/cargo"),
            "agent_env keys must be present: {:?}",
            spec.env
        );
    }

    #[test]
    fn cockpit_sandbox_spec_passes_merged_env_to_openshell() {
        let mut profile_env = BTreeMap::new();
        profile_env.insert("PATH".into(), "/custom/bin".into());
        let resolved = crate::model::ResolvedSandboxCreate {
            image: "sandboard-sandbox:latest".into(),
            policy: "version: 1\n".into(),
            cpu: None,
            memory: None,
            engine: Some("agy".into()),
            model: None,
            profile_id: Some("cockpit".into()),
            providers: Vec::new(),
            mcp_server_ids: Vec::new(),
            env: profile_env,
            prompt: None,
        };
        let spec = sandbox_spec_for_cockpit("sandboard-cockpit", &resolved, &[], "agy");
        assert_eq!(
            spec.env.iter().find(|(k, _)| k == "PATH").map(|(_, v)| v.as_str()),
            Some("/custom/bin")
        );
        assert!(
            spec.env
                .iter()
                .any(|(k, v)| k == "DISABLE_TELEMETRY" && v == "1"),
            "agent_env keys must remain when not overridden: {:?}",
            spec.env
        );
    }

    #[test]
    fn cockpit_briefing_is_operator_seat_not_worker() {
        let cold = cockpit_briefing(None);
        assert!(cold.contains("cockpit"), "{cold}");
        assert!(cold.contains("operator"), "{cold}");
        assert!(cold.contains("board_snapshot"), "{cold}");
        assert!(
            cold.contains("clone_repo") && cold.contains("owner/name"),
            "briefing must require create_project clone_repo: {cold}"
        );
        assert!(
            cold.contains("project_prompt"),
            "briefing must explain project_prompt: {cold}"
        );
        assert!(
            cold.contains("Configuration stacks in layers"),
            "briefing must name configuration layers: {cold}"
        );
        assert!(
            cold.contains("do not assume cargo") || cold.contains("does not assume cargo"),
            "briefing must not invent cargo gates: {cold}"
        );
        assert!(
            cold.contains("mcp.json"),
            "briefing must point at injected MCP config: {cold}"
        );
        assert!(
            cold.contains("do **not** run") || cold.contains("browser OAuth"),
            "briefing must forbid browser OAuth in the seat: {cold}"
        );
        for verb in [
            "claim",
            "heartbeat",
            "report",
            "report_pull_request",
            "split",
            "escalate",
            "release",
            "list_ready",
        ] {
            assert!(
                cold.contains(verb),
                "briefing must name denied worker verb {verb}"
            );
        }
        assert!(
            !cold.contains("/sandbox/.sandboard/report.json"),
            "cockpit must not use card report path"
        );
        assert!(
            !cold.contains("Do exactly this card"),
            "cockpit is not a card worker briefing"
        );

        let resume = cockpit_resume_briefing();
        assert!(resume.contains("parked"), "{resume}");
        assert!(resume.contains("cockpit"), "{resume}");
        assert!(resume.contains("worker verbs"), "{resume}");
        assert!(
            resume.contains("mcp.json"),
            "resume briefing must mention MCP inject path: {resume}"
        );
    }

    #[test]
    fn cockpit_seat_helpers_do_not_encode_worker_verbs() {
        // Production cockpit path must not call Board worker verbs — grep the
        // cockpit section for claim/heartbeat/report as call sites.
        let src = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/supervisor.rs"),
        )
        .expect("supervisor.rs");
        let prod = src
            .split("#[cfg(test)]")
            .next()
            .expect("supervisor has a test module");
        let cockpit_pol = prod
            .split("// ----------------------------------------------------- durable cockpit")
            .nth(1)
            .and_then(|rest| {
                rest.split(
                    "// ----------------------------------------------------------------- helpers",
                )
                .next()
            })
            .expect("cockpit section");
        for needle in [
            "board.claim(",
            "board.heartbeat(",
            "board.report(",
            "board.split(",
            "list_awaiting_dispatch",
            ".list_ready(",
        ] {
            assert!(
                !cockpit_pol.contains(needle),
                "cockpit must not call worker path {needle}"
            );
        }
        assert!(
            cockpit_pol.contains("ready for Cockpit attach"),
            "cockpit must hand the agent to Cockpit attach"
        );
        assert!(
            cockpit_pol.contains("LABEL_COCKPIT"),
            "cockpit must label sandboxes"
        );
    }

    #[test]
    fn relay_and_not_ready_are_infrastructure() {
        assert!(is_infrastructure(
            "clone failed: Error: exec relay closed before the command reported an exit status"
        ));
        assert!(is_infrastructure("sandbox is not ready"));
        assert!(is_infrastructure(
            "code: 'The service is currently unavailable', message: \"exec relay closed\""
        ));
        assert!(is_infrastructure(
            "GitHub App token sync failed (infrastructure): github api: boom"
        ));
        assert!(!is_infrastructure("agent panicked in user code"));
    }

    /// Without these matches, a flaky OpenShell h2/hyper drop burns the card's
    /// 3-strike run budget even though the agent never ran.
    #[test]
    fn h2_broken_pipe_does_not_burn_card_run_budget() {
        assert!(is_infrastructure(
            "openshell sandbox exec: status: Unknown, message: \"h2 protocol error: error reading a body from connection\", details: [], metadata: MetadataMap { headers: {} }"
        ));
        assert!(is_infrastructure(
            "openshell sandbox exec: error reading a body from connection: Broken pipe (os error 32)"
        ));
        assert!(is_infrastructure(
            "openshell sandbox exec: connection closed before message completed"
        ));
        assert!(!is_infrastructure("agent exited 1: cargo test failed"));
    }

    #[test]
    fn refresh_script_fetches_and_rebases_in_place() {
        let cfg = repo_cfg();
        let s = refresh_script(&cfg, "sandboard/card-8");
        assert!(s.contains("cd /sandbox/repo"), "{s}");
        assert!(
            !s.contains("git reset --hard"),
            "must not discard mid-run tracked edits: {s}"
        );
        assert!(
            !s.contains("git clean -fd"),
            "must not discard mid-run untracked edits: {s}"
        );
        assert!(
            s.contains("git status --porcelain"),
            "dirty tree must skip supervisor rebase: {s}"
        );
        assert!(
            s.contains("git rev-parse --verify sandboard/card-8"),
            "must prefer local card branch before origin: {s}"
        );
        assert!(s.contains("fetch -q upstream main"), "{s}");
        assert!(s.contains("fetch -q origin sandboard/card-8"), "{s}");
        assert!(s.contains("rebase -q upstream/main"), "{s}");
        assert!(!s.contains("rm -rf"), "must not wipe workdir: {s}");
    }

    /// Cold start (`!is_reused`) empties `/sandbox/repo` before the agent
    /// clones. Park resume and Needs You answer reclaim share the `is_reused`
    /// path and must never call this wipe script.
    #[test]
    fn cold_start_empty_workdir_wipes_repo() {
        let s = empty_workdir_script();
        assert!(
            s.contains("rm -rf /sandbox/repo"),
            "cold start must clear workdir: {s}"
        );
        assert!(s.contains("mkdir -p /sandbox/repo"), "{s}");
        assert!(s.contains(MARK_FRESH), "{s}");
        assert!(
            !s.contains("git clone"),
            "supervisor must not clone: {s}"
        );
    }

    /// Reuse without a checkout (same `is_reused` path as park resume and
    /// Needs You reclaim): ensure the directory exists; do not wipe contents
    /// or caches. Agent still owns clone.
    #[test]
    fn reuse_ensure_workdir_does_not_wipe() {
        let s = ensure_workdir_script();
        assert!(
            !s.contains("rm -rf"),
            "reuse must not wipe /sandbox/repo: {s}"
        );
        assert!(s.contains("mkdir -p /sandbox/repo"), "{s}");
        assert!(s.contains(MARK_FRESH), "{s}");
        assert!(
            !s.contains("git clone"),
            "supervisor must not clone: {s}"
        );
    }

    /// Where to resume after sandboard restarts mid-run.
    #[test]
    fn a_probe_says_where_to_resume() {
        let out = format!("{MARK_ALIVE}\n{MARK_LINES}117\n");
        assert_eq!(probe_of(&out), Some(118));

        // A run that finished while sandboard was down still has a PR to record.
        let done = format!("{MARK_EXITED}\n{MARK_LINES}4\n");
        assert_eq!(probe_of(&done), Some(5));

        // Nothing running means there is nothing to adopt — the card goes back
        // in the queue rather than being watched forever.
        let gone = format!("{MARK_GONE}\n{MARK_LINES}0\n");
        assert_eq!(probe_of(&gone), None);
    }

    /// The other way to lose a live run to a restart.
    ///
    /// Reconciliation used to no-op when the gateway could not answer, and the
    /// sweeper started regardless — so a podman machine that was merely slow to
    /// come up got a still-running card requeued and a second agent dispatched
    /// onto its branch. `false` stands in for a gateway that is not there.
    #[tokio::test]
    async fn startup_waits_for_a_gateway_that_is_not_up_yet() {
        let board = test_board();
        // Short grace: assert we wait, not that we wait a human-scale outage.
        let grace = Duration::from_millis(50);

        let began = std::time::Instant::now();
        let adopted = reconcile_once_reachable(&board, 600, grace).await;

        assert!(
            adopted.is_empty(),
            "nothing can be adopted through a dead gateway"
        );
        assert!(
            began.elapsed() >= grace,
            "must wait for the gateway, not skip past it"
        );
    }

    /// The wait is bounded on purpose. A gateway that is never coming back must
    /// not hold the sweeper — and therefore every Running card — forever.
    #[tokio::test]
    async fn a_gateway_that_never_answers_does_not_freeze_the_board() {
        let began = std::time::Instant::now();
        reconcile_once_reachable(&test_board(), 600, Duration::from_millis(50)).await;
        assert!(
            began.elapsed() < Duration::from_secs(30),
            "gave up in bounded time"
        );
    }

    /// Never flushed, so the path is only ever a name.
    fn test_board() -> SharedBoard {
        Arc::new(crate::store::Board::new(
            crate::schema::Schema::default(),
            std::env::temp_dir().join("sandboard-test-reconcile.json"),
        ))
    }

    /// Cross-fork PRs need `owner:branch` as the head, or gh silently looks for
    /// the branch on upstream and fails.
    fn cross_fork_repo() -> crate::schema::RepoConfig {
        crate::schema::RepoConfig {
            upstream: "sandboard-app/sandboard".into(),
            fork: "clankrshq/sandboard".into(),
            base: "main".into(),
        }
    }

    fn repo_cfg() -> AgentConfig {
        let mut cfg = AgentConfig::default();
        cfg.repo.upstream = "sandboard-app/sandboard".into();
        cfg.repo.fork = "clankrshq/sandboard".into();
        cfg.repo.base = "main".into();
        cfg
    }

    /// In-process openshell for `process_verdict` tests — no bash spawn.
    fn verdict_openshell(
        kind: &str,
        payload_path: impl AsRef<std::path::Path>,
        pr_lookup_url: Option<&str>,
        deny_download_substr: Option<&str>,
    ) -> OpenShell {
        verdict_openshell_mergeable(
            kind,
            payload_path,
            pr_lookup_url,
            None,
            deny_download_substr,
        )
    }

    fn verdict_openshell_mergeable(
        kind: &str,
        payload_path: impl AsRef<std::path::Path>,
        pr_lookup_url: Option<&str>,
        mergeable: Option<&str>,
        deny_download_substr: Option<&str>,
    ) -> OpenShell {
        let kind = kind.to_string();
        let payload = payload_path.as_ref().to_path_buf();
        let pr_url = pr_lookup_url.map(|s| s.to_string());
        let mergeable = mergeable.map(|s| s.to_string());
        let deny = deny_download_substr.map(|s| s.to_string());
        OpenShell::mock(
            move |args| {
                let ok = |stdout: String| crate::openshell::Output {
                    code: 0,
                    stdout,
                    stderr: String::new(),
                };
                let fail = || crate::openshell::Output {
                    code: 1,
                    stdout: String::new(),
                    stderr: "mock fail".into(),
                };
                match (
                    args.first().map(String::as_str),
                    args.get(1).map(String::as_str),
                ) {
                    (Some("sandbox"), Some("exec")) => {
                        let script = args.last().map(String::as_str).unwrap_or("");
                        if script.contains("gh pr list") {
                            return match &pr_url {
                                Some(url) => {
                                    let mut out = format!("{PR_URL_MARK}{url}\n");
                                    if let Some(m) = &mergeable {
                                        out.push_str(&format!("{PR_MERGEABLE_MARK}{m}\n"));
                                    }
                                    ok(out)
                                }
                                None => ok(String::new()),
                            };
                        }
                        ok(format!("{}:{}\n", kind, payload.display()))
                    }
                    (Some("sandbox"), Some("download")) => {
                        let remote = args.get(3).map(String::as_str).unwrap_or("");
                        let dest = args.get(4).map(String::as_str).unwrap_or("");
                        if deny.as_ref().is_some_and(|d| remote.contains(d.as_str())) {
                            return fail();
                        }
                        let src = std::path::PathBuf::from(remote);
                        if std::fs::copy(&src, dest).is_ok() {
                            ok(String::new())
                        } else {
                            fail()
                        }
                    }
                    _ => fail(),
                }
            },
            Duration::from_secs(5),
        )
    }

    fn grant() -> ClaimGrant {
        let deadline = chrono::Utc::now() + chrono::Duration::seconds(1800);
        ClaimGrant {
            item_id: 7,
            title: "t".into(),
            intent: "why the card exists".into(),
            definition_of_done: None,
            project_title: Some("Test Project".into()),
            board_prompt: Some(
                "Do not assume cargo. Name quality gates in standing prompts.".into(),
            ),
            project_prompt: Some("Follow the Plan.".into()),
            plan_summary: Some("Do the thing.".into()),
            plan_tasks: vec![crate::model::PlanTaskBrief {
                key: "t1".into(),
                title: "t".into(),
                intent: "why".into(),
                definition_of_done: "done".into(),
                blocked_by_keys: vec![],
                current: true,
            }],
            plan_task_key: Some("t1".into()),
            notes: vec![],
            lease_expires_at: deadline,
            run_deadline_at: deadline,
            engine: None,
            model: None,
            sandbox_prompt: None,
        }
    }

    #[test]
    fn probe_verdict_script_checks_locations() {
        let s = probe_verdict_script();
        assert!(
            !s.contains("{WORKDIR}/.sandboard"),
            "probe script must not search {WORKDIR}/.sandboard: {s}"
        );
        assert!(
            !s.contains("WORKDIR"),
            "probe script must not reference WORKDIR: {s}"
        );
        assert!(s.contains("escalate.json"), "{s}");
        assert!(s.contains(".sandboard"), "{s}");
        assert!(s.contains("/work/.sandboard"), "{s}");
        assert!(s.contains("/sandbox/.sandboard"), "{s}");
    }

    #[test]
    fn escalate_file_deserializes_structs_and_strings() {
        let json1 = r#"{
            "question": "Which database?",
            "options": [
                {"label": "Postgres", "detail": "Relational"},
                {"label": "SQLite", "detail": "Embedded"}
            ],
            "recommended": 0
        }"#;
        let esc1: EscalateFile = serde_json::from_str(json1).unwrap();
        assert_eq!(esc1.question, "Which database?");
        assert_eq!(esc1.options.len(), 2);
        assert_eq!(esc1.recommended, 0);

        let json2 = r#"{
            "question": "Which database?",
            "options": ["Postgres", "SQLite"]
        }"#;
        let esc2: EscalateFile = serde_json::from_str(json2).unwrap();
        let opts: Vec<_> = esc2
            .options
            .into_iter()
            .map(|o| o.into_escalation_option())
            .collect();
        assert_eq!(opts[0].label, "Postgres");
        assert_eq!(opts[0].detail, "Postgres");
        assert_eq!(opts[1].label, "SQLite");

        // Cursor agents often emit title/body instead of label/detail (#197).
        let json3 = r#"{
            "question": "How finish?",
            "options": [
                {"title": "Accept smoke", "body": "No PR"},
                {"title": "Re-run", "description": "With bot token"}
            ],
            "recommended": 0,
            "evidence": {"auth_login": "shanemcd"}
        }"#;
        let esc3: EscalateFile = serde_json::from_str(json3).unwrap();
        let opts: Vec<_> = esc3
            .options
            .into_iter()
            .map(|o| o.into_escalation_option())
            .collect();
        assert_eq!(opts[0].label, "Accept smoke");
        assert_eq!(opts[0].detail, "No PR");
        assert_eq!(opts[1].label, "Re-run");
        assert_eq!(opts[1].detail, "With bot token");
    }

    #[test]
    fn committed_split_json_inside_workdir_cannot_trigger_split() {
        let script = probe_verdict_script();
        assert!(
            !script.contains("{WORKDIR}"),
            "probe_verdict_script must not reference WORKDIR: {script}"
        );

        let temp_dir = std::env::temp_dir().join(format!(
            "sandboard-test-workdir-{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        ));
        let sandboard_dir = temp_dir.join(".sandboard");
        std::fs::create_dir_all(&sandboard_dir).expect("create .sandboard in temp workdir");
        let split_file = sandboard_dir.join("split.json");
        std::fs::write(
            &split_file,
            r#"{"children":[{"title":"Fake Child","intent":"Fake Intent"}]}"#,
        )
        .expect("write fake split.json in workdir");

        let output = std::process::Command::new("bash")
            .arg("-c")
            .arg(&script)
            .current_dir(&temp_dir)
            .output()
            .expect("execute probe_verdict_script in bash");

        let stdout = String::from_utf8_lossy(&output.stdout);
        let _ = std::fs::remove_dir_all(&temp_dir);

        assert!(
            !stdout.contains("split.json") || !stdout.contains(temp_dir.to_string_lossy().as_ref()),
            "a split.json inside WORKDIR/.sandboard must NOT be detected by probe_verdict_script: stdout={stdout}"
        );
    }

    #[test]
    fn briefing_mentions_verdict_escalate_protocol() {
        let b = briefing(
            &grant(),
            BranchState::Fresh,
            "sandboard/card-12",
            &cross_fork_repo(),
        );
        assert!(
            b.contains("/sandbox/.sandboard/escalate.json"),
            "briefing must mention /sandbox/.sandboard/escalate.json: {b}"
        );
        assert!(
            !b.contains("`.sandboard/escalate.json`"),
            "briefing must omit WORKDIR .sandboard/escalate.json: {b}"
        );
    }

    #[test]
    fn briefing_forbids_hacking_around_network_policy() {
        let b = briefing(
            &grant(),
            BranchState::Fresh,
            "sandboard/card-12",
            &cross_fork_repo(),
        );
        assert!(
            b.contains("do **not** hack around") || b.contains("do not hack around"),
            "briefing must forbid network workarounds: {b}"
        );
        assert!(
            b.contains("sandbox network policy") || b.contains("network policy"),
            "briefing must defer policy changes to humans: {b}"
        );
    }

    /// Unbound first runs used to say "clone per the Project prompt" with no
    /// guard — agents invented sandboard-app/sandboard from ambient context. Needs You.
    #[test]
    fn unbound_briefing_forbids_guessing_the_repo() {
        let unbound = crate::schema::RepoConfig::default();
        assert!(!unbound.is_complete());
        let b = briefing(&grant(), BranchState::Fresh, "sandboard/card-172", &unbound);
        assert!(
            b.contains("Do **not** guess") || b.contains("do not guess"),
            "must forbid guessing: {b}"
        );
        assert!(
            b.contains("/sandbox/.sandboard/escalate.json"),
            "must send unbound ambiguity to escalate: {b}"
        );
        assert!(
            b.contains("only if this card's intent/DoD")
                || b.contains("only if the Project prompt names")
                || b.contains("only when the Project prompt names"),
            "must gate clone on an explicit name: {b}"
        );
        assert!(
            !b.contains("Clone into `/sandbox/repo` per the Project prompt"),
            "old invite-to-guess wording must be gone: {b}"
        );
        assert!(
            !b.contains("Remotes for this run:"),
            "unbound must not claim structured Remotes: {b}"
        );
    }

    /// Complete RepoConfig (from pull_request after report) — Remotes name
    /// clone_target and must not order escalate-for-missing-clone.
    #[test]
    fn bound_remotes_briefing_includes_clone_target_without_escalate() {
        let bound = crate::schema::RepoConfig {
            upstream: "acme/widgets".into(),
            fork: String::new(),
            base: "main".into(),
        };
        assert!(bound.is_complete());
        let clone = bound.clone_target();
        let b = briefing(&grant(), BranchState::Fresh, "sandboard/card-189", &bound);
        assert!(
            b.contains("Remotes for this run:"),
            "must use structured Remotes when pull_request resolves: {b}"
        );
        assert!(
            b.contains(clone) || b.contains("acme/widgets"),
            "Remotes must include clone_target: {b}"
        );
        assert!(
            !b.contains("Do **not** guess which repository"),
            "must not order escalate-for-missing-clone when remotes are bound: {b}"
        );
        assert!(
            !b.contains("only if the Project prompt names")
                && !b.contains("only when the Project prompt names")
                && !b.contains("only if this card's intent/DoD"),
            "must not keep unbound escalate gate when remotes resolve: {b}"
        );

        let resume = resume_briefing(&grant(), &bound);
        assert!(
            resume.contains("Remotes for this run:"),
            "resume must carry structured Remotes too: {resume}"
        );
        assert!(
            resume.contains(clone) || resume.contains("acme/widgets"),
            "resume Remotes must include clone_target: {resume}"
        );
        assert!(
            !resume.contains("Do **not** guess which repository"),
            "resume must not escalate for missing clone: {resume}"
        );
    }

    /// Answering "Clone owner/name" must stop the Remotes block from
    /// re-instructing escalate on the next claim — that loop burned #146.
    #[test]
    fn unbound_briefing_honors_clone_decision_note() {
        let unbound = crate::schema::RepoConfig::default();
        let mut g = grant();
        g.notes = vec![
            "Decision: Clone sandboard-app/sandboard (suggested by beads External https://github.com/sandboard-app/sandboard/issues/204)"
                .into(),
        ];
        let b = briefing(&g, BranchState::Fresh, "sandboard/card-146", &unbound);
        assert!(b.contains("sandboard-app/sandboard"), "{b}");
        assert!(
            b.contains("already decided") || b.contains("human-decided"),
            "must treat Decision as the clone target: {b}"
        );
        assert!(
            b.contains("Do **not** re-escalate") || b.contains("do not re-ask"),
            "must forbid re-asking: {b}"
        );
        assert!(
            !b.contains("only if the Project prompt names"),
            "must not keep the escalate-when-prompt-silent gate: {b}"
        );

        let resume = resume_briefing(&g, &unbound);
        assert!(resume.contains("sandboard-app/sandboard"), "{resume}");
        assert!(
            resume.contains("already decided"),
            "resume must carry the decided clone too: {resume}"
        );
    }

    /// Pasted Proof facts must stop a meta-proof card from re-asking for a
    /// host Board probe after "host runs …; re-claim to document" (#174).
    #[test]
    fn briefing_honors_host_proof_facts_note() {
        let unbound = crate::schema::RepoConfig::default();
        let mut g = grant();
        g.notes = vec![
            "Decision: Host runs probe dispatch; re-claim this card to document".into(),
            "Proof: card=#180 pr_url=https://github.com/clankrshq/sandboard-sandbox-probe/pull/2 upstream=clankrshq/sandboard-sandbox-probe fork=clankrshq/sandboard-sandbox-probe".into(),
        ];
        let b = briefing(&g, BranchState::Fresh, "sandboard/card-174", &unbound);
        assert!(
            b.contains("Host proof facts are already on this card"),
            "must surface Proof facts: {b}"
        );
        assert!(
            b.contains("clankrshq/sandboard-sandbox-probe/pull/2"),
            "must include pr_url: {b}"
        );
        assert!(
            b.contains("Do **not** re-escalate"),
            "must forbid asking for another probe: {b}"
        );
        assert!(
            !b.contains("only if the Project prompt names"),
            "must not keep the unbound escalate gate when Proof facts exist: {b}"
        );

        let resume = resume_briefing(&g, &unbound);
        assert!(
            resume.contains("Host proof facts are already on this card"),
            "resume must carry Proof facts too: {resume}"
        );
    }

    #[test]
    fn briefing_mentions_verdict_split_protocol() {
        let b = briefing(
            &grant(),
            BranchState::Fresh,
            "sandboard/card-13",
            &cross_fork_repo(),
        );
        assert!(
            b.contains("/sandbox/.sandboard/split.json"),
            "briefing must mention /sandbox/.sandboard/split.json: {b}"
        );
        assert!(
            !b.contains("`.sandboard/split.json`"),
            "briefing must omit WORKDIR .sandboard/split.json: {b}"
        );
        assert!(
            b.contains("smaller slices of the same outcome"),
            "briefing must instruct slice-only splits: {b}"
        );
        assert!(
            b.contains("Approve") && b.contains("creates the sibling"),
            "briefing must say Approve creates siblings from split proposal: {b}"
        );
        assert!(
            b.contains("Do not invent work that belongs to another Project"),
            "briefing must prohibit inventing external work: {b}"
        );
        assert!(
            b.contains("Split and publish are mutually exclusive"),
            "briefing must state PR/split mutual exclusivity: {b}"
        );
    }

    #[test]
    fn briefing_initial_plan_requires_plan_json_not_pr() {
        let mut g = grant();
        g.title = crate::model::initial_plan_title("Test Project");
        let b = briefing(&g, BranchState::Fresh, "sandboard/card-92", &cross_fork_repo());
        assert!(
            b.contains("Initial plan"),
            "briefing must identify Initial plan: {b}"
        );
        assert!(
            b.contains("plan.json"),
            "briefing must require plan.json for Initial plan: {b}"
        );
        assert!(
            b.contains("Finish this card with `plan.json`")
                || b.contains("write `/sandbox/.sandboard/plan.json`"),
            "briefing must center plan.json for Initial plan: {b}"
        );
        assert!(
            b.contains("Skip `split.json` and `report.json`"),
            "briefing must tell Initial plan to skip split/report: {b}"
        );
        assert!(
            b.contains("Approve") && b.contains("Tasks"),
            "briefing must say Approve creates Tasks from plan.json: {b}"
        );
        assert!(
            !b.contains("Split and publish are mutually exclusive"),
            "Initial plan briefing must use the plan.json path, not impl exclusivity: {b}"
        );
    }

    #[test]
    fn plan_file_to_specs_synthesizes_keys() {
        let plan: PlanFile = serde_json::from_str(
            r#"{
                "summary": "cut",
                "tasks": [
                    {"title": "A", "intent": "do a", "definition_of_done": "a done"},
                    {"key": "b", "title": "B", "intent": "do b", "dod": "b done", "blocked_by_keys": ["t1"]}
                ]
            }"#,
        )
        .unwrap();
        let (summary, specs) = plan_file_to_specs(plan).unwrap();
        assert_eq!(summary, "cut");
        assert_eq!(specs[0].key, "t1");
        assert_eq!(specs[1].key, "b");
        assert_eq!(specs[1].blocked_by_keys, vec!["t1".to_string()]);
    }

    #[test]
    fn briefing_mentions_verdict_report_protocol() {
        let b = briefing(
            &grant(),
            BranchState::Fresh,
            "sandboard/card-17",
            &cross_fork_repo(),
        );
        assert!(
            b.contains("/sandbox/.sandboard/report.json"),
            "briefing must mention /sandbox/.sandboard/report.json: {b}"
        );
        assert!(
            !b.contains("`.sandboard/report.json`"),
            "briefing must omit WORKDIR .sandboard/report.json: {b}"
        );
        assert!(
            b.contains("diffstat"),
            "briefing must mention diffstat: {b}"
        );
    }

    #[test]
    fn parse_diffstat_sums_numstat_lines() {
        let sample = "10\t2\tsrc/main.rs\n3\t0\tREADME.md\n-\t-\timage.png\n";
        let (added, removed) = parse_diffstat(sample);
        assert_eq!(added, 13);
        assert_eq!(removed, 2);

        let empty = "";
        assert_eq!(parse_diffstat(empty), (0, 0));
    }

    #[test]
    fn split_file_deserializes_children() {
        let json = r#"{
            "children": [
                {
                    "title": "Part 1",
                    "intent": "Do part 1",
                    "definition_of_done": "Part 1 complete"
                },
                {
                    "title": "Part 2",
                    "intent": "Do part 2",
                    "dod": "Part 2 complete"
                },
                {
                    "title": "Part 3",
                    "intent": "Do part 3"
                }
            ]
        }"#;
        let split: SplitFile = serde_json::from_str(json).unwrap();
        assert_eq!(split.children.len(), 3);
        assert_eq!(split.children[0].title, "Part 1");
        assert_eq!(split.children[0].dod.as_deref(), Some("Part 1 complete"));
        assert_eq!(split.children[1].dod.as_deref(), Some("Part 2 complete"));
        assert_eq!(split.children[2].dod, None);
    }

    #[test]
    fn report_file_deserializes_url_base_head() {
        let json = r#"{
            "added": 10,
            "removed": 2,
            "gates": ["agent-reported"],
            "url": "https://github.com/sandboard-app/sandboard/pull/42",
            "base": { "repo": "sandboard-app/sandboard", "ref": "main" },
            "head": { "repo": "clankrshq/sandboard", "ref": "sandboard/card-7" }
        }"#;
        let rep: ReportFile = serde_json::from_str(json).unwrap();
        assert_eq!(rep.added, 10);
        let pr = report_to_pull_request(&rep).expect("pull_request");
        assert_eq!(pr.url, "https://github.com/sandboard-app/sandboard/pull/42");
        assert!(pr.has_forge_ends());
        assert_eq!(pr.head.as_ref().unwrap().repo, "clankrshq/sandboard");

        // Legacy pr_url alias still loads into url.
        let legacy: ReportFile = serde_json::from_str(
            r#"{"added":5,"removed":0,"pr_url":"https://github.com/sandboard-app/sandboard/pull/9"}"#,
        )
        .unwrap();
        assert_eq!(
            legacy.url.as_deref(),
            Some("https://github.com/sandboard-app/sandboard/pull/9")
        );

        let json_empty_url = r#"{
            "added": 5,
            "removed": 0,
            "pr_url": ""
        }"#;
        let rep_empty: ReportFile = serde_json::from_str(json_empty_url).unwrap();
        assert!(report_to_pull_request(&rep_empty).is_none());
    }

    #[tokio::test]
    async fn process_verdict_refuses_split_when_board_has_pr_url() {
        let board = test_board();
        let project = board
            .create(None, "project", "why", None, Origin::Human, true, None)
            .unwrap();
        let task = board
            .create(
                Some(project.id),
                "task",
                "intent",
                Some("dod".into()),
                Origin::Human,
                false,
                None,
            )
            .unwrap();
        let _ = board.transition(task.id, State::Shaping, "test", None);
        let _ = board.transition(task.id, State::Backlog, "test", None);
        let _ = board.claim(task.id, "agent-1", None, 60).unwrap();
        let _ = board.transition(task.id, State::Running, "agent-1", None);

        let pr_url = "https://github.com/sandboard-app/sandboard/pull/50";
        board.set_pr_url(task.id, Some(pr_url.to_string()));

        let dir = std::env::temp_dir().join(format!(
            "sandboard-test-split-pr-1-{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        ));
        let _ = std::fs::create_dir_all(&dir);
        let split_json_path = dir.join("split.json");
        std::fs::write(
            &split_json_path,
            r#"{
                "children": [
                    {"title": "Child A", "intent": "A", "definition_of_done": "A done"},
                    {"title": "Child B", "intent": "B", "definition_of_done": "B done"}
                ]
            }"#,
        )
        .unwrap();

        let os = verdict_openshell("split", &split_json_path, None, None);
        let cfg = repo_cfg();
        let handled = process_verdict(
            &board,
            &os,
            &cfg,
            "agent-1",
            task.id,
            "sandbox-1",
            "sandboard/card-1",
        )
        .await
        .unwrap();
        let _ = std::fs::remove_dir_all(&dir);

        assert!(handled);
        let item = board.get(task.id).unwrap();
        assert_eq!(item.state, State::NeedsHuman);
        assert_eq!(item.pr_url(), Some(pr_url));
        let esc = item.escalation.expect("escalation set");
        assert!(esc.question.contains("a PR already exists"));
    }

    #[tokio::test]
    async fn process_verdict_refuses_split_when_pr_detected_by_lookup() {
        let board = test_board();
        let project = board
            .create(None, "project", "why", None, Origin::Human, true, None)
            .unwrap();
        let task = board
            .create(
                Some(project.id),
                "task",
                "intent",
                Some("dod".into()),
                Origin::Human,
                false,
                None,
            )
            .unwrap();
        let _ = board.transition(task.id, State::Shaping, "test", None);
        let _ = board.transition(task.id, State::Backlog, "test", None);
        let _ = board.claim(task.id, "agent-1", None, 60).unwrap();
        let _ = board.transition(task.id, State::Running, "agent-1", None);
        assert!(board.get(task.id).unwrap().pr_url().is_none());

        let dir = std::env::temp_dir().join(format!(
            "sandboard-test-split-pr-2-{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        ));
        let _ = std::fs::create_dir_all(&dir);
        let split_json_path = dir.join("split.json");
        std::fs::write(
            &split_json_path,
            r#"{
                "children": [
                    {"title": "Child A", "intent": "A", "definition_of_done": "A done"},
                    {"title": "Child B", "intent": "B", "definition_of_done": "B done"}
                ]
            }"#,
        )
        .unwrap();

        let os = verdict_openshell(
            "split",
            &split_json_path,
            Some("https://github.com/sandboard-app/sandboard/pull/99"),
            None,
        );
        let cfg = repo_cfg();
        let handled = process_verdict(
            &board,
            &os,
            &cfg,
            "agent-1",
            task.id,
            "sandbox-1",
            "sandboard/card-1",
        )
        .await
        .unwrap();
        let _ = std::fs::remove_dir_all(&dir);

        assert!(handled);
        let item = board.get(task.id).unwrap();
        assert_eq!(item.state, State::NeedsHuman);
        assert_eq!(
            item.pr_url(),
            Some("https://github.com/sandboard-app/sandboard/pull/99")
        );
        let esc = item.escalation.expect("escalation set");
        assert!(esc.question.contains("a PR already exists"));
    }

    #[tokio::test]
    async fn process_verdict_refuses_split_when_children_are_off_theme() {
        let board = test_board();
        let project = board
            .create(
                None,
                "User Authentication",
                "Manage user accounts",
                None,
                Origin::Human,
                true,
                None,
            )
            .unwrap();
        let task = board
            .create(
                Some(project.id),
                "Implement OAuth2 login",
                "Handle authentication callback",
                Some("Login working".into()),
                Origin::Human,
                false,
                None,
            )
            .unwrap();
        let _ = board.transition(task.id, State::Shaping, "test", None);
        let _ = board.transition(task.id, State::Backlog, "test", None);
        let _ = board.claim(task.id, "agent-1", None, 60).unwrap();
        let _ = board.transition(task.id, State::Running, "agent-1", None);

        let dir = std::env::temp_dir().join(format!(
            "sandboard-test-split-offtheme-{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        ));
        let _ = std::fs::create_dir_all(&dir);
        let split_json_path = dir.join("split.json");
        std::fs::write(
            &split_json_path,
            r#"{
                "children": [
                    {"title": "Refactor database pool", "intent": "Tune connection limit", "definition_of_done": "Pool tuned"},
                    {"title": "Fix CSS margins", "intent": "Adjust padding in header", "definition_of_done": "CSS fixed"}
                ]
            }"#,
        )
        .unwrap();

        let os = verdict_openshell("split", &split_json_path, None, None);
        let cfg = repo_cfg();
        let handled = process_verdict(
            &board,
            &os,
            &cfg,
            "agent-1",
            task.id,
            "sandbox-1",
            "sandboard/card-1",
        )
        .await
        .unwrap();
        let _ = std::fs::remove_dir_all(&dir);

        assert!(handled);
        let item = board.get(task.id).unwrap();
        assert_eq!(item.state, State::NeedsHuman);
        let esc = item.escalation.expect("escalation set");
        assert!(esc.question.contains("refused by governor"));
        assert!(esc
            .question
            .contains("does not relate to parent card or project theme"));
    }

    #[tokio::test]
    async fn process_verdict_initial_plan_report_proposes_plan() {
        let board = test_board();
        let project = board
            .create(None, "Proj", "why", None, Origin::Human, true, None)
            .unwrap();
        let seed = board.init_plan(project.id).expect("init_plan");
        let seed_id = seed.id;
        let _ = board.transition(project.id, State::Shaping, "t", None);
        let _ = board.claim(seed_id, "agent-1", None, 60).unwrap();
        let _ = board.transition(seed_id, State::Running, "agent-1", None);

        let dir = std::env::temp_dir().join(format!(
            "sandboard-test-initial-plan-{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        ));
        let _ = std::fs::create_dir_all(&dir);
        let report_path = dir.join("report.json");
        std::fs::write(
            &report_path,
            r#"{"added":3,"removed":0,"pr_url":"https://github.com/sandboard-app/sandboard/pull/99"}"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("plan.json"),
            r#"{
                "summary": "webhook cut",
                "tasks": [
                    {"key":"a","title":"Ingress","intent":"webhooks","definition_of_done":"tests pass"},
                    {"key":"b","title":"Rebase","intent":"catch up","definition_of_done":"rebase queued","blocked_by_keys":["a"]}
                ]
            }"#,
        )
        .unwrap();

        let os = verdict_openshell("report", &report_path, None, None);
        let cfg = repo_cfg();
        let handled = process_verdict(
            &board,
            &os,
            &cfg,
            "agent-1",
            seed_id,
            "sandbox-1",
            "sandboard/card-ip",
        )
        .await
        .unwrap();
        let _ = std::fs::remove_dir_all(&dir);

        assert!(handled);
        let seed = board.get(seed_id).unwrap();
        assert_eq!(seed.state, State::Review);
        assert_eq!(
            seed.pr_url(),
            Some("https://github.com/sandboard-app/sandboard/pull/99")
        );
        let prop = seed.proposal.expect("proposal on Initial plan card");
        assert_eq!(prop.tasks.len(), 2);
        assert_eq!(prop.tasks[0].key, "a");
        assert_eq!(prop.tasks[1].blocked_by_keys, vec!["a".to_string()]);
        assert!(board.get(project.id).unwrap().plan.is_none());
    }

    #[tokio::test]
    async fn process_verdict_initial_plan_report_without_plan_escalates() {
        let board = test_board();
        let project = board
            .create(None, "Proj", "why", None, Origin::Human, true, None)
            .unwrap();
        let seed = board.init_plan(project.id).expect("init_plan");
        let seed_id = seed.id;
        let _ = board.transition(project.id, State::Shaping, "t", None);
        let _ = board.claim(seed_id, "agent-1", None, 60).unwrap();
        let _ = board.transition(seed_id, State::Running, "agent-1", None);

        let dir = std::env::temp_dir().join(format!(
            "sandboard-test-initial-plan-missing-{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        ));
        let _ = std::fs::create_dir_all(&dir);
        let report_path = dir.join("report.json");
        std::fs::write(
            &report_path,
            r#"{"added":1,"removed":0,"pr_url":"https://github.com/sandboard-app/sandboard/pull/98"}"#,
        )
        .unwrap();

        let os = verdict_openshell("report", &report_path, None, Some("plan.json"));
        let cfg = repo_cfg();
        let handled = process_verdict(
            &board,
            &os,
            &cfg,
            "agent-1",
            seed_id,
            "sandbox-1",
            "sandboard/card-ip2",
        )
        .await
        .unwrap();
        let _ = std::fs::remove_dir_all(&dir);

        assert!(handled);
        let seed = board.get(seed_id).unwrap();
        assert_eq!(seed.state, State::NeedsHuman);
        assert!(
            seed.escalation
                .as_ref()
                .is_some_and(|e| e.question.contains("plan.json")),
            "expected plan.json escalate, got {:?}",
            seed.escalation
        );
        assert!(board.get(project.id).unwrap().plan.is_none());
    }

    #[tokio::test]
    async fn initial_plan_without_verdict_escalates_instead_of_looking_up_an_empty_repo() {
        let board = test_board();
        let project = board
            .create(None, "Proj", "why", None, Origin::Human, true, None)
            .unwrap();
        let seed = board.init_plan(project.id).expect("init_plan");
        let seed_id = seed.id;
        let _ = board.transition(project.id, State::Shaping, "t", None);
        let _ = board.claim(seed_id, "agent-1", None, 60).unwrap();
        let _ = board.transition(seed_id, State::Running, "agent-1", None);

        let os = OpenShell::mock(
            |_| crate::openshell::Output {
                code: 0,
                stdout: String::new(),
                stderr: String::new(),
            },
            Duration::from_secs(5),
        );
        finish(
            &board,
            &os,
            &AgentConfig::default(),
            "agent-1",
            seed_id,
            "sandbox-1",
            "sandboard/card-ip3",
            &crate::openshell::Output {
                code: 0,
                stdout: String::new(),
                stderr: String::new(),
            },
        )
        .await
        .expect("missing initial-plan verdict should be handled");

        let seed = board.get(seed_id).unwrap();
        assert_eq!(seed.state, State::NeedsHuman);
        assert!(seed
            .escalation
            .as_ref()
            .is_some_and(|e| e.question.contains("plan.json")));
    }

    #[tokio::test]
    async fn process_verdict_refuses_report_when_pr_conflicting() {
        let board = test_board();
        let project = board
            .create(None, "project", "why", None, Origin::Human, true, None)
            .unwrap();
        let task = board
            .create(
                Some(project.id),
                "task",
                "intent",
                Some("dod".into()),
                Origin::Human,
                false,
                None,
            )
            .unwrap();
        let _ = board.transition(task.id, State::Shaping, "test", None);
        let _ = board.transition(task.id, State::Backlog, "test", None);
        let _ = board.claim(task.id, "agent-1", None, 60).unwrap();
        let _ = board.transition(task.id, State::Running, "agent-1", None);
        board.set_environment(task.id, Some("sandbox-keep".into()));
        board.set_conversation_id(task.id, Some("conv-keep".into()));

        let dir = std::env::temp_dir().join(format!(
            "sandboard-test-report-conflicting-{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        ));
        let _ = std::fs::create_dir_all(&dir);
        let report_path = dir.join("report.json");
        let pr_url = "https://github.com/sandboard-app/sandboard/pull/166";
        std::fs::write(
            &report_path,
            format!(r#"{{"added":2,"removed":1,"pr_url":"{pr_url}"}}"#),
        )
        .unwrap();

        let os = verdict_openshell_mergeable(
            "report",
            &report_path,
            Some(pr_url),
            Some("CONFLICTING"),
            None,
        );
        let cfg = repo_cfg();
        let handled = process_verdict(
            &board,
            &os,
            &cfg,
            "agent-1",
            task.id,
            "sandbox-1",
            "sandboard/card-166",
        )
        .await
        .unwrap();
        let _ = std::fs::remove_dir_all(&dir);

        assert!(handled);
        let item = board.get(task.id).unwrap();
        assert_eq!(
            item.state,
            State::Backlog,
            "must not reach Review while CONFLICTING"
        );
        assert_eq!(item.pr_url(), Some(pr_url));
        assert_eq!(
            item.last_bounce_reason.as_deref(),
            Some(CONFLICTING_PR_BOUNCE_REASON)
        );
        assert_eq!(
            item.environment.as_deref(),
            Some("sandbox-keep"),
            "sandbox must survive release"
        );
        assert_eq!(
            item.conversation_id.as_deref(),
            Some("conv-keep"),
            "conversation must survive release"
        );
    }

    #[tokio::test]
    async fn process_verdict_reports_when_mergeable_unknown() {
        let board = test_board();
        let project = board
            .create(None, "project", "why", None, Origin::Human, true, None)
            .unwrap();
        let task = board
            .create(
                Some(project.id),
                "task",
                "intent",
                Some("dod".into()),
                Origin::Human,
                false,
                None,
            )
            .unwrap();
        let _ = board.transition(task.id, State::Shaping, "test", None);
        let _ = board.transition(task.id, State::Backlog, "test", None);
        let _ = board.claim(task.id, "agent-1", None, 60).unwrap();
        let _ = board.transition(task.id, State::Running, "agent-1", None);

        let dir = std::env::temp_dir().join(format!(
            "sandboard-test-report-unknown-{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        ));
        let _ = std::fs::create_dir_all(&dir);
        let report_path = dir.join("report.json");
        let pr_url = "https://github.com/sandboard-app/sandboard/pull/167";
        std::fs::write(
            &report_path,
            format!(r#"{{"added":1,"removed":0,"pr_url":"{pr_url}"}}"#),
        )
        .unwrap();

        // UNKNOWN must not hard-fail — treat as proceed to Review.
        let os = verdict_openshell_mergeable(
            "report",
            &report_path,
            Some(pr_url),
            Some("UNKNOWN"),
            None,
        );
        let cfg = repo_cfg();
        let handled = process_verdict(
            &board,
            &os,
            &cfg,
            "agent-1",
            task.id,
            "sandbox-1",
            "sandboard/card-167",
        )
        .await
        .unwrap();
        let _ = std::fs::remove_dir_all(&dir);

        assert!(handled);
        let item = board.get(task.id).unwrap();
        assert_eq!(item.state, State::Review);
        assert_eq!(item.pr_url(), Some(pr_url));
    }

    /// Project override wins over the global default; unset Project inherits
    /// the default when the supervisor builds create knobs.
    #[test]
    fn sandbox_create_uses_project_override_over_default() {
        use crate::model::{Origin, SandboxProfile};

        let mut schema = crate::schema::Schema::default();
        schema.execution.agents.image = "yaml-image:fallback".into();
        schema.execution.agents.policy = "version: 1\n# yaml-fallback\n".into();
        schema.execution.agents.cpu = Some("1".into());
        schema.execution.agents.memory = Some("1Gi".into());

        let board = Arc::new(crate::store::Board::new(
            schema,
            std::env::temp_dir().join(format!(
                "sandboard-test-sbx-resolve-{}.json",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            )),
        ));

        let default_policy = "version: 1\n# default\n";
        let heavy_policy = "version: 1\n# heavy\n";
        board
            .upsert_openshell_policy(crate::model::OpenShellPolicy {
                id: "default-pol".into(),
                name: "Default policy".into(),
                yaml: default_policy.into(),
            })
            .unwrap();
        board
            .upsert_openshell_policy(crate::model::OpenShellPolicy {
                id: "heavy-pol".into(),
                name: "Heavy policy".into(),
                yaml: heavy_policy.into(),
            })
            .unwrap();
        board
            .upsert_sandbox_profile(SandboxProfile {
                id: "default".into(),
                name: "Default".into(),
                image: "default-image:1".into(),
                policy_id: "default-pol".into(),
                policy_inline_legacy: None,
                cpu: Some("2".into()),
                memory: Some("4Gi".into()),
                engine: None,
                model: None,
                provider_names: Vec::new(),
                mcp_server_ids: Vec::new(),
                env: Default::default(),
                prompt: None,
                shipped: false,
            })
            .unwrap();
        board
            .upsert_sandbox_profile(SandboxProfile {
                id: "heavy".into(),
                name: "Heavy".into(),
                image: "heavy-image:1".into(),
                policy_id: "heavy-pol".into(),
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
            .unwrap();
        board.set_default_sandbox_profile("default").unwrap();

        let project = board
            .create(None, "Sbx Proj", "why", None, Origin::Human, true, None)
            .unwrap();
        let task = board
            .create(
                Some(project.id),
                "task",
                "do it",
                Some("done".into()),
                Origin::Human,
                false,
                None,
            )
            .unwrap();

        // Unset Project → global default.
        let unset = board.resolve_sandbox_create(task.id);
        let unset_spec = sandbox_spec_for_card(task.id, "sandboard-card-test", &unset, &[]);
        assert_eq!(unset.profile_id.as_deref(), Some("default"));
        assert_eq!(unset_spec.from, "default-image:1");
        assert_eq!(unset_spec.policy.as_deref(), Some(default_policy));
        assert_eq!(unset_spec.cpu.as_deref(), Some("2"));
        assert_eq!(unset_spec.memory.as_deref(), Some("4Gi"));

        // Project override beats default.
        board
            .set_project_sandbox_profile(project.id, Some("heavy".into()))
            .unwrap();
        let over = board.resolve_sandbox_create(task.id);
        let over_spec = sandbox_spec_for_card(task.id, "sandboard-card-test", &over, &[]);
        assert_eq!(over.profile_id.as_deref(), Some("heavy"));
        assert_eq!(over_spec.from, "heavy-image:1");
        assert_eq!(over_spec.policy.as_deref(), Some(heavy_policy));
        assert_eq!(over_spec.cpu.as_deref(), Some("8"));
        assert_eq!(over_spec.memory.as_deref(), Some("16Gi"));
    }

    #[tokio::test]
    async fn setup_agy_auth_writes_placeholder_token_via_exec_not_host_upload() {
        use std::sync::Arc;
        let path = std::env::temp_dir().join(format!(
            "sandboard-agy-auth-board-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let board = Arc::new(crate::store::Board::new(
            crate::schema::Schema::default(),
            path,
        ));
        let mut config = std::collections::BTreeMap::new();
        config.insert(
            crate::antigravity::CONFIG_PROJECT.into(),
            "test-agy-project".into(),
        );
        config.insert(
            crate::antigravity::CONFIG_LOCATION.into(),
            "global".into(),
        );
        board.upsert_openshell_provider(
            crate::model::OpenShellProviderDesired {
                name: "antigravity".into(),
                provider_type: "antigravity".into(),
                config,
                credentials_sealed: None,
                credential_keys: vec!["ANTIGRAVITY_ACCESS_TOKEN".into()],
                refresh: None,
            }
            .normalized(),
        );

        let seen = Arc::new(parking_lot::Mutex::new(Vec::<Vec<String>>::new()));
        let seen_c = seen.clone();
        let os = OpenShell::mock(
            move |args| {
                seen_c.lock().push(args.to_vec());
                Output {
                    code: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                }
            },
            Duration::from_secs(5),
        );
        setup_agy_auth(&os, "sandboard-cockpit", &board)
            .await
            .expect("setup");

        let calls = seen.lock().clone();
        assert!(
            calls.iter().all(|a| !a.iter().any(|s| s == "upload")),
            "must not upload host oauth file: {calls:?}"
        );
        let exec = calls
            .iter()
            .find(|a| a.iter().any(|s| s == "exec"))
            .expect("exec call");
        let script = exec
            .iter()
            .find(|s| s.contains("ANTIGRAVITY_ACCESS_TOKEN"))
            .expect("inject script");
        assert!(
            script.contains("antigravity-oauth-token"),
            "token file path: {script}"
        );
        assert!(
            script.contains("auth_method") && script.contains("access_token"),
            "nested token shape: {script}"
        );
        assert!(
            script.contains("test-agy-project"),
            "must use Board antigravity config project: {script}"
        );
        assert!(
            script.contains("GOOGLE_CLOUD_PROJECT")
                && script.contains("GOOGLE_CLOUD_QUOTA_PROJECT")
                && script.contains("sandboard-cloud.env"),
            "must export Board project over Vertex seat env: {script}"
        );
        assert!(
            script.contains("enableTelemetry"),
            "settings.json: {script}"
        );
    }

    #[test]
    fn durable_agent_runtime_overlays_engine_into_effective_agents() {
        let path = std::env::temp_dir().join(format!(
            "sandboard-test-agent-rt-spec-{}.json",
            std::process::id()
        ));
        let mut schema = crate::schema::Schema::default();
        schema.execution.agents.engine = "cursor".into();
        let board = crate::store::Board::new(schema, path);
        assert!(board.seed_agent_runtime_if_empty());

        board.set_agent_runtime(crate::model::AgentRuntimeConfig {
            engine: "claude".into(),
            max_concurrent: 1,
            agent_timeout_secs: 1800,
            max_attempts: 3,
            ..Default::default()
        });

        let cfg = board.effective_agents();
        assert_eq!(cfg.engine, "claude");

        board.upsert_openshell_provider(crate::model::OpenShellProviderDesired {
            name: "vertex".into(),
            provider_type: "google-vertex-ai".into(),
            config: Default::default(),
            credentials_sealed: None,
            credential_keys: Vec::new(),
            refresh: None,
        });
        board.upsert_openshell_provider(crate::model::OpenShellProviderDesired {
            name: "local-only".into(),
            provider_type: "github".into(),
            config: Default::default(),
            credentials_sealed: None,
            credential_keys: Vec::new(),
            refresh: None,
        });
        let resolved = crate::model::ResolvedSandboxCreate {
            image: "img:1".into(),
            policy: "version: 1\n".into(),
            cpu: None,
            memory: None,
            engine: None,
            model: None,
            profile_id: Some("default".into()),
            providers: vec!["vertex".into(), "missing".into()],
            mcp_server_ids: Vec::new(),
            env: Default::default(),
            prompt: None,
        };
        let attach = board.attach_providers_for_resolved(&resolved);
        let spec = sandbox_spec_for_card(1, "sandboard-card-1-a1", &resolved, &attach);
        assert_eq!(spec.from, "img:1");
        assert_eq!(spec.policy.as_deref(), Some("version: 1\n"));
        assert_eq!(spec.providers, vec!["vertex".to_string()]);
    }

    fn review_awaiting_mergeable_board() -> (SharedBoard, crate::model::WorkItem) {
        let board = Arc::new(crate::store::Board::new(
            crate::schema::Schema::default(),
            std::env::temp_dir().join(format!(
                "sandboard-test-awaiting-mergeable-{}.json",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            )),
        ));
        let project = board
            .create(
                None,
                "Mergeable Proj",
                "why",
                None,
                Origin::Human,
                true,
                None,
            )
            .unwrap();
        let task = board
            .create(
                Some(project.id),
                "Review mergeable",
                "catch up",
                Some("checked".into()),
                Origin::Human,
                false,
                None,
            )
            .unwrap();
        board
            .transition(task.id, State::Shaping, "test", None)
            .unwrap();
        board
            .transition(task.id, State::Backlog, "test", None)
            .unwrap();
        board
            .transition(task.id, State::Claimed, "agent", None)
            .unwrap();
        board
            .transition(task.id, State::Review, "agent", None)
            .unwrap();
        board.set_pull_request(
            task.id,
            Some(crate::model::PullRequest {
                url: format!("https://github.com/sandboard-app/sandboard/pull/{}", task.id),
                base: Some(crate::model::PullRequestEnd::new("sandboard-app/sandboard", "main")),
                head: Some(crate::model::PullRequestEnd::new(
                    "sandboard-app/sandboard",
                    crate::schema::card_branch_name(task.id),
                )),
                ..Default::default()
            }),
        );
        board.dispatch_rebase(task.id).unwrap();
        let item = board.get(task.id).unwrap();
        (board, item)
    }

    fn fixed_mergeable_fetch(
        check: crate::github_app::PrConflictCheck,
    ) -> impl for<'a> FnMut(&'a SharedBoard, &'a str) -> MergeableFetchFut<'a> {
        move |_board, _pr_url| {
            let check = check.clone();
            Box::pin(async move { Ok(Some(check)) })
        }
    }

    async fn process_awaiting_mergeable_checks_with<F>(
        board: &SharedBoard,
        cfg: &AgentConfig,
        fetch: F,
    ) -> Vec<Result<crate::model::WorkItem, String>>
    where
        F: for<'a> FnMut(&'a SharedBoard, &'a str) -> MergeableFetchFut<'a>,
    {
        let awaiting = board.list_awaiting_rebase();
        observe_review_catch_up_with(board, cfg, awaiting, fetch).await
    }

    async fn process_main_advanced_review_catch_up_with<F>(
        board: &SharedBoard,
        cfg: &AgentConfig,
        advanced_repo: &str,
        fetch: F,
    ) -> Vec<Result<crate::model::WorkItem, String>>
    where
        F: for<'a> FnMut(&'a SharedBoard, &'a str) -> MergeableFetchFut<'a>,
    {
        let candidates = board.identify_review_prs_for_main_advanced(advanced_repo);
        observe_review_catch_up_with(board, cfg, candidates, fetch).await
    }

    /// MERGEABLE clears a retry queue and keeps the card in Review.
    #[tokio::test]
    async fn process_awaiting_mergeable_checks_mergeable_stays_review() {
        let (board, item) = review_awaiting_mergeable_board();
        assert!(item.rebase_requested);

        let results = process_awaiting_mergeable_checks_with(
            &board,
            &repo_cfg(),
            fixed_mergeable_fetch(crate::github_app::PrConflictCheck {
                mergeable: crate::github_app::PrMergeableState::Mergeable,
                base_ref: Some("main".into()),
            }),
        )
        .await;
        assert_eq!(results.len(), 1);
        let updated = results[0].as_ref().unwrap();
        assert_eq!(updated.state, State::Review);
        assert!(!updated.rebase_requested);
        assert!(!updated.awaiting_dispatch);
    }

    /// CONFLICTING bounces to Backlog with a BINDING note for the next claim.
    #[tokio::test]
    async fn process_awaiting_mergeable_checks_conflicting_bounces_backlog() {
        let (board, item) = review_awaiting_mergeable_board();
        assert!(item.rebase_requested);

        let results = process_awaiting_mergeable_checks_with(
            &board,
            &repo_cfg(),
            fixed_mergeable_fetch(crate::github_app::PrConflictCheck {
                mergeable: crate::github_app::PrMergeableState::Conflicting,
                base_ref: Some("main".into()),
            }),
        )
        .await;
        assert_eq!(results.len(), 1);
        let updated = results[0].as_ref().unwrap();
        assert_eq!(updated.state, State::Backlog);
        assert!(!updated.rebase_requested);
        assert!(updated
            .last_bounce_reason
            .as_deref()
            .unwrap_or("")
            .contains("CONFLICTING"));
        assert!(updated
            .notes
            .iter()
            .any(|n| n.text.contains("BINDING")
                && n.text.contains("do-not-re-report-while-CONFLICTING")));
    }

    /// UNKNOWN leaves the card queued — GitHub has not finished computing.
    #[tokio::test]
    async fn process_awaiting_mergeable_checks_unknown_retries() {
        let (board, item) = review_awaiting_mergeable_board();
        assert!(item.rebase_requested);

        let results = process_awaiting_mergeable_checks_with(
            &board,
            &repo_cfg(),
            fixed_mergeable_fetch(crate::github_app::PrConflictCheck {
                mergeable: crate::github_app::PrMergeableState::Unknown,
                base_ref: Some("main".into()),
            }),
        )
        .await;
        assert!(results.is_empty());
        let still = board.get(item.id).unwrap();
        assert_eq!(still.state, State::Review);
        assert!(
            still.rebase_requested,
            "UNKNOWN must leave rebase_requested queued for the next sweep"
        );
    }

    /// Tip catch-up observes mergeable first: MERGEABLE is a silent Review no-op;
    /// live Running cards on the advancing upstream still get steer + park/unpark.
    #[tokio::test]
    async fn main_advanced_review_mergeable_is_noop_while_live_run_steered() {
        let mut schema = crate::schema::Schema::default();
        schema.execution.agents.repo.upstream = "sandboard-app/sandboard".into();
        let board = Arc::new(crate::store::Board::new(
            schema,
            std::env::temp_dir().join(format!(
                "sandboard-test-main-adv-mergeable-{}.json",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            )),
        ));
        let project = board
            .create(
                None,
                "Idle Race Proj",
                "why",
                None,
                Origin::Human,
                true,
                None,
            )
            .unwrap();
        let done = board
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
        let review = board
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
        let running = board
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

        board
            .transition(done.id, State::Shaping, "test", None)
            .unwrap();
        board
            .transition(done.id, State::Done, "test", None)
            .unwrap();

        board
            .transition(review.id, State::Shaping, "test", None)
            .unwrap();
        board
            .transition(review.id, State::Backlog, "test", None)
            .unwrap();
        board
            .transition(review.id, State::Claimed, "agent", None)
            .unwrap();
        board
            .transition(review.id, State::Review, "agent", None)
            .unwrap();
        board.set_pull_request(
            review.id,
            Some(crate::model::PullRequest {
                url: format!("https://github.com/sandboard-app/sandboard/pull/{}", review.id),
                base: Some(crate::model::PullRequestEnd::new("sandboard-app/sandboard", "main")),
                head: Some(crate::model::PullRequestEnd::new(
                    "sandboard-app/sandboard",
                    crate::schema::card_branch_name(review.id),
                )),
                ..Default::default()
            }),
        );

        board
            .transition(running.id, State::Shaping, "test", None)
            .unwrap();
        board
            .transition(running.id, State::Backlog, "test", None)
            .unwrap();
        board
            .transition(running.id, State::Claimed, "agent", None)
            .unwrap();
        board
            .transition(running.id, State::Running, "agent", None)
            .unwrap();

        board.notify_main_advanced(
            "sandboard-app/sandboard",
            "refs/heads/main",
            Some("idle-race-sha".into()),
        );

        let running_after = board.get(running.id).unwrap();
        assert_eq!(running_after.state, State::Backlog);
        assert!(running_after.awaiting_dispatch);
        assert!(
            running_after
                .notes
                .iter()
                .any(|n| n.text.contains("idle-race-sha") && n.text.contains("upstream/main")),
            "Running still gets steer + park/unpark: {:?}",
            running_after.notes
        );

        let review_before = board.get(review.id).unwrap();
        assert_eq!(review_before.state, State::Review);
        assert!(
            !review_before.rebase_requested,
            "MainAdvanced must not set rebase_requested before mergeable observation"
        );
        assert!(
            !review_before
                .notes
                .iter()
                .any(|n| n.text.contains("Main advanced")),
            "Review must not reuse the Running steer path: {:?}",
            review_before.notes
        );

        let results = process_main_advanced_review_catch_up_with(
            &board,
            &repo_cfg(),
            "sandboard-app/sandboard",
            fixed_mergeable_fetch(crate::github_app::PrConflictCheck {
                mergeable: crate::github_app::PrMergeableState::Mergeable,
                base_ref: Some("main".into()),
            }),
        )
        .await;
        assert!(
            results.is_empty(),
            "MERGEABLE with no prior queue is a silent no-op: {results:?}"
        );
        let updated = board.get(review.id).unwrap();
        assert_eq!(updated.state, State::Review);
        assert!(
            !updated.rebase_requested,
            "MERGEABLE leaves no catch-up work signal"
        );
        assert!(!updated.awaiting_dispatch);
    }

    /// Tip catch-up CONFLICTING bounces Review to Backlog with a binding note.
    #[tokio::test]
    async fn main_advanced_review_conflicting_bounces_backlog() {
        let board = Arc::new(crate::store::Board::new(
            crate::schema::Schema::default(),
            std::env::temp_dir().join(format!(
                "sandboard-test-main-adv-conflicting-{}.json",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            )),
        ));
        let project = board
            .create(
                None,
                "Conflict Tip Proj",
                "why",
                None,
                Origin::Human,
                true,
                None,
            )
            .unwrap();
        let review = board
            .create(
                Some(project.id),
                "Conflicts After Main",
                "intent",
                Some("dod".into()),
                Origin::Human,
                false,
                None,
            )
            .unwrap();
        board
            .transition(review.id, State::Shaping, "test", None)
            .unwrap();
        board
            .transition(review.id, State::Backlog, "test", None)
            .unwrap();
        board
            .transition(review.id, State::Claimed, "agent", None)
            .unwrap();
        board
            .transition(review.id, State::Review, "agent", None)
            .unwrap();
        board.set_pull_request(
            review.id,
            Some(crate::model::PullRequest {
                url: format!("https://github.com/sandboard-app/sandboard/pull/{}", review.id),
                base: Some(crate::model::PullRequestEnd::new("sandboard-app/sandboard", "main")),
                head: Some(crate::model::PullRequestEnd::new(
                    "sandboard-app/sandboard",
                    crate::schema::card_branch_name(review.id),
                )),
                ..Default::default()
            }),
        );

        board.notify_main_advanced(
            "sandboard-app/sandboard",
            "refs/heads/main",
            Some("conflict-sha".into()),
        );
        assert!(!board.get(review.id).unwrap().rebase_requested);

        let results = process_main_advanced_review_catch_up_with(
            &board,
            &repo_cfg(),
            "sandboard-app/sandboard",
            fixed_mergeable_fetch(crate::github_app::PrConflictCheck {
                mergeable: crate::github_app::PrMergeableState::Conflicting,
                base_ref: Some("main".into()),
            }),
        )
        .await;
        assert_eq!(results.len(), 1);
        let updated = results[0].as_ref().unwrap();
        assert_eq!(updated.state, State::Backlog);
        assert!(!updated.rebase_requested);
        assert!(updated
            .notes
            .iter()
            .any(|n| n.text.contains("BINDING")
                && n.text.contains("do-not-re-report-while-CONFLICTING")));
    }

    /// Tip catch-up UNKNOWN queues rebase_requested so the sweeper can retry.
    #[tokio::test]
    async fn main_advanced_review_unknown_queues_retry() {
        let board = Arc::new(crate::store::Board::new(
            crate::schema::Schema::default(),
            std::env::temp_dir().join(format!(
                "sandboard-test-main-adv-unknown-{}.json",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            )),
        ));
        let project = board
            .create(
                None,
                "Unknown Tip Proj",
                "why",
                None,
                Origin::Human,
                true,
                None,
            )
            .unwrap();
        let review = board
            .create(
                Some(project.id),
                "Mergeable Pending",
                "intent",
                Some("dod".into()),
                Origin::Human,
                false,
                None,
            )
            .unwrap();
        board
            .transition(review.id, State::Shaping, "test", None)
            .unwrap();
        board
            .transition(review.id, State::Backlog, "test", None)
            .unwrap();
        board
            .transition(review.id, State::Claimed, "agent", None)
            .unwrap();
        board
            .transition(review.id, State::Review, "agent", None)
            .unwrap();
        board.set_pull_request(
            review.id,
            Some(crate::model::PullRequest {
                url: format!("https://github.com/sandboard-app/sandboard/pull/{}", review.id),
                base: Some(crate::model::PullRequestEnd::new("sandboard-app/sandboard", "main")),
                head: Some(crate::model::PullRequestEnd::new(
                    "sandboard-app/sandboard",
                    crate::schema::card_branch_name(review.id),
                )),
                ..Default::default()
            }),
        );

        board.notify_main_advanced(
            "sandboard-app/sandboard",
            "refs/heads/main",
            Some("unknown-sha".into()),
        );
        let _ = process_main_advanced_review_catch_up_with(
            &board,
            &repo_cfg(),
            "sandboard-app/sandboard",
            fixed_mergeable_fetch(crate::github_app::PrConflictCheck {
                mergeable: crate::github_app::PrMergeableState::Unknown,
                base_ref: Some("main".into()),
            }),
        )
        .await;
        let still = board.get(review.id).unwrap();
        assert_eq!(still.state, State::Review);
        assert!(
            still.rebase_requested,
            "UNKNOWN must queue rebase_requested for the next sweep"
        );
    }

    /// Webhook-equivalent path: MainAdvanced steers a live run, park+unpark
    /// queues resume, and the supervisor can claim again with sandbox preserved.
    #[tokio::test]
    async fn main_advanced_live_steer_resume_path_claimable_after_unpark() {
        use crate::model::State;

        let mut schema = crate::schema::Schema::default();
        schema.execution.agents.repo.upstream = "sandboard-app/sandboard".into();
        let board = Arc::new(crate::store::Board::new(
            schema,
            std::env::temp_dir().join(format!(
                "sandboard-test-main-adv-resume-{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            )),
        ));
        let project = board
            .create(
                None,
                "Resume Path Proj",
                "why",
                None,
                Origin::Human,
                true,
                None,
            )
            .unwrap();
        let running = board
            .create(
                Some(project.id),
                "Live Resume",
                "intent",
                Some("dod".into()),
                Origin::Human,
                false,
                None,
            )
            .unwrap();
        board
            .transition(running.id, State::Shaping, "test", None)
            .unwrap();
        board
            .transition(running.id, State::Backlog, "test", None)
            .unwrap();
        board
            .transition(running.id, State::Claimed, "agent", None)
            .unwrap();
        board
            .transition(running.id, State::Running, "agent", None)
            .unwrap();
        board.set_environment(running.id, Some("sandboard-card-resume-sandbox".into()));
        board.set_conversation_id(running.id, Some("conv-resume-main".into()));

        let steered = board.notify_main_advanced(
            "sandboard-app/sandboard",
            "refs/heads/main",
            Some("resume-path-sha".into()),
        );
        assert_eq!(steered, vec![running.id]);

        let catch_up = process_main_advanced_review_catch_up_with(
            &board,
            &repo_cfg(),
            "sandboard-app/sandboard",
            fixed_mergeable_fetch(crate::github_app::PrConflictCheck {
                mergeable: crate::github_app::PrMergeableState::Mergeable,
                base_ref: Some("main".into()),
            }),
        )
        .await;
        assert!(
            catch_up.is_empty(),
            "no Review PRs in this fixture: {catch_up:?}"
        );

        let after_steer = board.get(running.id).unwrap();
        assert_eq!(after_steer.state, State::Backlog);
        assert!(after_steer.awaiting_dispatch);
        assert!(!after_steer.parked);
        assert_eq!(
            after_steer.environment.as_deref(),
            Some("sandboard-card-resume-sandbox")
        );
        assert_eq!(
            after_steer.conversation_id.as_deref(),
            Some("conv-resume-main")
        );
        assert!(
            after_steer.notes.iter().any(|n| {
                n.text.contains("resume-path-sha") && n.text.contains("Main advanced")
            }),
            "steer note must survive webhook path: {:?}",
            after_steer.notes
        );

        let awaiting = board.list_awaiting_dispatch();
        assert!(
            awaiting.iter().any(|i| i.id == running.id),
            "supervisor must see the steered card for resume: {awaiting:?}"
        );
        assert!(board.may_claim(running.id));

        let grant = board
            .claim(running.id, "agent-resume", None, 60)
            .expect("resume claim after main-advanced steer");
        assert_eq!(grant.item_id, running.id);
        let after_claim = board.get(running.id).unwrap();
        assert_eq!(after_claim.state, State::Claimed);
        assert_eq!(
            after_claim.conversation_id.as_deref(),
            Some("conv-resume-main"),
            "resume claim must keep conversation for reclaim"
        );
    }
}
