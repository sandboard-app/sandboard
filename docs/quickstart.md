# Quickstart

Get the board running on your machine in a few minutes, without OpenShell or
credentials. Nothing dispatches until you set those up and start a card.

When you are ready for sandboxed runs, use the empty-board **Welcome** guide
(or **Help**) — OpenShell + sandbox before the Project loop — then the checklist
on [Your first agent](first-agent.md).

**You need:** a current Rust stable toolchain, and a recent Node.js if you want
to build the UI.

## 1. Run it

```bash
git clone https://github.com/sandboard-app/sandboard.git
cd sandboard
cargo run
```

That serves the API, SSE, MCP, and the built UI. Bind port defaults to `8080`
(`SANDBOARD_PORT` overrides). Open the board at whatever Host you use — UI copy and
OAuth redirect URIs come from that origin, not a hardcoded loopback URL.

If `web/dist` does not exist yet, build the UI once:

```bash
npm --prefix web install && npm --prefix web run build
```

## 2. Create your admin

The first time you open the board it asks you to create an admin account. Until
you do, the API refuses everything — there is no anonymous mode.

Pick any username and password; it is stored locally, in your board database.

## 3. Make something

The board starts empty. Create a **Project**, give it an intent, and point it at
a repository (`owner/name` — the repo the planning agent will clone).

sandboard creates an **Initial plan** Task under it automatically. You now have a
Project, a Task, and a board that looks like the one in the [Tour](tour.md) —
minus anything running, because nothing has been dispatched yet.

Click into the card. The detail drawer shows **why this exists** (the chain up
to its Project), its definition of done, and the Proposed Tasks section that an
agent would fill in.

Move it around. Nothing will claim it, nothing will spend money, and you cannot
break anything that a restart does not fix.

Once the Project has cards, **Create Task** on the swimlane or Detail drawer
(or MCP `create_task`) adds another Backlog card under that Project without
re-running Initial plan. Name the clone target in intent/DoD. See
[Workflow](workflow.md#adding-tasks-to-an-existing-project).

## 4. Connect a chat client (optional)

You can drive the board from Cursor or Claude Code over MCP instead of the UI.
sandboard must already be listening.

For an agent bringing up a **fresh** board (admin, OpenShell, providers, sandbox
spec, first Project), point it at the public bootstrap guide first:

```bash
curl -sS "$SANDBOARD_URL/llms.txt"   # SANDBOARD_URL = the origin you open the board on
```

`GET /llms.txt` needs no auth. Source lives at [`llms.txt`](../llms.txt) in the
repo; Vite’s `:5173` proxy forwards the same path in `make dev-ui`.

`/mcp` is for operators: create Projects, triage, dispatch, park, steer,
approve. Worker verbs (`claim`, `heartbeat`, `report`, …) stay with the
supervisor. The Help / Board empty guide shows `{origin}/mcp` from
`window.location`.

**Cursor** — point `.cursor/mcp.json` at your board origin:

```json
{
  "mcpServers": {
    "sandboard": {
      "type": "http",
      "url": "http://YOUR_HOST:PORT/mcp",
      "auth": { "CLIENT_ID": "sandboard-cursor", "scopes": ["mcp"] }
    }
  }
}
```

```bash
agent mcp login sandboard
```

**Claude Code:**

```bash
claude mcp add --transport http sandboard "$SANDBOARD_URL/mcp"
```

Either way a browser opens for login and consent, using the same account you
just created. Tokens survive a sandboard restart, so you will not be logging in
repeatedly.

If the tools list stays empty, reload the client.

## Developing on it

```bash
make dev                  # watchexec rebuilds and restarts on Rust changes
make dev-ui               # Vite on :5173, proxying to :8080
```

`make dev` needs [`watchexec`](https://crates.io/crates/watchexec-cli)
(`brew install watchexec` or `cargo install watchexec-cli`).

## Next

- **[Your first agent](first-agent.md)** — Welcome/Help OpenShell onboarding, then one sandboxed run
- [Workflow](workflow.md) — the day-to-day loop
- [Configuration](configuration.md) — database URL, environment, Settings
