# Invariants

Properties sandboard will not trade away as the surface grows, and why each one
matters. If a change would break one of these, the change is wrong.

## One state machine

Every mutation — UI, MCP, supervisor — goes through `Board` in `src/store.rs`.
Legal transitions live in `src/machine.rs`. No transport holds state-machine
logic.

*Why:* the board has several faces and they must not drift. A rule encoded in
`api.rs` is a rule MCP does not have, and the first time those disagree you have
two products.

## Workers cannot reach sandboard

The card agent gets no network path to sandboard. The supervisor calls `claim` /
`heartbeat` / `report` on its behalf.

*Why:* an agent that could reach sandboard's MCP could approve its own review. The
containment is what makes the review boundary real.

## Liveness is observed

The supervisor parses the agent's output stream. There is no timer-based
keepalive.

*Why:* a keepalive can fire while the agent is wedged. Then the lease no longer
means anything — and a wedged agent holding a valid lease is exactly the case
the lease exists to catch.

## Merging is human

Approving in sandboard surfaces the pull request. It does not merge.

*Why:* merge is irreversible and needs a human. A card that passes every gate
can still be building the wrong thing.

## Feature branches are writable; the default branch is human-gated

Agents push `sandboard/card-*` and open PRs. A repository ruleset keeps the default
branch owner-only.

*Why:* defence in depth for the rule above. The boundary should hold even if
sandboard has a bug.

## Everything in the sandbox stack fails as a hang

Denied egress, a missing credential, a wedged relay — all of it presents as
silence, never as an error. Every exec carries a deadline, and silence is
treated as failure.

*Why:* this is how the stack behaves, and it shapes the code thoroughly. It is
why `openshell.rs` looks the way it does, and why "it is taking a while" usually
means "it has already failed."

## Conventions that follow from these

**Comments explain why, not what.** A comment that restates the line below it is
noise.

**Describe how it works now.** Docs, UI copy, MCP descriptions, and briefings
should make sense to someone who never saw the previous design. Bug-history
notes that justify a still-present invariant are fine; teaching the product by
arguing with its past is not.

**Tests name the failure they prevent**, not the function they call.
`machine.rs` holds the lifecycle invariants; other modules test what breaks
silently — argv shape, shell quoting, config validation.

## Working on sandboard

```bash
cargo test
cargo clippy --all-targets -- -D warnings
```

Both must be clean. A card's sandbox has no pre-baked sandboard build cache —
`cargo`/`npm` reach crates.io/npm live (see [Sandbox](sandbox.md#image)) — so
`--offline` no longer applies there; `--locked` still does.

Stage specific paths. `git add -A` has committed unintended local state here
before.

### Building these docs

```bash
make docs             # mdbook build → target/mdbook
make docs-serve       # http://localhost:3000
```

Screenshots are **not** committed. CI captures them from a real board against
the fixture in `web/ui-fixture.mjs` and drops them into `docs/images/` before
mdBook runs, so a local `make docs` builds without them. To see them locally:

```bash
npm --prefix web run shots     # → web/shots/
cp web/shots/*.png docs/images/
```

CI publishes `target/mdbook` to
[`sandboard-app/sandboard-app.github.io`](https://github.com/sandboard-app/sandboard-app.github.io)
via a write deploy key (`PAGES_DEPLOY_KEY`). The org's **Deploy keys** setting
must stay enabled.
