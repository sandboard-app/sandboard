//! Inject MCP client config into sandboxes (cockpit + optional workers).
//!
//! Catalog entries (`McpServerDesired`) render into Cursor/Claude/agy/OpenCode/Hermes
//! shapes under `/sandbox/.sandboard/mcp/`. Cockpit's shipped `sandboard` entry is stdio
//! over a local Unix socket (`cockpit_mcp_tunnel`); vestigial JWTs may still be
//! minted for older HTTP paths (`mcp_oauth`).

use crate::mcp_oauth::{self, OpsMcpTokens};
use crate::model::{
    McpHttpAuth, McpServerDesired, McpTransport, SANDBOARD_MCP_SERVER_ID,
};
use crate::openshell::OpenShell;
use crate::store::SharedBoard;

use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

pub const COCKPIT_MCP_DIR: &str = "/sandbox/.sandboard/mcp";

/// Stdio client for the shipped `sandboard` MCP: retries until the one-shot
/// board relay is listening, then `exec`s `socat`. Direct `socat` races the
/// listen respawn (inject `agent mcp enable`, prior disconnect) and Cursor
/// reports "MCP is not connected" / "Connection closed" after a single miss.
pub const SANDBOARD_MCP_STDIO_WRAPPER: &str = "/sandbox/.sandboard/mcp/sandboard-mcp-stdio";

/// Claude `--bare` does not auto-discover project MCP; attach / engine argv
/// pass this path via `--mcp-config`.
pub const COCKPIT_CLAUDE_MCP_CONFIG: &str = "/sandbox/.sandboard/mcp/claude_mcp.json";

/// Antigravity (`agy`) global MCP config path (`~/.gemini/config/mcp_config.json`).
pub const COCKPIT_AGY_MCP_CONFIG: &str = "/sandbox/.gemini/config/mcp_config.json";

/// OpenCode global config (`~/.config/opencode/opencode.jsonc`) — MCP lives under `mcp`.
pub const COCKPIT_OPENCODE_CONFIG: &str = "/sandbox/.config/opencode/opencode.jsonc";

/// Hermes MCP fragment. The Hermes launcher merges this into its config before
/// starting, keeping the image's baseline config separate from Board injection.
pub const COCKPIT_HERMES_MCP_FRAGMENT: &str = "/sandbox/.sandboard/mcp/hermes_mcp.yaml";

/// Fallback principal when no browser session is available (supervisor reconcile).
pub const COCKPIT_FALLBACK_SUB: &str = "cockpit";

