# Workflow

Day-to-day operation once the board is up. To enable sandboxed agents first, use
the empty-board **Welcome** or **Help** OpenShell + sandbox guide, then
[Your first agent](first-agent.md).

## The happy path

1. **Create a Project** with a `clone_repo` (`owner/name`). That repo is what
   the planning agent clones. sandboard creates an **Initial plan** Task that already
   names that repo.
2. **Start** the Initial plan. The agent clones, reads, and writes `plan.json`
   proposing sibling Tasks. The card lands in **Review**.
3. **Approve** it. The proposal becomes real Tasks under the Project. Approving
   does not auto-start them unless the Project's auto mode is already on.
4. **Start** each Task (or turn auto mode on).
5. The worker clones, works, and opens a PR. The card lands in **Review**.
6. **You merge on GitHub.** A webhook (or Forge polling) moves the card to Done.

Read the whole plan before approving. One good card can still be the wrong plan.

`propose_breakdown` is for manually replanning a Project. A card that turns out
too big uses the same path in reverse: the agent writes `split.json`, the card
goes to Review with a proposal, and Approve creates the siblings.

## Adding Tasks to an existing Project

After a Project exists, you can add more Backlog Tasks without re-running
Initial plan:

| Surface | How |
|---|---|
| **Board UI** | **Create Task** on the Project swimlane (when the lane is open) or in the Project Detail drawer. Title, intent, definition of done; optional blockers from sibling Tasks. Not on the empty-board Welcome path. |
| **MCP** | Operator tool `create_task` — parent Project id, title, intent, `definition_of_done`, optional `blocked_by`. Same Board path as `POST /api/items` with `parent`. |

The new card lands in **Backlog**, ready for **Start** / `dispatch` (or Project
auto mode). Parent must be a Project — nesting under a Task is refused.

Each Task must name its clone target (`owner/name`) in intent and/or definition
of done. When the caller omits an explicit `Clone repository:` line, sandboard stamps
the Project default from Project intent when one is present; otherwise Remotes
escalate rather than guessing.

**Approve** still only materializes proposals (Initial plan or `split.json`) and
never merges. Creating a Task ad-hoc is not Approve — it is a separate human
create path.

## Which repo an agent clones

Agents clone the repository named in the card's **intent**, **definition of
done**, or **notes**. The supervisor never clones — the agent does.

**Cold start** (brand-new sandbox): the supervisor clears `/sandbox/repo` and
the agent clones into that empty workdir.

**Reclaim** (live sandbox kept on the card): park resume and Needs You answer
share one reuse path. The supervisor does not wipe `/sandbox/repo`; it refreshes
an existing checkout in place, or ensures the directory without clearing caches
when no checkout is present. The agent clones only if the workdir still has no
repo.

That is why `clone_repo` is required when you create a Project: it lands in the
Project intent and the Initial plan, so the first agent does not invent a name.
Proposed Tasks should each name their clone target the same way.

Resolution at claim time is short:

```text
card.pull_request (base/head, once a PR exists)
  → else: clone from the card's prose
  → else: escalate
```

An unbound card escalates rather than guessing an `owner/name`. Once
`report.json` sets `pull_request`, that becomes the durable handle every later
claim uses for resume, rebase, and request-changes.

## Standing instructions and quality gates

sandboard stacks configuration in layers ([Configuration](configuration.md)):

1. **Process boot** — database URL, compile-time hierarchy.
2. **Board Settings** — Policies, sandbox specs, agent runtime (standing prompt), Forge/providers.
3. **Project fields** — `clone_repo`, optional sandbox override.
4. **`project_prompt`** — optional Project-only standing extras.
5. **Per-card intent / DoD** — clone target and card-specific work.

Boot, Settings, and Project fields are operator setup — they do not belong in
standing prompts. Cold briefings stack hardwired protocol, then the board
standing prompt (Settings → Agent runtime), then optional `project_prompt`,
then card prose.

Put board-wide rules in the standing prompt when you want them: escalation,
house style, and shared quality gates. Use `project_prompt` only when one
Project needs extras. Fresh boards leave standing prompt empty.

**Quality gates** — commands agents must run before publish — go in the board
standing prompt when they apply everywhere, or in `project_prompt` / card DoD
when narrower. Name the commands explicitly. sandboard does **not** assume `cargo`
unless those instructions name it.

## Triage order

1. **Needs You** — an agent is stopped and waiting. Resolve these first.
2. **Review** — finished work. Sort by size and risk, not arrival time.
3. Everything else waits for a digest (`board_snapshot` / `board_digest`).

If you are driving sandboard through a chat agent, interrupt the human for three
things only: irreversible actions, an ambiguity blocking several items, and
repeated failure on the same card. Otherwise summarise and let them walk away.

## Dispatch and auto mode

By default the operator decides what starts. A Backlog card is inert until
someone calls `dispatch` (**Start** in the UI), which sets `awaiting_dispatch`.

**Project auto mode** — the swimlane play/pause, or `set_auto_dispatch` — is the
exception. With it on, each supervisor tick queues every claimable Backlog leaf
under that Project. Pause clears `awaiting_dispatch` on cards still in Backlog
but does **not** halt Claimed or Running agents. Auto mode never approves a
Review, answers a Needs You, or unparks anything.

The supervisor takes the oldest claimable Backlog card with `awaiting_dispatch`
that is not already running, subject to concurrency and gateway health.

Lease expiry, park, halt, release, and request_changes all clear
`awaiting_dispatch`. With auto off, dispatch again; with auto on, the next tick
re-queues it. Unpark clears the hold and queues the supervisor, same as Start.

