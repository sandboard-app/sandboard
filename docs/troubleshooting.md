# Troubleshooting

## Everything fails as a hang

**This is the single most useful thing to know about running sandboard.**

A denied egress, a missing credential, a wedged relay, a stopped compute driver
— none of them produce an error. They produce silence. Nothing in the sandbox
stack has a reliable failure path that surfaces as a failure.

So: **if something is taking longer than it should, it has already failed.**
Do not wait it out. Go look.

That observation shapes the code as much as the operations. Every exec sandboard
issues carries a deadline, and a deadline expiring is treated as failure rather
than as "maybe a bit more time."

## Where to look first

```bash
openshell logs <sandbox> -n 60     # grep for DENIED, ALLOWED, ssrf, HTTP:
openshell sandbox list             # phases; Deleting still shows up here
journalctl -u sandboard | grep 'openshell exec failed'   # board-side ExecSandbox drops
```

Failed `ExecSandbox` / interactive setup paths in `src/openshell.rs` emit a
structured `openshell exec failed` line with `gateway_endpoint`, `sandbox_name`,
`sandbox_id`, `elapsed_ms`, and `request_id` (client-generated `x-request-id`,
overwritten by the gateway echo when response headers arrive). Use that
`request_id` to align board logs with gateway journalctl around the same h2
stream.

The card carries its sandbox name, so you can go from a stuck card to its logs
directly.

A **failed card keeps its sandbox** rather than deleting it. `openshell logs` is
the tool that answers questions and a deleted sandbox answers none. Sandbox
names are attempt-scoped (`sandboard-card-8-a2`), so a retry never collides with the
one being kept for inspection, and `reconcile` clears them at next startup.

## Common causes

### The compute driver stopped

The podman machine stops on its own. So does Colima, occasionally.

```bash
docker info      # if this fails, nothing below matters
```

sandboard classifies this as infrastructure, not as the card failing: it
health-checks before claiming and pauses after an infrastructure failure rather
than spending a card's retry budget on an outage it cannot fix.

### Egress was denied

The network policy is a literal allow-list, and binary paths in it are matched
literally too. Git's real remote helper is `/usr/lib/git-core/git-remote-http`,
not `git`.

Grep the sandbox log for `DENIED`. Allow-list YAML lives in the **board
Policies** catalog (Settings → OpenShell → Policies, or
`/api/openshell/policies`). Sandbox specs only reference a policy by id. Edit
the Policy, then create a new sandbox so the updated YAML is applied at create
time.

### Policy edits are not taking effect

Two separate traps:

- **Policy is immutable on a live sandbox** for the filesystem and process
  sections. Live policy comes from the board and is set at create time —
  recreate the sandbox after a policy change.
- **Board Policies are authoritative.** Create-form defaults select the seeded
  minimal policy (`src/seed_policies.rs`); edit egress under Settings →
  OpenShell → Policies (and keep the spec's `policy_id` pointed at the row you
  mean).

### The model calls hang

Do not set `CLAUDE_CODE_USE_VERTEX=1` in a sandbox. It forces direct Vertex with
ADC/metadata discovery, which OpenShell blocks — real GCE metadata is
SSRF-hardened. Use `inference.local` instead; see [Sandbox](sandbox.md).

Also check the `/v1` suffix: `claude` wants `https://inference.local` and
appends its own path, while `opencode` wants
`https://inference.local/v1`. Getting this wrong hangs rather than 404s.

### An environment variable the agent needs is missing

**The image's `ENV` does not reach `openshell sandbox exec`.** Baking
`ENV PATH=…` into the Containerfile is not enough. Sandboard always passes
`agent_env` at create; overlay non-secret seat vars on the sandbox spec's
**`env`** (Settings → Sandbox specs — profile wins on key clash). Secrets
belong on Providers, not spec env. See
[Sandbox](sandbox.md#spec-env-and-prompt).

### An uploaded file landed in the wrong place

**`sandbox upload` takes a destination directory**, and the destination must
already exist. Uploading to `/tmp/foo.py` creates a *directory* named
`/tmp/foo.py` with the file inside it. Upload to `/tmp`.

## Restarting sandboard while a card is running

This is safe. The agent runs **detached** inside its sandbox, so it does not
care that sandboard went away.

On startup `reconcile` lists the sandboxes sandboard labelled, matches each against
its card's environment, and picks the run back up for any card still Claimed or
Running. The card stays Running; no second sandbox is created.

Two cases worth knowing:

- **The sandbox is up but nothing is running in it.** The card returns to
  Backlog *without* spending a retry — that was the restart's fault, not the
  card's.
- **The gateway is not back yet.** Startup waits up to 3 minutes, then logs
  `gateway unreachable after 180s; starting without reconciling`. That message
  is loud on purpose: treat every Running card as suspect until you have checked
  it.

## Cards that will not start

A Backlog card is inert until someone dispatches it. Things that clear
`awaiting_dispatch` and leave a card sitting there:

lease expiry, park, halt, release, and request_changes.

With auto mode off, dispatch again. With auto mode on, the next supervisor tick
re-queues it. Auto mode never approves a Review, answers a Needs You, or unparks
anything.

Also check `max_concurrent` — with the default of 1, a second card genuinely
will not start until the first finishes.

## Getting a look at the UI

```bash
npm --prefix web run shots      # → web/shots/*.png
```

Runs a scratch sandboard on `:8081` against a fixture board and captures desktop and
phone views. Your real board is untouched.
