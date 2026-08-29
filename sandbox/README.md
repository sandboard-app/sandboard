# Sandbox assets

Inputs to `src/openshell.rs` and `src/supervisor.rs`. How a run works and what
breaks if you change them: [`docs/sandbox.md`](../docs/sandbox.md).

Card context is briefing-only (`/sandbox/.sandboard` contracts); see
[`docs/sandbox.md`](../docs/sandbox.md).

## Worker network policy (board Policies)

The **card-worker** allow-list is a named row in the board **Policies** catalog
(Settings → OpenShell → Policies, or `GET`/`POST` `/api/openshell/policies`).
A Sandbox spec references it by `policy_id`; that YAML is what OpenShell gets
at `sandbox create`.

Empty boards seed a minimal Policy from `src/seed_policies.rs`. After seed,
edit the **Policy on the board**, and keep the worker sandbox spec pointed at
it. Policy filesystem/process sections are immutable on a live sandbox; set
them at create time.

## Cockpit network policy (board Policies)

The **cockpit** sandbox uses the same catalog. `src/seed_policies.rs` seeds one
minimal Cockpit policy per engine (`cockpit-cursor`, `cockpit-agy`,
`cockpit-claude`, `cockpit-opencode`, `cockpit-hermes`) — inference/API egress for that engine
plus GitHub App `GH_TOKEN` and crates.io/npm registry egress (a card's own
`cargo build`/`npm ci` fetch live; there is no baked-in dependency cache) —
matched to a seeded Sandbox spec (`sandbox-cursor`, …) that already selects
it. Host sandboard MCP is stdio over a local Unix socket (`socat`, see
`src/cockpit_mcp_tunnel.rs`) — no network hop, so no policy entry for it. Edit
the live seeded Policy under Settings → OpenShell → Policies if these
defaults don't match your install — a re-seed on the next boot never
overwrites an edited row.

## `Containerfile`

Builds sandboard's own minimal base — Red Hat UBI9, not the OpenShell community
image — plus a Rust toolchain. Multi-stage: a `shared` stage installs OS
packages (`git`, `nodejs`/`npm`, `gh`, `gcc`/`make`, `iproute`, `nftables`,
`socat`) and `cargo`/`clippy`, then one leaf stage per agent engine (`cursor`,
`agy`, `claude`, `opencode`, `hermes`) installs only that engine's CLI on top, so each
resulting image only carries the binary it will actually run.

OpenShift restricted SCC assigns a random UID in group 0 and ignores image
`USER`. Installer tarballs that unpack as uid 2000 (Cursor) must be
`chown root:root` — `cp -a` would keep 2000, Landlock skips `/opt/cursor-agent`,
and `agent` is Permission denied. Writable paths are group-owned by 0 with
`g=u` so the random UID can write; the named `sandbox` user is **not** in
GID 0, because OpenShell's local podman driver refuses that membership.

The toolchain is baked in; sandboard's own source and dependency cache are not — a
card's own `cargo build`/`npm ci` populate `/opt/cargo`, `/opt/cargo-target`,
and `/opt/npm-cache` at runtime by fetching crates.io/npm live. Full rationale
(including why UBI9 over the community image) in
[`docs/sandbox.md`](../docs/sandbox.md#image).

Build from the **repo root**, not this directory:

```bash
podman build -f sandbox/Containerfile --target cursor -t quay.io/sandboard-app/sandbox-cursor:latest .
# make sandbox builds all five; make sandbox-push also pushes them.
```

Rebuild when you need a newer engine CLI, OS package, or Rust toolchain
version. Matching `/opt` entries belong in the worker **board Policy** (and
the embedded seed in `src/seed_policies.rs`): `/opt/cargo`,
`/opt/cargo-target`, `/opt/npm-cache` need **read-write** (populated live, not
pre-baked); `/opt/rust` (plus that engine's own `/opt/cursor-agent`,
`/opt/opencode`, or `/opt/hermes`) stays **read-only**. The Hermes leaf also
bakes Python 3.12 and its virtualenv under `/opt/hermes`; `/sandbox/.hermes`
is the writable runtime state. `/opt/cargo/bin/cargo` is rustup's proxy
binary and re-execs the real `cargo` under `/opt/rust/toolchains/<version>/bin`
at runtime — a process exec, not a symlink OpenShell's literal binary matching
can follow, so the toolchain path needs its own policy entry too (verified
live: omitting it gets a 403 on crates.io even with the proxy path allowed).

Claude / OpenCode auth goes through OpenShell `inference.local`. Direct
OpenRouter clients use an attached endpoint-bearing OpenRouter provider, which
supplies `OPENROUTER_API_KEY` only to that sandbox; no key is baked into the image.
