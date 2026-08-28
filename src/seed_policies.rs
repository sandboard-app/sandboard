//! Minimal OpenShell policy used as create-form / last-resort defaults, plus
//! one seeded per-engine Cockpit policy matching the split `sandbox-<engine>`
//! images (`sandbox/Containerfile`).
//!
//! Live policy lives in the board Policies catalog (Settings → OpenShell → Policies).
//! Sandbox specs reference a policy by id; create materializes YAML for OpenShell.

/// Stable id for the seeded minimal policy row.
pub const MINIMAL_POLICY_ID: &str = "minimal";

/// Display name for [`MINIMAL_POLICY_ID`].
pub const MINIMAL_POLICY_NAME: &str = "Minimal";

/// Bare-bones policy for a new sandbox spec. No sandboard MCP, no package registries,
/// no language-toolchain paths — operators add egress as needed.
pub const MINIMAL_SANDBOX_POLICY: &str = r#"# Minimal OpenShell sandbox policy.
# Edit under Settings → OpenShell → Policies for your egress needs.
version: 1

filesystem_policy:
  include_workdir: true
  read_only: [/usr, /lib, /proc, /etc, /var/log]
  read_write: [/sandbox, /tmp, /dev]

landlock:
  compatibility: best_effort

network_policies: {}
"#;

/// Stable ids for the seeded per-engine cockpit policy rows, matching the
/// split `sandbox-<engine>` images.
pub const COCKPIT_CURSOR_POLICY_ID: &str = "cockpit-cursor";
pub const COCKPIT_AGY_POLICY_ID: &str = "cockpit-agy";
pub const COCKPIT_CLAUDE_POLICY_ID: &str = "cockpit-claude";
pub const COCKPIT_OPENCODE_POLICY_ID: &str = "cockpit-opencode";

pub const COCKPIT_CURSOR_POLICY_NAME: &str = "Cockpit (cursor)";
pub const COCKPIT_AGY_POLICY_NAME: &str = "Cockpit (agy)";
pub const COCKPIT_CLAUDE_POLICY_NAME: &str = "Cockpit (claude)";
pub const COCKPIT_OPENCODE_POLICY_NAME: &str = "Cockpit (opencode)";

/// Minimal Cockpit policy for `sandbox-cursor`: card-work toolchain + GitHub
/// egress (git/gh) + Cursor's own API. Host sandboard MCP is stdio over a local
/// Unix socket (`socat`, see `cockpit_mcp_tunnel`) — no network hop, so no
/// entry for it. Cursor does not go through OpenShell `inference.local`, so
/// no separate inference block.
pub const COCKPIT_CURSOR_POLICY: &str = r#"# Seeded Cockpit policy for the sandbox-cursor image. Adjust under
# Settings → OpenShell → Policies; this is a starting point, not a floor.
version: 1

filesystem_policy:
  include_workdir: true
  read_only: [/usr, /lib, /proc, /app, /etc, /var/log, /opt/rust, /opt/cursor-agent]
  read_write: [/sandbox, /tmp, /dev, /opt/cargo, /opt/cargo-target, /opt/npm-cache]

landlock:
  compatibility: best_effort

