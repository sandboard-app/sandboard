# Your first agent

The shortest path from an empty board to one real sandboxed run that opens a
pull request.

> **This spends real money and opens pull requests.** Do this once on a repo
> you do not mind receiving a small PR against.

## Start in the product

On an empty board, **Welcome to sandboard** embeds the same operator guide as
**Help** (nav → Help). That guide is the named first-run path:

1. **Connect MCP**
2. **OpenShell + sandbox** — Connectivity, Providers, Policies, Sandbox specs
3. **First Project loop**

Work the checklist there; deep links land on Settings → OpenShell and Agent
runtime. This page is the prose companion: the same order, with checks and the
host-side pieces the UI does not run for you. An operator agent should start from
the public [`/llms.txt`](../llms.txt) guide (same order, API-shaped checks).

Every step has a check. Do not move on until the check passes — see
[why](troubleshooting.md#everything-fails-as-a-hang) below: in this stack a
half-finished step does not error, it hangs.

## What you are assembling

Four things have to be true on the host. The tools named are examples, not the
only stack.

| # | Role | Concretely |
|---|---|---|
| 1 | Something that runs containers | podman, Colima, or Docker |
| 2 | The OpenShell gateway | holds sandboxes, network policy, credentials |
| 3 | Model + GitHub credentials | as OpenShell *providers*, never baked into an image |
| 4 | A sandbox image | with whatever toolchain the work needs |

sandboard itself holds none of those credentials. It talks to the gateway over gRPC
and the gateway injects secrets on egress, so nothing sensitive enters the
sandbox.

## 1. A compute driver

OpenShell's gateway needs a working Docker-compatible API. How you provide it is
your choice:

| Driver | Typical setup |
|---|---|
| **podman** | `podman machine start` |
| **Colima** | `colima start`, then point the gateway at `unix://$HOME/.colima/default/docker.sock` |
| **Docker Desktop / engine** | Make sure the daemon is up and the gateway can reach its socket |

`DOCKER_HOST` and friends belong to the **gateway process**, not to sandboard
Settings.

**Check:**

```bash
docker info        # must succeed
```

The driver can stop on its own — the podman machine especially. sandboard classifies
that as infrastructure rather than the card failing, so it will not burn a
card's retry budget, but it cannot prevent the outage.

## 2. The OpenShell gateway

Start it however your install expects (Homebrew service, systemd, …). sandboard does
not spawn an `openshell` CLI for board traffic: `src/openshell.rs` talks to the
gateway in-process over gRPC with client certificates.

**Check:**

```bash
openshell status   # expect Connected + Authenticated
```

Then tell sandboard how to reach it, in **Settings → OpenShell → Connectivity**
(Welcome/Help deep-links here):

- **Gateway endpoint** — often `https://127.0.0.1:17670` (not sandboard's `8080`;
  your install may differ).
- **mTLS PEMs** — CA, client cert, client key. Paste them in. They are stored
  encrypted in the board database (`~/.config/sandboard/master.key`). The API does
  not return private keys. sandboard does not read them from disk — upload them in
  Settings.

**Settings** (stored on the board) is the live source of truth for gateway
endpoint and sealed PEMs — same split as [Configuration](configuration.md).

**Check:** hit **Refresh status** in Settings. You want **Healthy**.

## 3. Providers

**Settings → OpenShell → Providers** is the credential list on the board.
**Sync** applies it to the gateway. Which providers attach on create is chosen
per Sandbox spec.

For **`claude`** and **`opencode`** sandboxes, point OpenShell's local router at
your model (this is separate from the optional **Model** field on Sandbox
specs, which only applies to CLIs that accept `--model` — `agy` and `cursor`):

```bash
openshell provider create --name vertex --type google-vertex-ai --from-gcloud-adc \
  --config VERTEX_AI_PROJECT_ID=<project> --config VERTEX_AI_REGION=global
openshell inference set --provider vertex --model claude-sonnet-4-6@default
```

Those agents then reach models at `https://inference.local` and the gateway
swaps in the real credential on the way out. Details, including the one
environment variable that will silently break this:
[Sandbox](sandbox.md#how-credentials-reach-the-agent).

For **`agy`** and **`cursor`**, model selection is on the Sandbox spec (or per
card at claim); sandboard passes the resolved value as `agy --model …` or
`agent --model …`. See
[Configuration](configuration.md#model) and [Sandbox](sandbox.md#model-selection).

For GitHub, add or edit the shipped **`github-app`** provider under
**Settings → OpenShell → Providers** (type profile under **Provider types**,
same catalog as `cursor-agent` / `antigravity`). Set App ID, private key, and
installation; Save/Sync mints `GH_TOKEN` onto the gateway — attach that
provider on your Sandbox spec.

**Check:** Sync reports success and the providers you expect are listed on the
gateway.

## 4. Policies and a sandbox image

OpenShell allow-lists are named **Policies** on the board
(**Settings → OpenShell → Policies**). A Sandbox spec picks one by id — image,
resources, engine, and providers live on the spec; YAML lives on the Policy.
[Configuration](configuration.md#policies) covers the split; a running sandbox
keeps the policy it was created with.

A fresh board already has four seeded specs — `sandbox-cursor`,
`sandbox-agy`, `sandbox-claude`, `sandbox-opencode` — each pointed at
`quay.io/sandboard-app/sandbox-<engine>:latest` and a matching minimal Cockpit
policy. None of them is the default yet — pick one (Welcome flags this until
you do). Build and push those images yourself (or point the seeded specs at
wherever you host them):

```bash
make sandbox        # builds all four quay.io/sandboard-app/sandbox-<engine>:latest
make sandbox-push   # builds, then pushes all four
# Docker: CONTAINER_ENGINE=docker make sandbox
# Different registry: REGISTRY=ghcr.io/you make sandbox
```

From the **repo root**, not `sandbox/` — the Containerfile is multi-stage and
`podman build -f sandbox/Containerfile` resolves relative to wherever you run
it. Each image bakes a Rust toolchain but no sandboard source or dependency cache;
a card's own `cargo build`/`npm ci` fetch crates.io/npm live, so the seeded
Cockpit policies allow that egress (`src/seed_policies.rs`).

Then set the board's **default** sandbox spec in
**Settings → OpenShell → Sandbox specs** (Welcome/Help deep-links here) —
either one of the four seeded rows or one you made. Nothing is default until
you choose; the Welcome "Sandbox spec" readiness check stays red until then.
Optionally set **Model** on the spec when using `agy` (or override per card at
claim); `claude`/`opencode` model routing stays on the gateway via
`openshell inference set` as above. Specs live on the board;
[Configuration](configuration.md#sandbox-specs) and [Sandbox](sandbox.md) cover
resolution.

**Check:** `podman image ls | grep sandbox-`, and Welcome's "Sandbox spec"
readiness check turns green.

## 5. Agent runtime (optional tune)

Tune concurrency / timeouts / sweep under **Settings → Agent runtime** if you
want ([Configuration](configuration.md)).

**Check:** OpenShell readiness on Welcome shows Gateway/mTLS and Sandbox spec
ready.

## 6. Run one card

This is the **First Project loop** section of Welcome/Help:

1. Create a Project pointed at your repo (`clone_repo` as `owner/name`).
2. **Start** its Initial plan card.
3. Watch it move Backlog → Running. The card shows its sandbox name.
4. It lands in **Review** with a proposed breakdown. Read it, edit it, Approve.
5. **Start** one of the resulting Tasks.
6. It opens a pull request and lands in Review.
7. Merge on GitHub. The card moves to Done.

Keep `max_concurrent` at 1 until you have watched this work end to end.

If something takes longer than it should, it has already failed —
[Troubleshooting](troubleshooting.md#everything-fails-as-a-hang) is the next
page you want. Denied egress, missing credentials, and wedged relays present as
silence; treat hangs as failure, not as "give it more time."

## Next

- [Workflow](workflow.md) — steering cards day to day
- [Configuration](configuration.md) — Policies, sandbox specs, engines, timeouts
- [Sandbox](sandbox.md) — what actually happens inside a run
- [Cockpit](cockpit.md) — a durable terminal with operator reach