/// OpenShell MITM proxy + CA for stdio children (see OpenShell#886).
const OPENSHELL_HTTPS_PROXY: &str = "http://10.200.0.1:3128";
const OPENSHELL_CA_BUNDLE: &str = "/etc/openshell-tls/ca-bundle.pem";
const OPENSHELL_CA_PEM: &str = "/etc/openshell-tls/openshell-ca.pem";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Msg(String),
    #[error("openshell: {0}")]
    OpenShell(#[from] crate::openshell::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

impl From<String> for Error {
    fn from(s: String) -> Self {
        Error::Msg(s)
    }
}

/// Mint tokens for `sub` and write MCP config into `sandbox` (cockpit).
pub async fn provision_cockpit_mcp(
    board: &SharedBoard,
    os: &OpenShell,
    sandbox: &str,
    sub: &str,
) -> Result<OpsMcpTokens> {
    let resource = mcp_oauth::cockpit_mcp_resource();
    let tokens = mcp_oauth::mint_cockpit_seat_tokens(board, sub, &resource)?;
    // Fail closed if mint and verify disagree (keeps inject from shipping junk).
    if mcp_oauth::verify_cockpit_access_token(board, &tokens.access_token, &resource).as_deref()
        != Some(sub.trim())
    {
        return Err(Error::Msg(
            "minted cockpit access token failed resource verify".into(),
        ));
    }
    let servers = mcp_servers_for_cockpit_inject(board);
    inject_sandbox_mcp(os, sandbox, Some(&tokens), &servers).await?;
    Ok(tokens)
}

/// Inject MCP config for a worker sandbox (no cockpit Bearer / host `/mcp`).
///
/// Always writes config files (possibly empty `mcpServers`) so Claude
/// `--strict-mcp-config` has a valid path.
pub async fn provision_worker_mcp(
    board: &SharedBoard,
    os: &OpenShell,
    sandbox: &str,
    resolved: &crate::model::ResolvedSandboxCreate,
) -> Result<()> {
    let servers = board.attach_mcp_servers_for_resolved(resolved, false);
    inject_sandbox_mcp(os, sandbox, None, &servers).await
}

/// Cockpit inject list: profile attachments + shipped `sandboard` if missing.
fn mcp_servers_for_cockpit_inject(board: &SharedBoard) -> Vec<McpServerDesired> {
    let resolved = board.resolve_cockpit_sandbox_create();
    let mut servers = board.attach_mcp_servers_for_resolved(&resolved, true);
    if !servers.iter().any(|s| s.id == SANDBOARD_MCP_SERVER_ID) {
        if let Some(sandboard) = board.get_mcp_server(SANDBOARD_MCP_SERVER_ID) {
            servers.insert(0, sandboard);
        }
    }
    servers
}

async fn inject_sandbox_mcp(
    os: &OpenShell,
    sandbox: &str,
    tokens: Option<&OpsMcpTokens>,
    servers: &[McpServerDesired],
) -> Result<()> {
    let staging = staging_dir()?;
    std::fs::create_dir_all(&staging)?;

    if let Some(tokens) = tokens {
        let token_path = staging.join("token.json");
        let token_doc = json!({
            "access_token": tokens.access_token,
            "refresh_token": tokens.refresh_token,
            "expires_at": tokens.expires_at,
            "expires_in": tokens.expires_in,
            "resource": tokens.resource,
            "client_id": tokens.client_id,
            "sub": tokens.sub,
        });
        std::fs::write(
            &token_path,
            serde_json::to_vec_pretty(&token_doc).map_err(|e| Error::Msg(e.to_string()))?,
        )?;
    }

    let mcp_doc = mcp_json_document(tokens, servers);
    let mcp_bytes = serde_json::to_vec_pretty(&mcp_doc).map_err(|e| Error::Msg(e.to_string()))?;
    let mcp_path = staging.join("mcp.json");
    let claude_path = staging.join("claude_mcp.json");
    let opencode_path = staging.join("opencode.jsonc");
    let hermes_filename = std::path::Path::new(COCKPIT_HERMES_MCP_FRAGMENT)
        .file_name()
        .ok_or_else(|| Error::Msg("Hermes MCP fragment path has no filename".into()))?;
    let hermes_path = staging.join(hermes_filename);
    let env_path = staging.join("env.sh");
    std::fs::write(&mcp_path, &mcp_bytes)?;
    // Same Cursor-shaped map for Claude Code's config reader.
    std::fs::write(&claude_path, &mcp_bytes)?;
    let opencode_bytes = serde_json::to_vec_pretty(&opencode_jsonc_document(tokens, servers))
        .map_err(|e| Error::Msg(e.to_string()))?;
    std::fs::write(&opencode_path, &opencode_bytes)?;
    let hermes_yaml = serde_yaml::to_string(&hermes_mcp_document(tokens, servers))
        .map_err(|e| Error::Msg(e.to_string()))?;
    std::fs::write(&hermes_path, hermes_yaml)?;

    // Nothing to export: the shipped sandboard entry is stdio over a local Unix
    // socket (see mcp.json / cockpit_mcp_tunnel::AGENT_SOCK_PATH) — no URL,
    // no Bearer.
    let env_sh = "# sandboard sandbox MCP — socat stdio relay, see /sandbox/.sandboard/mcp/mcp.json\n";
    std::fs::write(&env_path, env_sh)?;

    let wrapper_path = staging.join("sandboard-mcp-stdio");
    std::fs::write(&wrapper_path, sandboard_mcp_stdio_wrapper_script())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&wrapper_path, std::fs::Permissions::from_mode(0o755))?;
    }

    // Ensure destination exists; upload takes a directory.
    let mkdir = os
        .exec(
            sandbox,
            &format!("mkdir -p {COCKPIT_MCP_DIR} /sandbox/.cursor /sandbox/.gemini/config /sandbox/.config/opencode"),
            std::time::Duration::from_secs(60),
        )
        .await?;
    if !mkdir.ok() {
        return Err(Error::Msg(format!(
            "mkdir mcp dir failed: {}",
            mkdir.stderr.trim()
        )));
    }

    if tokens.is_some() {
        os.upload(
            sandbox,
            staging.join("token.json").to_str().unwrap(),
            COCKPIT_MCP_DIR,
        )
        .await?;
    }
    os.upload(sandbox, mcp_path.to_str().unwrap(), COCKPIT_MCP_DIR)
        .await?;
    os.upload(sandbox, claude_path.to_str().unwrap(), COCKPIT_MCP_DIR)
        .await?;
    os.upload(sandbox, opencode_path.to_str().unwrap(), COCKPIT_MCP_DIR)
        .await?;
    os.upload(sandbox, hermes_path.to_str().unwrap(), COCKPIT_MCP_DIR)
        .await?;
    os.upload(sandbox, env_path.to_str().unwrap(), COCKPIT_MCP_DIR)
        .await?;
    os.upload(sandbox, wrapper_path.to_str().unwrap(), COCKPIT_MCP_DIR)
        .await?;

    let out = os
        .exec(
            sandbox,
            &format!(
                r#"
set -e
chmod 755 {SANDBOARD_MCP_STDIO_WRAPPER}
# Expand Bearer ${{ENV}} placeholders to the OpenShell resolve token from the
# process env so Cursor/Claude send a value the egress proxy can rewrite.
python3 - <<'PY'
import json, os, re
from pathlib import Path

def expand(obj):
    if isinstance(obj, dict):
        return {{k: expand(v) for k, v in obj.items()}}
    if isinstance(obj, list):
        return [expand(v) for v in obj]
    if isinstance(obj, str):
        def repl(m):
            return os.environ.get(m.group(1), m.group(0))
        return re.sub(r"\$\{{([A-Za-z_][A-Za-z0-9_]*)\}}", repl, obj)
    return obj

for rel in ("mcp.json", "claude_mcp.json", "opencode.jsonc"):
    path = Path("{COCKPIT_MCP_DIR}") / rel
    if not path.exists():
        continue
    doc = json.loads(path.read_text())
    path.write_text(json.dumps(expand(doc), indent=2) + "\n")
PY
mkdir -p /sandbox/.cursor /sandbox/repo/.cursor
cp -f {COCKPIT_MCP_DIR}/mcp.json /sandbox/.cursor/mcp.json 2>/dev/null || true
cp -f {COCKPIT_MCP_DIR}/mcp.json /sandbox/repo/.cursor/mcp.json 2>/dev/null || true
# Project-scoped Claude MCP (non-bare / discovery); --bare still needs --mcp-config.
cp -f {COCKPIT_MCP_DIR}/claude_mcp.json /sandbox/repo/.mcp.json 2>/dev/null || true
# Antigravity reads ~/.gemini/config/mcp_config.json (same HTTP MCP shape as Cursor).
cp -f {COCKPIT_MCP_DIR}/mcp.json {COCKPIT_AGY_MCP_CONFIG} 2>/dev/null || true
# OpenCode: merge `mcp.*` into opencode.jsonc (remote + local).
python3 - <<'PY'
import json
from pathlib import Path
p = Path("{COCKPIT_OPENCODE_CONFIG}")
frag = json.loads(Path("{COCKPIT_MCP_DIR}/opencode.jsonc").read_text())
doc = {{}}
if p.exists():
    try:
        doc = json.loads(p.read_text())
    except Exception:
        doc = {{}}
if not isinstance(doc, dict):
    doc = {{}}
mcp = doc.get("mcp") if isinstance(doc.get("mcp"), dict) else {{}}
mcp.update(frag.get("mcp") or {{}})
doc["mcp"] = mcp
if "$schema" not in doc and "$schema" in frag:
    doc["$schema"] = frag["$schema"]
p.parent.mkdir(parents=True, exist_ok=True)
p.write_text(json.dumps(doc, indent=2) + "\n")
PY
if ! grep -q 'sandboard/mcp/env.sh' /sandbox/.bashrc 2>/dev/null; then
  printf '\n# sandboard MCP\n[ -f %s/env.sh ] && . %s/env.sh\n' {COCKPIT_MCP_DIR} {COCKPIT_MCP_DIR} >> /sandbox/.bashrc
fi
# Cursor 2026.08+: project mcp.json servers stay "needs approval" / unloaded
# even with `agent --approve-mcps` (observed on Cockpit attach + resume).
# `agent mcp enable <id>` writes the project approval Cursor actually checks
# (`~/.cursor/projects/<id>/mcp-approvals.json`). Best-effort — images
# without the Cursor CLI (agy/claude/opencode-only) skip this.
if command -v agent >/dev/null 2>&1; then
  (
    export HOME=/sandbox USER=sandbox
    cd /sandbox/repo 2>/dev/null || cd /sandbox
    python3 - <<'PY'
import json, subprocess
from pathlib import Path
doc = json.loads(Path("/sandbox/.cursor/mcp.json").read_text())
for name in (doc.get("mcpServers") or {{}}):
    subprocess.run(
        ["agent", "mcp", "enable", name],
        check=False,
        env={{**dict(__import__("os").environ), "HOME": "/sandbox", "USER": "sandbox"}},
    )
PY
  ) || true
fi
true
"#
            ),
            std::time::Duration::from_secs(60),
        )
        .await?;
    if !out.ok() {
        return Err(Error::Msg(format!(
            "place mcp failed: {}",
            out.stderr.trim()
        )));
    }
    let _ = staging;
    Ok(())
}