network_policies:
  github:
    name: github
    endpoints:
      - { host: api.github.com, port: 443, protocol: rest, enforcement: enforce, access: full }
      - { host: github.com, port: 443, protocol: rest, enforcement: enforce, access: full }
    binaries:
      - { path: /usr/bin/git }
      - { path: /usr/local/bin/git }
      - { path: /usr/bin/gh }
      - { path: /usr/local/bin/gh }
      - { path: /usr/bin/git-remote-https }
      - { path: /usr/lib/git-core/git-remote-https }
      - { path: /usr/bin/curl }
      - { path: /usr/local/bin/curl }
      - { path: /bin/sh }
      - { path: /usr/bin/sh }
      - { path: /bin/bash }
      - { path: /usr/bin/bash }
      - { path: /usr/local/bin/agent }
      - { path: /usr/local/bin/cursor-agent }
      - { path: /opt/cursor-agent/versions/**/cursor-agent }
      - { path: /opt/cursor-agent/versions/**/node }
      - { path: /usr/bin/node }
      - { path: /usr/local/bin/node }
      # Cockpit sandboard MCP stdio relay client (mcp.json → agent.sock).
      - { path: /usr/bin/socat }

  cargo_npm:
    name: cargo-npm
    endpoints:
      - { host: index.crates.io, port: 443, protocol: rest, enforcement: enforce, access: full }
      - { host: static.crates.io, port: 443, protocol: rest, enforcement: enforce, access: full }
      - { host: registry.npmjs.org, port: 443, protocol: rest, enforcement: enforce, access: full }
      - { host: github.com, port: 443, protocol: rest, enforcement: enforce, access: full }
    binaries:
      # /opt/cargo/bin/cargo is rustup's proxy binary, which re-execs the real
      # cargo under the toolchain dir — a runtime exec, not a symlink, so the
      # toolchain path needs its own entry (verified live: without it, cargo's
      # own crates.io fetch gets a 403 even though this proxy path is listed).
      - { path: /opt/cargo/bin/cargo }
      - { path: /opt/rust/toolchains/**/bin/cargo }
      - { path: /usr/local/bin/cargo }
      - { path: /usr/bin/npm }
      - { path: /usr/local/bin/npm }
      - { path: /usr/bin/node }
      - { path: /usr/local/bin/node }

  cursor:
    name: cursor
    endpoints:
      - { host: api2.cursor.sh, port: 443, protocol: rest, enforcement: enforce, access: full }
      - { host: '*.api5.cursor.sh', port: 443, access: full, tls: skip }
      - { host: agentn.us.api5.cursor.sh, port: 443, access: full, tls: skip }
      - { host: '*.api5geo.cursor.sh', port: 443, access: full, tls: skip }
      - { host: '*.api5lat.cursor.sh', port: 443, access: full, tls: skip }
      - { host: agentn.api5geo.cursor.sh, port: 443, access: full, tls: skip }
      - { host: agentn.api5lat.cursor.sh, port: 443, access: full, tls: skip }
      - { host: repo.cursor.sh, port: 443, protocol: rest, enforcement: enforce, access: full }
      - { host: 'repo*.cursor.sh', port: 443, protocol: rest, enforcement: enforce, access: full }
      - { host: repo42.cursor.sh, port: 443, protocol: rest, enforcement: enforce, access: full }
      - { host: cursor.blob.core.windows.net, port: 443, protocol: rest, enforcement: enforce, access: full }
      - { host: download.cursor.sh, port: 443, protocol: rest, enforcement: enforce, access: full }
      - { host: downloads.cursor.com, port: 443, protocol: rest, enforcement: enforce, access: full }
      - { host: cursor.download.prss.microsoft.com, port: 443, protocol: rest, enforcement: enforce, access: full }
    binaries:
      - { path: /usr/local/bin/agent }
      - { path: /usr/local/bin/cursor-agent }
      - { path: /opt/cursor-agent/versions/**/cursor-agent }
      - { path: /opt/cursor-agent/versions/**/node }
      - { path: /usr/bin/node }
      - { path: /usr/local/bin/node }
      - { path: /usr/bin/bash }
      - { path: /bin/bash }
      - { path: /usr/bin/socat }
"#;

/// Minimal Cockpit policy for `sandbox-agy`: card-work toolchain + GitHub
/// egress + full Vertex/Antigravity API surface. agy is a direct Google
/// client (does not go through OpenShell `inference.local`), so it needs
/// the real upstream hosts, not just a passthrough hostname.
pub const COCKPIT_AGY_POLICY: &str = r#"# Seeded Cockpit policy for the sandbox-agy image. Adjust under
# Settings → OpenShell → Policies; this is a starting point, not a floor.
version: 1

filesystem_policy:
  include_workdir: true
  read_only: [/usr, /lib, /proc, /app, /etc, /var/log, /opt/rust]
  read_write: [/sandbox, /tmp, /dev, /opt/cargo, /opt/cargo-target, /opt/npm-cache]

landlock:
  compatibility: best_effort

