# Architecture

One page on how the pieces fit. Present tense; code paths, not history.

## One state machine

Every mutation — UI, MCP, supervisor — goes through `Board` in `src/store.rs`.
Legal transitions and lifecycle invariants live in `src/machine.rs`. Transports
(`api.rs`, `mcp.rs`, SSE) render and invoke; they do not own rules.

```
UI / MCP / supervisor
         │
         ▼
      Board (store.rs) ── persistence (SQLx) ── event bus (SSE)
         │
         ├── machine.rs   legal transitions
         └── model.rs     Project + Task node type
```

## Layout

| Path | What |
|---|---|
| `src/model.rs` | One node type: Project (container) + Task (claimable leaf). Cockpit session singleton. |
| `src/machine.rs` | Legal transitions and lifecycle invariants, for cards and the cockpit session. |
| `src/store.rs` | The board: state, persistence, event bus, derived reads. |
| `src/api.rs` `src/sse.rs` `src/cockpit_chat.rs` | The human face — REST, board SSE, cockpit chat bridge. |
| `src/mcp.rs` | Operator MCP tools; the supervisor keeps worker verbs. |
| `src/openshell.rs` | In-process gRPC client to the OpenShell gateway (board endpoint + sealed mTLS); every call has a deadline. |
| `src/supervisor.rs` | Card dispatch, durable cockpit start/reconcile/stop, briefing, lease sweeping. |
| `src/engine.rs` | Explicit registry of agent engines — unknown ids fail loud. |
| Process boot | Database URL via `SANDBOARD_DATABASE_URL` (else `sqlite:sandboard.db`). Hierarchy is compile-time Project + Task. |
| `sandbox/` | Container image; minimal create-form Policy seed lives in `src/seed_policies.rs` (board Policies catalog is live). |
| `web/` | React UI + Playwright screenshot harness. |
| `migrations/` | Versioned SQLx migrations for the board store. |

## Sandboard's supervisor

The supervisor is an internal part of Sandboard, not a separate service. It is the
execution loop that connects Board state to the OpenShell gateway and keeps a run
reconciled with the card that started it.

The supervisor:

1. Health-checks the OpenShell gateway.
2. Auto-enqueues claimable Backlog leaves under Projects with auto mode on.
3. Claims the oldest `awaiting_dispatch` card within concurrency limits.
4. Creates or reuses a sandbox, builds a briefing from the Project→Task chain,
   and starts the agent detached.
5. Parses the output stream for liveness; calls `heartbeat` / `report` on the
   board's behalf.
6. Sweeps expired leases, and on startup reconciles live sandboxes so a sandboard
   restart does not orphan a running agent.

Separately, when a Board **cockpit session** exists, the supervisor creates or
reuses the cockpit-spec sandbox (`sandboard.cockpit` label), starts the agent
detached, reconciles across restart (keeping sandbox and conversation, like
park), and stops cleanly when the session is cleared. That path never touches
claim / heartbeat / report / split or the card dispatch queue — the Board's
`cockpit_session` fields stay authoritative.

The card worker has no network path to sandboard. The supervisor is the only caller
of worker verbs on the live path.

## MCP and REST

| Face | Transport | Audience |
|---|---|---|
| Operator MCP (operator tools only) | MCP streamable HTTP at `/mcp` | Chat and cockpit agents (OAuth) |
| Host operator (operator + worker verbs) | `Operator::host`, in-process | Supervisor/host tooling and tests |
| Human UI | REST + board SSE | React app; one-tap answers and approvals |
| Agent bootstrap guide | `GET /llms.txt` (no auth) | Operator agents on a fresh board |
| Cockpit terminal | `GET`/WS `/api/cockpit-attach` | xterm → `ExecSandboxInteractive` |
| Cockpit chat bridge (legacy) | `POST /api/cockpit-chat` (SSE) | Detached-agent stream-json bridge |

`/mcp` does not expose worker verbs (`claim`, `heartbeat`, `report`, `split`,
`escalate`, `release`, `list_ready`). Operator clients triage and dispatch; they
do not run the card lifecycle.

Steer, pin, park, halt, and cut scope all want a reason, so they live in MCP.
What stays one-tap in the UI is answering an escalation and approving a review.

The MCP surface is stateless on purpose: tools are request/response over
`SharedBoard`. An in-memory session id only made clients brittle across restarts
without buying server→client streams.

## How the CLI attaches

There is no `ConnectSandbox` RPC. `openshell sandbox connect` (a human at a
terminal, not sandboard) is a chain:

1. `GetSandbox(name)` → `sandbox_id`
2. `CreateSshSession(sandbox_id)` → short-lived token plus gateway host/port
3. local `ssh -tt sandbox` with `ProxyCommand=openshell ssh-proxy … --token …`
4. `ssh-proxy` tunnels via `ForwardTcp`

sandboard itself never runs this chain — no local `ssh`, no `CreateSshSession`. A
browser cannot complete the OpenSSH ProxyCommand chain anyway, so both the
in-browser terminal and cockpit's MCP session use `ExecSandboxInteractive`
directly: the terminal relays it over a WebSocket (see
[Cockpit](cockpit.md#how-the-browser-terminal-works)), and cockpit MCP wraps
its stdin/stdout as an `rmcp` transport instead of tunneling TCP (see
[Cockpit](cockpit.md)).

## Persistence

SQLx board store — SQLite by default, Postgres optional. Configured by
`board.database.url` or `SANDBOARD_DATABASE_URL`. Mutations flush as row updates,
with an optional one-shot import from `sandboard.json` when the database is empty.
See [Configuration](configuration.md#board-database).

## Related

- [Concepts](concepts.md) — the product model
- [Invariants](invariants.md) — what will not change, and why
- [Sandbox](sandbox.md) — what a run looks like from inside