pub async fn clear_cockpit_mcp(os: &OpenShell, sandbox: &str) -> Result<()> {
    let out = os
        .exec(
            sandbox,
            &format!(
                r#"
rm -rf {COCKPIT_MCP_DIR} /sandbox/.cursor/mcp.json /sandbox/repo/.cursor/mcp.json {COCKPIT_AGY_MCP_CONFIG} 2>/dev/null
python3 - <<'PY'
import json
from pathlib import Path
p = Path("{COCKPIT_OPENCODE_CONFIG}")
if not p.exists():
    raise SystemExit(0)
try:
    doc = json.loads(p.read_text())
except Exception:
    raise SystemExit(0)
mcp = doc.get("mcp")
if isinstance(mcp, dict):
    mcp.pop("sandboard", None)
    # Drop catalog keys we may have written; leave operator-owned entries.
    for k in list(mcp.keys()):
        if k.startswith("sandboard-") or k == "sandboard":
            mcp.pop(k, None)
    doc["mcp"] = mcp
p.write_text(json.dumps(doc, indent=2) + "\n")
PY
true
"#
            ),
            std::time::Duration::from_secs(60),
        )
        .await?;
    if !out.ok() {
        return Err(Error::Msg(format!(
            "clear mcp failed: {}",
            out.stderr.trim()
        )));
    }
    Ok(())
}

