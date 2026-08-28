# AGENTS.md

Orientation for the **operator** agent sitting with the human. If you are an
agent sandboard dispatched to work on a card, you already have a briefing — ignore
this file.

**Operator mode:** drive sandboard via MCP (`{board-origin}/mcp`) — operator tools
only; no worker verbs. Do not implement product work by editing this tree;
shape Projects/Plans, triage Needs You / Review, let sandboxed workers open PRs.
See [`.cursor/rules/sandboard-operator.mdc`](.cursor/rules/sandboard-operator.mdc).

Start with [`docs/index.md`](docs/index.md) and
[`docs/concepts.md`](docs/concepts.md).

## What this is

An agent orchestrator that dispatches work against **its own source**. Moving a
card *is* an action. sandboard claims a card, runs an agent in an OpenShell sandbox,
and the agent opens a same-repo PR (`sandboard/card-*` → `main`) that a human merges.

## The invariants worth protecting

**One state machine.** Every mutation — UI, MCP, supervisor — goes through
`Board` in `src/store.rs`. No transport holds state-machine logic. If you find
yourself encoding a rule in `api.rs` or `mcp.rs`, it belongs in `machine.rs` or
`store.rs` instead.

**Workers cannot reach sandboard.** The card agent gets no network path to sandboard. The
supervisor calls `claim`/`heartbeat`/`report` on its behalf. An agent that
could reach the board’s MCP could approve its own review.

**Liveness is observed.** It is parsed from the agent's output stream. Do not
add a timer-based keepalive — it would assert liveness without evidence.

**Merging is human.** Approving in sandboard surfaces the PR. It does not merge.

**Feature branches are writable; `main` is human-gated.** The GitHub ruleset
keeps the default branch owner-only. Agents use the App installation on
`shanemcd` with `fork` = `upstream` = `sandboard-app/sandboard`, push `sandboard/card-*`, and
open PRs; humans merge.

## Conventions

Comments explain **why**, not what. The existing code reads like prose and
argues with itself where a decision was close; match that. A comment that
restates the line below it is noise.

**Describe how it works now.** Docs, UI copy, MCP descriptions, briefings, and
comments must make sense to a reader who never saw the previous design. Prefer
“Initial plan finishes with `plan.json`; Approve creates Tasks” over arguing
with removed designs. Bug-history *why* notes that justify a still-present
invariant are fine; teaching the product by arguing with the past is not.

Tests live next to what they test. `machine.rs` holds the lifecycle
invariants; other modules test the things that break silently — argv shape,
shell quoting, config validation. Prefer a test that names the
failure it prevents over one that names the function it calls.

Before you finish: `cargo test` and `cargo clippy --all-targets -- -D warnings`
must both be clean. A card's sandbox has no pre-baked sandboard build cache —
`cargo`/`npm` reach crates.io/npm live — so `--offline` no longer applies
there; `--locked` still does.

Stage specific paths. `git add -A` has committed unintended local state here
before.

## Things that will waste your time if you don't know them

**Everything in the sandbox stack fails as a hang, not an error.** Denied
egress, a missing credential, a wedged relay — all silence. Every exec needs a
deadline; treat silence as failure. This shaped `openshell.rs` entirely.

**Don't script what the agent can drive.** The supervisor asks GitHub what
happened after publish; it does not run `gh pr create` / push itself. Before
adding a shell script to the supervisor, ask whether the briefing could say it
instead.

**The image's `ENV` does not reach `openshell sandbox exec`.** Pass what the
agent needs explicitly in `agent_env`, or install wrappers on the default PATH.

**`sandbox upload` takes a destination directory**, and the destination must
already exist.

**The podman machine stops on its own.** Classify that as infrastructure, not
as the card failing — see `is_infrastructure`.

**Rebase onto upstream `main`.** Fetch and rebase onto the default-branch tip;
do not treat a stale local `main` as truth.

## Where things are

| Path | What |
|---|---|
| `src/machine.rs` | Legal transitions and the two invariants |
| `src/store.rs` | The board — the only write path |
| `src/supervisor.rs` | Dispatch, per-card lifecycle, briefing, lease sweeping |
| `src/openshell.rs` | In-process gRPC client to the OpenShell gateway (board mTLS); every call has a deadline |
| `src/cockpit_chat.rs` | Host-mediated cockpit chat bridge (prompt → sandbox → SSE) |
| `src/mcp.rs` | Operator MCP tools; supervisor keeps worker verbs |
| `sandbox/` | Containerfile, network policy |
| `web/` | React UI + `npm run shots` screenshot harness |
| `docs/sandbox.md` | How a sandboxed run works and the gotchas that matter. Read before touching `sandbox/`. |

## Environment

Model credentials for sandboxed agents come from OpenShell providers (Settings
→ OpenShell Providers / shipped `github-app`), not from process boot. GitHub
git + `gh` in the sandbox use the App installation token (`GH_TOKEN`) for
`execution.agents.repo` (`upstream` / `fork` / `base`).

## Working with the human here

They will tell you when the board looks wrong, and they have been right every
time it mattered. Check the evidence before defending the code — twice in one
session a confident "it's fine" was based on a log that was not capturing
anything.

State corrections plainly and move on. Do not narrate the mistake at length.