network_policies:
  github:
    name: github
    endpoints:
      - { host: api.github.com, port: 443, protocol: rest, enforcement: enforce, access: full }
      - { host: github.com, port: 443, protocol: rest, enforcement: enforce, access: full }
    binaries:
      - { path: /usr/bin/git }
      - { path: /usr/local/bin/git }
      - { path: /usr/bin/gh }
      - { path: /usr/local/bin/gh }
      - { path: /usr/bin/git-remote-https }
      - { path: /usr/lib/git-core/git-remote-https }
      - { path: /usr/bin/curl }
      - { path: /usr/local/bin/curl }
      - { path: /bin/sh }
      - { path: /usr/bin/sh }
      - { path: /bin/bash }
      - { path: /usr/bin/bash }
      - { path: /usr/local/bin/agy }
      - { path: /usr/bin/agy }
      - { path: /sandbox/.local/bin/agy }
      - { path: /usr/bin/node }
      - { path: /usr/local/bin/node }

  cargo_npm:
    name: cargo-npm
    endpoints:
      - { host: index.crates.io, port: 443, protocol: rest, enforcement: enforce, access: full }
      - { host: static.crates.io, port: 443, protocol: rest, enforcement: enforce, access: full }
      - { host: registry.npmjs.org, port: 443, protocol: rest, enforcement: enforce, access: full }
      - { host: github.com, port: 443, protocol: rest, enforcement: enforce, access: full }
    binaries:
      # /opt/cargo/bin/cargo is rustup's proxy binary, which re-execs the real
      # cargo under the toolchain dir — a runtime exec, not a symlink, so the
      # toolchain path needs its own entry (verified live: without it, cargo's
      # own crates.io fetch gets a 403 even though this proxy path is listed).
      - { path: /opt/cargo/bin/cargo }
      - { path: /opt/rust/toolchains/**/bin/cargo }
      - { path: /usr/local/bin/cargo }
      - { path: /usr/bin/npm }
      - { path: /usr/local/bin/npm }
      - { path: /usr/bin/node }
      - { path: /usr/local/bin/node }

  vertex_ai:
    name: vertex-ai
    endpoints:
      - { host: aiplatform.googleapis.com, port: 443, protocol: rest, enforcement: enforce, access: full }
      - { host: '*-aiplatform.googleapis.com', port: 443, protocol: rest, enforcement: enforce, access: full }
      - { host: cloudcode-pa.googleapis.com, port: 443, protocol: rest, enforcement: enforce, access: full }
      - { host: daily-cloudcode-pa.googleapis.com, port: 443, protocol: rest, enforcement: enforce, access: full }
      - { host: oauth2.googleapis.com, port: 443, protocol: rest, enforcement: enforce, access: full }
      - { host: www.googleapis.com, port: 443, protocol: rest, enforcement: enforce, access: full }
      - { host: play.googleapis.com, port: 443, protocol: rest, enforcement: enforce, access: full }
      - { host: lh3.googleusercontent.com, port: 443, protocol: rest, enforcement: enforce, access: full }
      - { host: antigravity-unleash.goog, port: 443, protocol: rest, enforcement: enforce, access: full }
    binaries:
      - { path: /usr/local/bin/agy }
      - { path: /usr/bin/agy }
      - { path: /sandbox/.local/bin/agy }
      - { path: /usr/bin/node }
      - { path: /usr/local/bin/node }
"#;

/// Minimal Cockpit policy for `sandbox-claude`: card-work toolchain + GitHub
/// egress. Claude's model traffic goes through OpenShell `inference.local`
/// (see `engine::anthropic_inference_env`) — a gateway-side passthrough with
/// no sandbox network hop, so no Vertex/Anthropic endpoint entry is needed.
pub const COCKPIT_CLAUDE_POLICY: &str = r#"# Seeded Cockpit policy for the sandbox-claude image. Adjust under
# Settings → OpenShell → Policies; this is a starting point, not a floor.
version: 1

filesystem_policy:
  include_workdir: true
  read_only: [/usr, /lib, /proc, /app, /etc, /var/log, /opt/rust]
  read_write: [/sandbox, /tmp, /dev, /opt/cargo, /opt/cargo-target, /opt/npm-cache]

landlock:
  compatibility: best_effort

network_policies:
  github:
    name: github
    endpoints:
      - { host: api.github.com, port: 443, protocol: rest, enforcement: enforce, access: full }
      - { host: github.com, port: 443, protocol: rest, enforcement: enforce, access: full }
    binaries:
      - { path: /usr/bin/git }
      - { path: /usr/local/bin/git }
      - { path: /usr/bin/gh }
      - { path: /usr/local/bin/gh }
      - { path: /usr/bin/git-remote-https }
      - { path: /usr/lib/git-core/git-remote-https }
      - { path: /usr/bin/curl }
      - { path: /usr/local/bin/curl }
      - { path: /bin/sh }
      - { path: /usr/bin/sh }
      - { path: /bin/bash }
      - { path: /usr/bin/bash }
      - { path: /usr/local/bin/claude }
      - { path: /usr/bin/node }
      - { path: /usr/local/bin/node }

  cargo_npm:
    name: cargo-npm
    endpoints:
      - { host: index.crates.io, port: 443, protocol: rest, enforcement: enforce, access: full }
      - { host: static.crates.io, port: 443, protocol: rest, enforcement: enforce, access: full }
      - { host: registry.npmjs.org, port: 443, protocol: rest, enforcement: enforce, access: full }
      - { host: github.com, port: 443, protocol: rest, enforcement: enforce, access: full }
    binaries:
      # /opt/cargo/bin/cargo is rustup's proxy binary, which re-execs the real
      # cargo under the toolchain dir — a runtime exec, not a symlink, so the
      # toolchain path needs its own entry (verified live: without it, cargo's
      # own crates.io fetch gets a 403 even though this proxy path is listed).
      - { path: /opt/cargo/bin/cargo }
      - { path: /opt/rust/toolchains/**/bin/cargo }
      - { path: /usr/local/bin/cargo }
      - { path: /usr/bin/npm }
      - { path: /usr/local/bin/npm }
      - { path: /usr/bin/node }
      - { path: /usr/local/bin/node }