fn staging_dir() -> Result<PathBuf> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!(
        "sandboard-cockpit-mcp-{}-{}",
        std::process::id(),
        nanos
    ));
    Ok(dir)
}

/// Build Cursor/Claude/agy JSON bodies (testable without upload).
pub fn mcp_json_document(
    tokens: Option<&OpsMcpTokens>,
    servers: &[McpServerDesired],
) -> serde_json::Value {
    let mut map = Map::new();
    for s in servers {
        if let Some(entry) = render_cursor_entry(s, tokens) {
            map.insert(s.id.clone(), entry);
        }
    }
    json!({ "mcpServers": Value::Object(map) })
}

/// OpenCode `opencode.jsonc` fragment (`mcp` map).
pub fn opencode_jsonc_document(
    tokens: Option<&OpsMcpTokens>,
    servers: &[McpServerDesired],
) -> serde_json::Value {
    let mut map = Map::new();
    for s in servers {
        if let Some(entry) = render_opencode_entry(s, tokens) {
            map.insert(s.id.clone(), entry);
        }
    }
    json!({
        "$schema": "https://opencode.ai/config.json",
        "mcp": Value::Object(map)
    })
}

/// Build Hermes' `mcp_servers` YAML-compatible document.
///
/// Hermes uses its own config shape rather than the Cursor/OpenCode `type`
/// discriminator. The launcher merges this Board-owned fragment into the
/// image's baseline config immediately before each headless or interactive run.
pub fn hermes_mcp_document(
    tokens: Option<&OpsMcpTokens>,
    servers: &[McpServerDesired],
) -> serde_json::Value {
    let mut map = Map::new();
    for server in servers {
        let entry = match &server.transport {
            McpTransport::Http { url, auth } => {
                let Some(url) = resolve_http_url(url, auth, tokens) else {
                    continue;
                };
                let mut entry = Map::new();
                entry.insert("url".into(), json!(url));
                if let Some(headers) = http_headers(auth, tokens) {
                    entry.insert("headers".into(), headers);
                }
                Value::Object(entry)
            }
            McpTransport::Stdio {
                command,
                args,
                ..
            } => {
                let Some((command, args)) = resolve_stdio_command(command, args) else {
                    continue;
                };
                let env = if is_sandboard_uds_relay(&command, &args) {
                    local_stdio_env()
                } else {
                    stdio_env(&server.env)
                };
                json!({
                    "command": command,
                    "args": args,
                    "env": env,
                })
            }
        };
        map.insert(server.id.clone(), entry);
    }
    json!({ "mcp_servers": Value::Object(map) })
}