## Steering a card

| You want to | Do this |
|---|---|
| Send a reviewed card back with instructions | **Request changes** — the note reaches the next run's briefing. Does not auto-start; dispatch again. |
| Answer a blocked agent | **Needs You** — pick an option. |
| Stop a wedged run but keep its context | **Park** — stops the agent, keeps sandbox and conversation, holds until Resume. |
| Resume a parked card | **Unpark** — clears the hold; the next claim can resume the conversation. |
| Throw the run away | **Halt** — stops the agent, clears the conversation id, deletes the sandbox. Next dispatch starts clean. |
| Leave a note for later | **Steer** — stored, seen on the next claim. Does not inject mid-turn. |
| Auto-start claimable Backlog under a Project | Swimlane **Auto** play/pause. |

Prefer **park** over **halt** when the agent is stuck and you want the same
conversation to continue. Prefer **steer** when the note can wait.

## PR review feedback

When GitHub submits a PR review with state `CHANGES_REQUESTED` or `COMMENT`,
sandboard treats it like human **Request changes**: pointer steer note, clear any
proposal, move the matching card to Backlog. Same path for both review states —
no auto-dispatch.

The steer note only points at the PR (url / number). It tells the next agent
there is review feedback and to inspect it with `gh` (e.g. `gh pr view` /
reviews). It does not summarize or paste the review body; the agent figures out
the rest from the review itself.

Matching uses the card's `pr_url` the same way merge completion does. Applies
in Review, Needs You, and live Claimed/Running. Duplicate deliveries are safe
(already-Backlog cards are not matched again).

Ingress is the same webhook endpoint: `x-github-event=pull_request_review` with
`action=submitted`. Forge polling (when enabled) watches open PRs for newly
submitted reviews and calls the same Board helper — first observation only seeds
a per-PR cursor so historical reviews do not bounce the card.

GitHub `APPROVED` and dismissed reviews are board no-ops. Approving a PR on
GitHub does **not** Approve the card in sandboard, and it does not merge. Approve and
merge stay human.

## When main moves

Ingress is `POST /api/webhooks/github`. A push to the default branch emits
`MainAdvanced`, which does three things:

**1. Merged card → Done.** When a Review or Needs You card's `pr_url` matches
the merged PR, it completes. Webhook and polling both go through the same Board
completion helper.

**2. Review catch-up (scoped, CONFLICTING-only).** Main advancing under a Review PR
is a no-op unless GitHub reports a conflict. sandboard observes the host GitHub API
`mergeable` field with an App installation token — not a `git rebase` in a
sandbox — for open Review PRs on the **same upstream** that advanced (tip
advance and same-parent sibling merge share this path):

| `mergeable` | What happens |
|---|---|
| **MERGEABLE** | Silent no-op. Card stays in Review; no catch-up work signal. |
| **UNKNOWN** | GitHub is still computing. Retry on the next sweep. |
| **CONFLICTING** | Bounce to Backlog with a binding note so a worker can reclaim and rebase. |

Repeated overlapping conflict files escalate to **Needs You** (decomposition
failure) when those file lists are present.

**3. Live runs (repo-scoped steer + coalesced bounce).** Claimed and Running
cards on the **same upstream** that advanced get a binding steer note, a goal
story line, and a park+unpark so the next claim carries rebase instructions.
Steer alone does not inject mid-turn — the first interrupt on a Running card
still happens via park+unpark. Cards on other upstreams stay Running; unbound
live cards (no `pull_request` yet) steer only when the advanced repo matches
the board default `execution.agents.repo.upstream`.

| Situation | What happens |
|---|---|
| **Same upstream, first MainAdvanced while Running** | Steer note + goal story; park+unpark; card lands in Backlog with `awaiting_dispatch` (sandbox `environment` and `conversation_id` preserved). |
| **Same upstream, repeat MainAdvanced while already `awaiting_dispatch` from a prior steer** | Coalesce: no second park/unpark. Steer note and story refresh only when the commit sha changes. |
| **Different upstream** | No steer, no bounce — card stays Running. |

The goal story names the advanced ref/sha and that the live run was interrupted
for rebase (distinct from manual park). The Detail drawer and SSE upserts show
the steer note and story without a full page reload.

Webhook and poll responses list `steered_item_ids` for live-steered cards and
for Review cards bounced to Backlog on CONFLICTING (deduped). Live steering
does not replace Review catch-up: both fire on the same `MainAdvanced`, scoped
to the advancing `owner/name`.

### Local webhook forwarding

```bash
gh extension install cli/gh-webhook   # once

gh webhook forward \
  --repo=<owner/name> \
  --events=pull_request,pull_request_review,push \
  --url="$SANDBOARD_URL/api/webhooks/github"
```

`pull_request` and `push` cover merge → Done and main-advanced;
`pull_request_review` covers submitted review feedback → steer (see
[PR review feedback](#pr-review-feedback)). One forwarder per repo at a time.
For a polling fallback instead, see
[Configuration](configuration.md#openshell--forge--github-app-provider).

## Archive and Unarchive

**Archive** hides a Project (and its cards) from the active board. Nothing is
deleted — toggle **Show archived** to browse them.

**Unarchive** restores the Project so it rejoins the active board. Prior states
that were Claimed/Running (or other in-flight) come back as Backlog, not as live
work. Confirm-gated on the board and in the Detail drawer, same pattern as
Archive.

## Next

- [Troubleshooting](troubleshooting.md) — when a card stops moving
- [Cockpit](cockpit.md) — a durable terminal with operator reach
- [Configuration](configuration.md) — timeouts, concurrency, Policies, sandbox specs