"#;

/// Minimal Cockpit policy for `sandbox-opencode`: card-work toolchain +
/// GitHub egress + OpenCode's own domain (model catalog / CLI update
/// checks). Model traffic itself goes through OpenShell `inference.local`
/// (see `engine::anthropic_inference_env`), same passthrough as claude.
pub const COCKPIT_OPENCODE_POLICY: &str = r#"# Seeded Cockpit policy for the sandbox-opencode image. Adjust under
# Settings → OpenShell → Policies; this is a starting point, not a floor.
version: 1

filesystem_policy:
  include_workdir: true
  read_only: [/usr, /lib, /proc, /app, /etc, /var/log, /opt/rust, /opt/opencode]
  read_write: [/sandbox, /tmp, /dev, /opt/cargo, /opt/cargo-target, /opt/npm-cache]

landlock:
  compatibility: best_effort

network_policies:
  github:
    name: github
    endpoints:
      - { host: api.github.com, port: 443, protocol: rest, enforcement: enforce, access: full }
      - { host: github.com, port: 443, protocol: rest, enforcement: enforce, access: full }
    binaries:
      - { path: /usr/bin/git }
      - { path: /usr/local/bin/git }
      - { path: /usr/bin/gh }
      - { path: /usr/local/bin/gh }
      - { path: /usr/bin/git-remote-https }
      - { path: /usr/lib/git-core/git-remote-https }
      - { path: /usr/bin/curl }
      - { path: /usr/local/bin/curl }
      - { path: /bin/sh }
      - { path: /usr/bin/sh }
      - { path: /bin/bash }
      - { path: /usr/bin/bash }
      - { path: /usr/local/bin/opencode }
      - { path: /opt/opencode/bin/opencode }
      - { path: /usr/bin/node }
      - { path: /usr/local/bin/node }

  cargo_npm:
    name: cargo-npm
    endpoints:
      - { host: index.crates.io, port: 443, protocol: rest, enforcement: enforce, access: full }
      - { host: static.crates.io, port: 443, protocol: rest, enforcement: enforce, access: full }
      - { host: registry.npmjs.org, port: 443, protocol: rest, enforcement: enforce, access: full }
      - { host: github.com, port: 443, protocol: rest, enforcement: enforce, access: full }
    binaries:
      # /opt/cargo/bin/cargo is rustup's proxy binary, which re-execs the real
      # cargo under the toolchain dir — a runtime exec, not a symlink, so the
      # toolchain path needs its own entry (verified live: without it, cargo's
      # own crates.io fetch gets a 403 even though this proxy path is listed).
      - { path: /opt/cargo/bin/cargo }
      - { path: /opt/rust/toolchains/**/bin/cargo }
      - { path: /usr/local/bin/cargo }
      - { path: /usr/bin/npm }
      - { path: /usr/local/bin/npm }
      - { path: /usr/bin/node }
      - { path: /usr/local/bin/node }

  opencode:
    name: opencode
    endpoints:
      - { host: models.dev, port: 443, protocol: rest, enforcement: enforce, access: full }
      - { host: opencode.ai, port: 443, protocol: rest, enforcement: enforce, access: full }
      - { host: '*.opencode.ai', port: 443, protocol: rest, enforcement: enforce, access: full }
      - { host: api.opencode.ai, port: 443, protocol: rest, enforcement: enforce, access: full }
    binaries:
      - { path: /usr/local/bin/opencode }
      - { path: /opt/opencode/bin/opencode }
      - { path: /usr/bin/node }
      - { path: /usr/local/bin/node }
      - { path: /usr/bin/bash }
      - { path: /bin/bash }
"#;