fn render_cursor_entry(server: &McpServerDesired, tokens: Option<&OpsMcpTokens>) -> Option<Value> {
    match &server.transport {
        McpTransport::Http { url, auth } => {
            let url = resolve_http_url(url, auth, tokens)?;
            let mut entry = Map::new();
            entry.insert("type".into(), json!("http"));
            entry.insert("url".into(), json!(url));
            if let Some(headers) = http_headers(auth, tokens) {
                entry.insert("headers".into(), headers);
            }
            if !server.env.is_empty() {
                entry.insert("env".into(), json!(server.env));
            }
            Some(Value::Object(entry))
        }
        McpTransport::Stdio {
            command,
            args,
            cwd,
        } => {
            let (command, args) = resolve_stdio_command(command, args)?;
            let mut entry = Map::new();
            // Cursor requires type for stdio; omitting it leaves the server out
            // of the live MCP catalog while HTTP siblings still show up.
            entry.insert("type".into(), json!("stdio"));
            entry.insert("command".into(), json!(command));
            entry.insert("args".into(), json!(args));
            // Local unix-socket relay must not inherit OpenShell proxy env —
            // Cursor replaces the process env with this map, and `socat`
            // then tries ALL_PROXY for the UDS connect (connection refused).
            let env = if is_sandboard_uds_relay(&command, &args) {
                local_stdio_env()
            } else {
                stdio_env(&server.env)
            };
            entry.insert("env".into(), json!(env));
            if let Some(c) = cwd {
                entry.insert("cwd".into(), json!(c));
            }
            Some(Value::Object(entry))
        }
    }
}

fn render_opencode_entry(server: &McpServerDesired, tokens: Option<&OpsMcpTokens>) -> Option<Value> {
    match &server.transport {
        McpTransport::Http { url, auth } => {
            let url = resolve_http_url(url, auth, tokens)?;
            let mut entry = Map::new();
            entry.insert("type".into(), json!("remote"));
            entry.insert("url".into(), json!(url));
            if let Some(headers) = http_headers(auth, tokens) {
                entry.insert("headers".into(), headers);
            }
            Some(Value::Object(entry))
        }
        McpTransport::Stdio {
            command,
            args,
            cwd,
        } => {
            let (command, args) = resolve_stdio_command(command, args)?;
            let mut entry = Map::new();
            entry.insert("type".into(), json!("local"));
            let mut cmdline = vec![command.clone()];
            cmdline.extend(args.iter().cloned());
            entry.insert("command".into(), json!(cmdline));
            let env = if is_sandboard_uds_relay(&command, &args) {
                local_stdio_env()
            } else {
                stdio_env(&server.env)
            };
            entry.insert("environment".into(), json!(env));
            if let Some(c) = cwd {
                entry.insert("cwd".into(), json!(c));
            }
            Some(Value::Object(entry))
        }
    }
}

/// Retrying stdio client for the one-shot `UNIX-LISTEN` relay in
/// `cockpit_mcp_tunnel`. Uploaded beside `mcp.json` on inject.
fn sandboard_mcp_stdio_wrapper_script() -> String {
    format!(
        r#"#!/bin/sh
# Board MCP relay listens one-shot; brief gaps between accepts are normal.
sock='{sock}'
i=0
while [ "$i" -lt 50 ]; do
  if [ -S "$sock" ]; then
    exec /usr/bin/socat - "UNIX-CONNECT:$sock"
  fi
  i=$((i + 1))
  sleep 0.1
done
echo "sandboard-mcp-stdio: $sock not listening after retries" >&2
exit 1
"#,
        sock = crate::cockpit_mcp_tunnel::AGENT_SOCK_PATH
    )
}

