# Configuration

sandboard stacks configuration in layers. Lower layers are operator concerns; upper
layers are what agents read at claim time. See also
[Workflow](workflow.md#standing-instructions-and-quality-gates) and
[Concepts](concepts.md#configuration-layers).

| Layer | Who sets it | Role |
|---|---|---|
| **Process boot** | Host / deploy | Database URL (`SANDBOARD_DATABASE_URL` else `sqlite:sandboard.db`). Hierarchy is compile-time Project + Task. |
| **Board Settings** | Operator | Policies, sandbox specs, Agent runtime (engine, concurrency, timeouts, sweep interval, **standing prompt**), OpenShell gateway/providers (incl. shipped `github-app`), Forge, and GitHub App repo access. |
| **Project fields** | Operator | Default clone repo (`clone_repo`), optional sandbox spec override (`sandbox_profile_id`). Seeded into Project intent and the Initial plan. |
| **`project_prompt`** | Operator | Optional Project-only standing extras. Board-wide policy lives in Agent runtime standing prompt. |
| **Per-card intent / DoD** | Operator (per Task) | Card-specific work: clone target (`owner/name`), card-local gates, and the operational proof. Notes can override at claim time. |

**Boot, Settings, and Project fields are operator concerns — not agent essay text.**
Do not put database URLs, Policy YAML, or sandbox spec ids in standing prompts.
Cold briefings stack hardwired protocol, then the board standing prompt, then
optional `project_prompt`, then card intent/DoD.

**Quality gates** — test/lint commands agents should run before publish — belong
in the board standing prompt when board-wide, or in `project_prompt` /
card DoD when narrower. Name the commands explicitly (`cargo test`, `npm test`,
…). sandboard does **not** assume `cargo` or any other toolchain unless those
instructions name it.

## Board database

Board rows live in a SQLx store. **SQLite is the default**; Postgres is
optional, for a shared server.

| Source | Example |
|---|---|
| Compiled default | `sqlite:sandboard.db` |
| Environment override | `SANDBOARD_DATABASE_URL=postgres://sandboard:sandboard@127.0.0.1:5432/sandboard` |

Accepted forms:

- SQLite — `sqlite:sandboard.db`, `sqlite://…`, `sqlite::memory:` (tests)
- Postgres — `postgres://…` or `postgresql://…`

On boot sandboard opens the URL, applies versioned migrations from `migrations/`, and
restores the board from rows.

The database URL cannot live in board Settings — Settings persist *inside* the
database.

**One-shot JSON import:** if the database is empty and `sandboard.json` exists in the
working directory, sandboard imports it once and leaves the JSON alone — archive or
delete it yourself. Later boots use the database only.

Offline `cargo test` always uses SQLite. To exercise Postgres migrations
locally, point `SANDBOARD_TEST_DATABASE_URL` at a reachable Postgres URL.

## Environment

| Variable | Effect |
|---|---|
| `SANDBOARD_PORT` | Listen port (default 8080) |
| `SANDBOARD_BIND_ADDR` | Bind host (default `127.0.0.1`; containers use `0.0.0.0`) |
| `SANDBOARD_DATABASE_URL` | Board database URL (default `sqlite:sandboard.db`) |
| `SANDBOARD_TEST_DATABASE_URL` | Postgres URL for migration tests |

Cockpit's shipped `sandboard` MCP entry is stdio over a local Unix socket
(`socat`, see [Cockpit](cockpit.md#how-the-mcp-relay-works)) — no URL, no env var.

One host secret file: `~/.config/sandboard/master.key`, which seals credentials
stored on the board.

## Hierarchy

Project + Task is fixed in code (`schema::default_levels`). There is no
install-time level ladder to configure.

## Project fields and `project_prompt`

When you create a Project (board UI, REST `POST /api/items`, or MCP
`create_project`):

| Field | Stored on | Purpose |
|---|---|---|
| `clone_repo` | Project intent | Default `owner/name` for the Initial plan and for Tasks that omit an explicit clone line. Required on create. |
| `sandbox_profile_id` | Project row | Optional override of the board default sandbox spec. Unset means inherit Settings. |
| `project_prompt` | Project row | Optional Project-only standing extras. Empty unless the operator supplies one. |

`project_prompt` is **not** a substitute for Settings or Project fields. Keep
boot-time config, OpenShell Policies, sandbox specs, and `clone_repo` where
they belong. Board-wide standing policy is **Settings → Agent runtime →
standing prompt**. Use `project_prompt` only for rules that apply inside one
Project.

Per-card **intent** and **definition of done** carry the card's clone target
and any gates that apply to that card only. The supervisor never invents gates;
it points agents at the board / Project standing text and the card DoD.

## Agent runtime

**Settings → Agent runtime** (REST: `/api/agent-runtime`): default engine,
concurrency, agent timeout, max attempts, sweep interval, and **standing
prompt** (optional board-wide agent policy; empty by default). Card branches /
sandboxes use a fixed `sandboard` stem (`sandboard/card-*`, `sandboard-cockpit`) — not a
Settings knob. OpenShell gateway + a sandbox spec are the practical readiness
gates before dispatch does anything useful.

## Policies

A **Policy** is a named OpenShell YAML allow-list (filesystem / network). The
catalog lives on the board and is edited in **Settings → OpenShell → Policies**
(REST: `/api/openshell/policies`). Empty boards seed a minimal row from
`src/seed_policies.rs`; operators add egress there as needed.

Live policy always comes from this board catalog. At sandbox create the
supervisor resolves the selected policy to YAML for OpenShell. Policy is
**fixed for that sandbox's life** for filesystem and process sections —
recreate the sandbox after a change.

## Sandbox specs

A sandbox spec is the recipe for a sandbox: image, CPU, memory, engine,
optional **model**, optional **env** / **prompt**, attached providers, and a
**reference to a named Policy** (`policy_id`). Specs live on the board and are
edited in **Settings → OpenShell → Sandbox specs** (REST:
`/api/sandbox-profiles`). Upsert requires a known `policy_id`; you edit
allow-list YAML under Policies, not on the spec.

Four specs come seeded — `sandbox-cursor`, `sandbox-agy`, `sandbox-claude`,
`sandbox-opencode` — one per split `quay.io/sandboard-app/sandbox-<engine>` image
([Sandbox](sandbox.md#image-and-offline-gates)), each already wired to a
matching minimal Cockpit policy with sandboard MCP attached. Editing a seeded row
sticks; the seed only inserts what's missing.

### Model

An optional **model** on the spec names the model sandboard passes to agent CLIs
that accept a `--model` flag on launch (`agy`, `cursor` / `agent`). Leave it
unset to inherit the engine default — for `agy`, `gemini-3.6-flash-high`
(`DEFAULT_SEAT_MODEL`); for `cursor`, the account default for your API key.

Resolution at claim/run:

1. **`card.model`** on the Task (if set) — per-card override on claim
2. **Sandbox spec `model`** — the winning profile for that card
3. **Engine default** — compiled fallback when both are unset (`agy` only; `cursor` uses the account default)

The board and card UI show the resolved value (`resolved_model`). Cockpit uses
the same spec → default chain (no card).

**`claude` and `opencode` do not read spec model.** Those engines reach models
through OpenShell's `inference.local` router; which model they get is whatever
you configured on the gateway with `openshell inference set` (see
[Sandbox](sandbox.md#how-credentials-reach-the-agent)). sandboard does not automate
that route — set it once per gateway as today.

### Environment (`env`)

Optional string map on the spec. At sandbox create (card and Cockpit), sandboard
builds env as `agent_env(engine)` then overlays the resolved profile's `env` —
**profile wins on key clash**. Spec `env` is **non-secret by contract**: put
API URLs, tool paths, and similar seat wiring here; put credentials on
**Providers** (attached on the same spec). The Settings editor shows that hint
next to the key/value fields. See [Sandbox](sandbox.md#spec-env-and-prompt).

### Prompt (`prompt`)

Optional seat notes on the spec. At claim, `ClaimGrant` carries
`sandbox_prompt` from the resolved profile. Cold card briefing inserts a
**Sandbox prompt (seat notes):** section after **Project prompt** when
non-empty; Cockpit seed briefing includes the cockpit profile's prompt the same
way. Resume briefing (conversation memory already has it) does **not** re-dump
the sandbox prompt. See [Sandbox](sandbox.md#spec-env-and-prompt).

**Operator practice:** put an API base URL in `env` and short usage notes in
`prompt` (for example how to call a cluster API from the seat). Do **not**
hardcode product-specific CLIs or cluster wiring into the binary or supervisor
— keep that on the live board's sandbox specs.

### Which spec a card gets

Resolution order is documented in [Sandbox](sandbox.md). Create-form defaults
select the seeded minimal policy; attach providers and pick the policy the run
needs.

### Cockpit

Cockpit uses the global default sandbox spec unless you set an explicit Cockpit
profile under Sandbox specs. A fresh board seeds all four specs but picks none
of them as default — that choice is an onboarding step (Welcome flags it red
until you set one). Pick a seeded spec (or one you made) and click **Set
default**, or **Use for Cockpit** to give Cockpit its own engine. That spec's
`policy_id` is what the sandbox gets at create. The cockpit profile's `env` and
`prompt` apply the same create-time overlay and seed-briefing rules as card
specs.

## OpenShell / Forge / GitHub App provider

Connectivity, providers (including the shipped `github-app` type that mints
`GH_TOKEN`), provider types, Policies, Sandbox specs, Forge poll, and **Repo
access** are board Settings — see the Settings UI and [Your first agent](first-agent.md).

**Repo access** walks every GitHub App installation and caches `owner/repo` →
installation id, permissions, and last-seen time. Refresh from Settings or wait
for the background job. Use the GitHub install link to add missing
repositories. Token minting is unchanged: the `github-app` provider still uses
the configured `GITHUB_INSTALLATION_ID`.
