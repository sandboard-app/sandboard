<p align="center">
  <img src="assets/sandboard-logo.png" alt="sandboard" width="200" />
</p>

<h1 align="center">sandboard</h1>

<p align="center">
  <strong>A board for agent work.</strong><br />
  Point it at a repository, dispatch sandboxed coding agents, merge the PRs yourself.
</p>

<p align="center">
  <a href="https://sandboard-app.github.io/"><img alt="Docs" src="https://img.shields.io/badge/docs-sandboard--app.github.io-3d7ea6?style=flat-square" /></a>
  <a href="Cargo.toml"><img alt="Rust" src="https://img.shields.io/badge/rust-stable-dea584?style=flat-square&logo=rust&logoColor=white" /></a>
  <a href="#status"><img alt="Status" src="https://img.shields.io/badge/status-active-2ea44f?style=flat-square" /></a>
</p>

---

**sandboard** is a board you point at a repository. You describe what you want; it
runs coding agents in sandboxes; pull requests come back for you to merge.

Moving a card starts an agent. Answering a question unblocks one. Approving a
plan creates the tasks. The UI and MCP share one board lifecycle in
`src/store.rs`.

<p align="center">
  <img src="https://sandboard-app.github.io/images/desktop-board.png" alt="The sandboard board: Backlog, Running, Needs You, Review, Done" width="900" />
</p>

**📖 [Read the docs →](https://sandboard-app.github.io/)** — start with the
[Tour](https://sandboard-app.github.io/tour.html), which walks one card's life with
screenshots and needs nothing installed.

## What makes it different

- **One board lifecycle.** UI and MCP call the same state machine. There is no
  separate “apply” step.
- **Workers cannot reach sandboard.** Card agents run in OpenShell sandboxes with no
  network path back to the board. The supervisor claims, heartbeats, and reports
  for them — so a worker cannot approve its own review.
- **You merge on GitHub.** Approving in sandboard surfaces the PR. sandboard does not
  merge.
- **Liveness comes from real output.** The supervisor watches the agent’s
  stream. There is no keepalive timer that pretends a wedged agent is alive.

**sandboard builds sandboard.** Cards against this repository open same-repo PRs that you
review like any other contribution.

```
you ──chat──> operator agent (Cursor / Claude Code)
                    │ MCP (streamable HTTP, /mcp)
                    ▼
            ┌────────────────────┐
            │  sandboard (Rust/axum)  │◀── REST + SSE ── React UI
            │  board state machine│
            └────────────────────┘
                    ▲
              supervisor ──> worker agent in an OpenShell sandbox
                                 └─> same-repo PR ──> you merge
```

## Quick start

**Requirements:** a current Rust stable toolchain, and a recent Node.js if you
want to build the UI.

```bash
git clone https://github.com/sandboard-app/sandboard.git
cd sandboard
cargo run                 # API + SSE + MCP + UI (default bind :8080)
```

The board asks you to create an admin on first open, then starts empty. Nothing
runs until you set up OpenShell and dispatch a card — so you can explore the UI
without Docker or credentials.

Full walkthrough: **[Quickstart](https://sandboard-app.github.io/quickstart.html)**.
First sandboxed run:
**[Your first agent](https://sandboard-app.github.io/first-agent.html)**.

```bash
make run                  # debug API + web/dist, then serve
make dev                  # watchexec rebuild/restart on Rust changes
make dev-ui               # Vite on :5173 (proxies to :8080)
make docs-serve           # this book, at :3000
```

## Repository layout

| Path | Role |
|---|---|
| `src/store.rs` | The board — the only write path |
| `src/machine.rs` | Legal transitions and lifecycle invariants |
| `src/supervisor.rs` | Dispatch, sandbox lifecycle, lease sweeping |
| `src/mcp.rs` | Operator MCP tools; supervisor keeps worker verbs |
| `src/openshell.rs` | In-process gRPC client to the OpenShell gateway |
| `sandbox/` | Container image, network policy |
| `web/` | React UI + screenshot harness |
| `docs/` | The mdBook published to sandboard-app.github.io |

## Status

sandboard is under active development and already used to ship changes to itself.
Expect sharp edges: the happy path (board → plan → sandboxed agent → PR) works;
independent verification gates are still ahead.

The properties we will not break while the surface grows are in
**[Invariants](https://sandboard-app.github.io/invariants.html)**.

## Contributing

The preferred path is the product itself: open or join a Project on a running
board, let a worker open a PR, review and merge on GitHub.

By hand:

1. Keep mutations on the board path — do not encode lifecycle rules in
   transports (`api.rs` / `mcp.rs`).
2. `cargo test` and `cargo clippy --all-targets -- -D warnings` must be clean.
3. Stage specific paths. `git add -A` has committed unintended local state here
   before.

Agent orientation for work *on* this repo: [`AGENTS.md`](AGENTS.md)
(`CLAUDE.md` is a symlink) and
[`.cursor/rules/sandboard-operator.mdc`](.cursor/rules/sandboard-operator.mdc).

## License

License terms are not yet published in this repository. Treat the code as
source-available until a `LICENSE` file lands; ask before redistributing.