/// Fill in the shipped sandboard entry's placeholder (empty `command`) with the
/// cockpit MCP relay's stdio client — mirrors `resolve_http_url`'s empty-URL
/// placeholder for the same reason: model.rs stays free of a dependency on
/// the inject layer. Wrapper (not bare `socat`): see `SANDBOARD_MCP_STDIO_WRAPPER`.
fn resolve_stdio_command(command: &str, args: &[String]) -> Option<(String, Vec<String>)> {
    let t = command.trim();
    if !t.is_empty() {
        return Some((t.to_string(), args.to_vec()));
    }
    Some((SANDBOARD_MCP_STDIO_WRAPPER.into(), Vec::new()))
}

fn is_sandboard_uds_relay(command: &str, args: &[String]) -> bool {
    if command == SANDBOARD_MCP_STDIO_WRAPPER && args.is_empty() {
        return true;
    }
    // Pre-wrapper mcp.json shape (hot-patched sandboxes / older injects).
    command == "socat"
        && args.len() == 2
        && args[0] == "-"
        && args[1] == format!("UNIX-CONNECT:{}", crate::cockpit_mcp_tunnel::AGENT_SOCK_PATH)
}

/// Minimal env for the local `socat` relay client. No proxy/CA — those break
/// Unix-domain connects when the MCP client replaces the process environment.
fn local_stdio_env() -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    env.insert(
        "PATH".into(),
        "/usr/local/bin:/usr/bin:/usr/sbin:/bin:/sbin:/sandbox/.local/bin".into(),
    );
    env.insert("HOME".into(), "/sandbox".into());
    env.insert("USER".into(), "sandbox".into());
    env
}

fn resolve_http_url(
    url: &str,
    auth: &McpHttpAuth,
    tokens: Option<&OpsMcpTokens>,
) -> Option<String> {
    let t = url.trim();
    if !t.is_empty() {
        return Some(t.to_string());
    }
    if matches!(auth, McpHttpAuth::CockpitBearer) {
        if let Some(tok) = tokens {
            return Some(tok.resource.clone());
        }
        return Some(mcp_oauth::cockpit_mcp_resource());
    }
    None
}

fn http_headers(auth: &McpHttpAuth, tokens: Option<&OpsMcpTokens>) -> Option<Value> {
    match auth {
        McpHttpAuth::None => None,
        McpHttpAuth::CockpitBearer => {
            let access = tokens?.access_token.as_str();
            Some(json!({ "Authorization": format!("Bearer {access}") }))
        }
        McpHttpAuth::OAuth { env, .. } => {
            // Engines expand env in headers inconsistently; document as literal
            // placeholder — providers inject the OpenShell resolve token into
            // the process env; egress rewrites it to the current access token.
            Some(json!({ "Authorization": format!("Bearer ${{{env}}}") }))
        }
    }
}

fn stdio_env(extra: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    // Cursor (and some other clients) replace the process environment with the
    // MCP `env` map instead of merging. Without PATH/HOME, relative commands
    // like `uv` fail with FileNotFoundError and the client reports
    // "Connection closed" / "MCP is not connected".
    let mut env = BTreeMap::new();
    env.insert(
        "PATH".into(),
        "/usr/local/bin:/usr/bin:/bin:/sandbox/.local/bin".into(),
    );
    env.insert("HOME".into(), "/sandbox".into());
    env.insert("USER".into(), "sandbox".into());
    env.insert("HTTPS_PROXY".into(), OPENSHELL_HTTPS_PROXY.into());
    env.insert("HTTP_PROXY".into(), OPENSHELL_HTTPS_PROXY.into());
    env.insert("https_proxy".into(), OPENSHELL_HTTPS_PROXY.into());
    env.insert("http_proxy".into(), OPENSHELL_HTTPS_PROXY.into());
    env.insert("ALL_PROXY".into(), OPENSHELL_HTTPS_PROXY.into());
    env.insert("NO_PROXY".into(), "127.0.0.1,localhost,::1".into());
    env.insert("no_proxy".into(), "127.0.0.1,localhost,::1".into());
    env.insert("SSL_CERT_FILE".into(), OPENSHELL_CA_BUNDLE.into());
    env.insert("REQUESTS_CA_BUNDLE".into(), OPENSHELL_CA_BUNDLE.into());
    env.insert("NODE_EXTRA_CA_CERTS".into(), OPENSHELL_CA_PEM.into());
    for (k, v) in extra {
        env.insert(k.clone(), v.clone());
    }
    // OpenShell metadata helper: keep IP in lockstep when only HOST is set.
    if env.contains_key("GCE_METADATA_HOST") && !env.contains_key("GCE_METADATA_IP") {
        if let Some(host) = env.get("GCE_METADATA_HOST").cloned() {
            env.insert("GCE_METADATA_IP".into(), host);
        }
    }
    env
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::openshell::{OpenShell, Output};
    use std::sync::Arc;

    #[test]
    fn mcp_json_oauth_uses_env_placeholder() {
        let server = McpServerDesired {
            id: "jira".into(),
            name: "Jira".into(),
            transport: McpTransport::Http {
                url: "https://mcp.example.com/v1".into(),
                auth: McpHttpAuth::OAuth {
                    provider: "mcp-jira".into(),
                    env: "MCP_OAUTH_JIRA_ACCESS_TOKEN".into(),
                },
            },
            policy_fragment_yaml: None,
            provider_names: vec!["mcp-jira".into()],
            env: Default::default(),
            audience: crate::model::McpAudience::Both,
            shipped: false,
        };
        let doc = mcp_json_document(None, std::slice::from_ref(&server));
        assert_eq!(
            doc["mcpServers"]["jira"]["headers"]["Authorization"],
            "Bearer ${MCP_OAUTH_JIRA_ACCESS_TOKEN}"
        );
    }

    #[test]
    fn mcp_json_resolves_shipped_sandboard_to_stdio_socat_relay() {
        let tokens = OpsMcpTokens {
            access_token: "tok-access".into(),
            refresh_token: "tok-refresh".into(),
            expires_in: 3600,
            expires_at: 999,
            resource: mcp_oauth::cockpit_mcp_resource(),
            client_id: mcp_oauth::COCKPIT_CLIENT_ID.into(),
            sub: "admin".into(),
        };
        let sandboard = McpServerDesired::shipped_sandboard();
        let doc = mcp_json_document(Some(&tokens), std::slice::from_ref(&sandboard));
        assert_eq!(doc["mcpServers"]["sandboard"]["type"], "stdio");
        assert_eq!(
            doc["mcpServers"]["sandboard"]["command"],
            SANDBOARD_MCP_STDIO_WRAPPER
        );
        assert_eq!(doc["mcpServers"]["sandboard"]["args"], json!([]));
        // Stdio transport has no headers/Authorization at all.
        assert!(doc["mcpServers"]["sandboard"].get("headers").is_none());
        assert!(doc["mcpServers"]["sandboard"].get("url").is_none());
        // Must not ship proxy env — Cursor replaces process env and `socat` breaks.
        assert!(doc["mcpServers"]["sandboard"]["env"].get("ALL_PROXY").is_none());
        assert!(doc["mcpServers"]["sandboard"]["env"].get("HTTP_PROXY").is_none());
        assert_eq!(doc["mcpServers"]["sandboard"]["env"]["HOME"], "/sandbox");

        let oc = opencode_jsonc_document(Some(&tokens), std::slice::from_ref(&sandboard));
        assert_eq!(oc["mcp"]["sandboard"]["type"], "local");
        assert_eq!(
            oc["mcp"]["sandboard"]["command"],
            json!([SANDBOARD_MCP_STDIO_WRAPPER])
        );
        assert!(oc["mcp"]["sandboard"]["environment"].get("ALL_PROXY").is_none());
    }

    #[test]
    fn hermes_mcp_document_uses_hermes_server_shape() {
        let sandboard = McpServerDesired::shipped_sandboard();
        let doc = hermes_mcp_document(None, std::slice::from_ref(&sandboard));
        let entry = &doc["mcp_servers"]["sandboard"];
        assert_eq!(entry["command"], SANDBOARD_MCP_STDIO_WRAPPER);
        assert_eq!(entry["args"], json!([]));
        assert_eq!(entry["env"]["HOME"], "/sandbox");
        assert!(entry.get("type").is_none());
        assert!(doc["mcp_servers"]["sandboard"].get("url").is_none());

        let yaml = serde_yaml::to_string(&doc).expect("Hermes YAML");
        assert!(yaml.contains("mcp_servers:"), "{yaml}");
        assert!(yaml.contains("command: /sandbox/.sandboard/mcp/sandboard-mcp-stdio"), "{yaml}");
    }

    #[test]
    fn mcp_json_renders_stdio_with_proxy_env() {
        let srv = McpServerDesired {
            id: "cnv".into(),
            name: "CNV context".into(),
            transport: McpTransport::Stdio {
                command: "uv".into(),
                args: vec![
                    "tool".into(),
                    "run".into(),
                    "--from".into(),
                    "context-server@latest".into(),
                    "context-server".into(),
                    "serve".into(),
                    "--db".into(),
                    "/tmp/spike.db".into(),
                ],
                cwd: None,
            },
            policy_fragment_yaml: None,
            provider_names: vec!["gcp-adc".into()],
            env: BTreeMap::from([("GCE_METADATA_HOST".into(), "127.0.0.1:8174".into())]),
            audience: crate::model::McpAudience::Both,
            shipped: false,
        };
        let doc = mcp_json_document(None, std::slice::from_ref(&srv));
        let entry = &doc["mcpServers"]["cnv"];
        assert_eq!(entry["command"], "uv");
        assert_eq!(entry["env"]["HTTPS_PROXY"], OPENSHELL_HTTPS_PROXY);
        assert_eq!(entry["env"]["PATH"], "/usr/local/bin:/usr/bin:/bin:/sandbox/.local/bin");
        assert_eq!(entry["env"]["HOME"], "/sandbox");
        assert_eq!(entry["env"]["GCE_METADATA_HOST"], "127.0.0.1:8174");
        assert_eq!(entry["env"]["GCE_METADATA_IP"], "127.0.0.1:8174");
        let oc = opencode_jsonc_document(None, std::slice::from_ref(&srv));
        assert_eq!(oc["mcp"]["cnv"]["type"], "local");
        assert!(oc["mcp"]["cnv"]["command"].as_array().unwrap().len() > 1);
    }

    #[tokio::test]
    async fn inject_sandbox_mcp_mkdir_and_uploads_via_mock() {
        let seen = Arc::new(parking_lot::Mutex::new(Vec::<Vec<String>>::new()));
        let seen_c = seen.clone();
        let os = OpenShell::mock(
            move |args| {
                seen_c.lock().push(args.to_vec());
                Output {
                    code: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                }
            },
            std::time::Duration::from_secs(5),
        );
        let tokens = OpsMcpTokens {
            access_token: "a".into(),
            refresh_token: "r".into(),
            expires_in: 3600,
            expires_at: 1,
            resource: mcp_oauth::cockpit_mcp_resource(),
            client_id: mcp_oauth::COCKPIT_CLIENT_ID.into(),
            sub: "cockpit".into(),
        };
        let sandboard = McpServerDesired::shipped_sandboard();
        inject_sandbox_mcp(&os, "sandboard-cockpit", Some(&tokens), std::slice::from_ref(&sandboard))
            .await
            .expect("inject");
        let calls = seen.lock().clone();
        assert!(
            calls
                .iter()
                .any(|a| a.windows(2).any(|w| w[0] == "exec" || a.contains(&"sandbox".into()))),
            "expected sandbox cockpit: {calls:?}"
        );
        // mkdir + uploads (token/mcp/claude/opencode/env) + place script
        let uploads = calls
            .iter()
            .filter(|a| a.iter().any(|s| s == "upload"))
            .count();
        assert!(
            uploads >= 6,
            "expected >=6 uploads (token/mcp/claude/opencode/env/wrapper), got {uploads}: {calls:?}"
        );
        let place = calls
            .iter()
            .rev()
            .find(|a| a.iter().any(|s| s.contains("mcp.json")))
            .expect("place script");
        let script = place.join(" ");
        assert!(
            script.contains(COCKPIT_AGY_MCP_CONFIG),
            "must install Antigravity mcp_config.json: {script}"
        );
        assert!(
            script.contains(COCKPIT_OPENCODE_CONFIG) || script.contains("opencode.jsonc"),
            "must install OpenCode mcp config: {script}"
        );
    }
}
