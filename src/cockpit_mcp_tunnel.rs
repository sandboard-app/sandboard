//! Board-owned MCP channel for cockpit: serve `Operator` directly over an
//! `ExecSandboxInteractive` pipe instead of a TCP tunnel.
//!
//! OpenShell sandbox SSH has no RemoteForward, and `ForwardTcp` only dials
//! *into* a sandbox loopback service — either way a TCP-based tunnel needs
//! an in-sandbox listener plus something to pair it with the agent's own
//! connection. `ExecSandboxInteractive` (what cockpit's browser terminal
//! already uses) skips that entirely:
//!
//! 1. sandboard spawns `socat UNIX-LISTEN:<AGENT_SOCK_PATH> STDIO` inside the
//!    sandbox via a raw (non-pty) interactive exec — its stdin/stdout are
//!    the gRPC-piped ends of that call, right here in this process.
//! 2. The agent's MCP client is configured for stdio transport:
//!    `socat - UNIX-CONNECT:<AGENT_SOCK_PATH>`. When that client
//!    disconnects, the one-shot listen `socat` exits, which is how the
//!    board sees session end and re-spawns for the next connect.
//! 3. Those piped bytes ARE the MCP JSON-RPC wire: sandboard wraps them as an
//!    `AsyncRead`/`AsyncWrite` pair and calls `rmcp::serve_server` with the
//!    same `Operator` handler the HTTP `/mcp` endpoint uses.
//!
//! **Why `socat`, not `nc`:** the Debian OpenBSD-netcat build in the sandbox
//! image accepts a UDS connection fine and relays client→relay bytes, but
//! never forwards bytes written to its stdin *after* accept out to the
//! socket — confirmed with a plain shell repro (delayed write into a
//! long-lived pipe feeding `nc -lU`, already-connected peer sees nothing).
//! That is exactly the relay→client direction `serve_server`'s responses
//! need. `socat UNIX-LISTEN:…,STDIO` relays both directions correctly for
//! the same delayed-write shape.
//!
//! One board-owned relay task per cockpit sandbox; the listen/`serve_server`
//! pair restarts across agent MCP reconnects.

use crate::mcp::Operator;
use crate::openshell::{InteractiveEvent, InteractiveExec, OpenShell};
use crate::store::SharedBoard;
use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::task::JoinHandle;

/// Unix domain socket the sandbox-side `socat` relay binds. Agent MCP
/// clients connect to it with `socat - UNIX-CONNECT:<AGENT_SOCK_PATH>`
/// (stdio transport).
pub const AGENT_SOCK_PATH: &str = "/sandbox/.sandboard/mcp/agent.sock";

/// Informational label for the (now vestigial) cockpit JWT `aud` / UI
/// display — nothing sends this over a wire; stdio has no headers.
pub const MCP_TRANSPORT_LABEL: &str = "stdio:socat - UNIX-CONNECT:/sandbox/.sandboard/mcp/agent.sock";

struct TunnelState {
    sandbox: String,
    stop: Arc<AtomicBool>,
    handle: JoinHandle<()>,
}

fn tunnel_slot() -> &'static Mutex<Option<TunnelState>> {
    static SLOT: std::sync::OnceLock<Mutex<Option<TunnelState>>> = std::sync::OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

fn ensure_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

/// One-shot listen: exits when the agent MCP client disconnects so the board
/// can re-spawn. (`fork` would hide connection boundaries on the gRPC pipe.)
fn relay_command() -> Vec<String> {
    let sock = shell_single_quote(AGENT_SOCK_PATH);
    vec![
        "sh".into(),
        "-c".into(),
        format!(
            "mkdir -p $(dirname {sock}) && rm -f {sock} && exec socat UNIX-LISTEN:{sock} STDIO"
        ),
    ]
}

fn readiness_probe() -> String {
    format!(
        "test -S {} && echo LISTEN || echo DOWN",
        shell_single_quote(AGENT_SOCK_PATH)
    )
}

fn shell_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\"'\"'"))
}

fn sandbox_gone(err: &str) -> bool {
    let e = err.to_ascii_lowercase();
    e.contains("sandbox not found") || e.contains("entity was not found")
}

/// Bridge `InteractiveExec`'s message-channel shape onto a plain
/// `AsyncRead`/`AsyncWrite` duplex half so `rmcp` can drive the other half
/// as a byte stream, unaware any of this exists.
async fn pump_loop(
    mut exec: InteractiveExec,
    driver_side: tokio::io::DuplexStream,
    stop: Arc<AtomicBool>,
) {
    let (mut driver_read, mut driver_write) = tokio::io::split(driver_side);
    let mut buf = [0u8; 8192];
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        tokio::select! {
            ev = exec.next_event() => {
                match ev {
                    Some(InteractiveEvent::Stdout(data)) => {
                        if driver_write.write_all(&data).await.is_err() {
                            break;
                        }
                    }
                    // `socat` has nothing to say on stderr in normal operation.
                    Some(InteractiveEvent::Stderr(data)) => {
                        tracing::debug!(bytes = data.len(), "cockpit: MCP relay stderr");
                    }
                    Some(InteractiveEvent::Exit(code)) => {
                        tracing::debug!(code, "cockpit: MCP relay process exited");
                        break;
                    }
                    None => break,
                }
            }
            n = driver_read.read(&mut buf) => {
                match n {
                    Ok(0) => break,
                    Ok(n) => {
                        if exec.write_stdin(buf[..n].to_vec()).await.is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        }
    }
}

async fn wait_socket_listen(os: &OpenShell, sandbox: &str, pump: &JoinHandle<()>) -> bool {
    for _ in 0..20 {
        if pump.is_finished() {
            return false;
        }
        match os
            .exec(sandbox, &readiness_probe(), Duration::from_secs(15))
            .await
        {
            Ok(out) if out.stdout.trim() == "LISTEN" => return true,
            Err(e) if sandbox_gone(&e.to_string()) => return false,
            _ => {}
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    false
}

/// Re-spawn `socat` + `serve_server` for each agent MCP connection until
/// `stop` is set or the sandbox disappears.
async fn relay_sessions(os: OpenShell, board: SharedBoard, sandbox: String, stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::Relaxed) {
        let exec = match os.exec_interactive_raw(&sandbox, relay_command()).await {
            Ok(e) => e,
            Err(e) => {
                let msg = e.to_string();
                if sandbox_gone(&msg)
                    || crate::openshell::is_expected_interactive_disconnect(&msg)
                {
                    tracing::info!(%sandbox, "cockpit: MCP relay stop — sandbox gone");
                    break;
                }
                tracing::warn!(%sandbox, "cockpit: spawn MCP relay (socat): {msg}");
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
        };

        let (mcp_side, driver_side) = tokio::io::duplex(64 * 1024);
        let pump_stop = Arc::new(AtomicBool::new(false));
        let pump = tokio::spawn(pump_loop(exec, driver_side, pump_stop.clone()));

        // Arm serve_server *before* advertising LISTEN. ensure() / mcp.json
        // inject race the agent onto the socket as soon as the inode exists;
        // if we wait for LISTEN first, the client's initialize can land on a
        // duplex with nobody reading yet.
        let operator = Operator::new(board.clone());
        let mut serve = tokio::spawn(async move {
            match rmcp::serve_server(operator, mcp_side).await {
                Ok(running) => {
                    let _ = running.waiting().await;
                }
                Err(e) => {
                    let msg = e.to_string();
                    // Stop/delete closes the exec pipe under serve_server —
                    // same class as openshell::is_expected_interactive_disconnect.
                    if crate::openshell::is_expected_interactive_disconnect(&msg)
                        || msg.to_ascii_lowercase().contains("connection closed")
                    {
                        tracing::debug!(
                            "cockpit: MCP serve_server ended on teardown: {msg}"
                        );
                    } else {
                        tracing::warn!(
                            "cockpit: MCP serve_server over exec pipe ended: {msg}"
                        );
                    }
                }
            }
        });

        if !wait_socket_listen(&os, &sandbox, &pump).await {
            pump_stop.store(true, Ordering::Relaxed);
            pump.abort();
            serve.abort();
            if stop.load(Ordering::Relaxed) {
                break;
            }
            // Socket not up yet — brief backoff then retry (create race, etc.).
            tokio::time::sleep(Duration::from_millis(500)).await;
            continue;
        }

        tracing::debug!(%sandbox, sock = AGENT_SOCK_PATH, "cockpit: MCP stdio session listening");

        let mut pump = pump;
        tokio::select! {
            _ = &mut serve => {
                pump_stop.store(true, Ordering::Relaxed);
                pump.abort();
            }
            _ = &mut pump => {
                serve.abort();
            }
        }

        if stop.load(Ordering::Relaxed) {
            break;
        }
        // Agent disconnected (one-shot socat exited) — loop for the next connect.
        tokio::task::yield_now().await;
    }
}

/// Poll until the sandbox-side socket is listening (or `dead` reports the
/// relay task is gone).
async fn wait_until_listen(os: &OpenShell, sandbox: &str, dead: impl Fn() -> bool) -> bool {
    for _ in 0..40 {
        if dead() {
            return false;
        }
        match os
            .exec(sandbox, &readiness_probe(), Duration::from_secs(15))
            .await
        {
            Ok(out) if out.stdout.trim() == "LISTEN" => return true,
            Err(e) if sandbox_gone(&e.to_string()) => return false,
            _ => {}
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    false
}

/// Ensure the cockpit MCP relay task is up for `sandbox`. Idempotent while
/// the spawned task lives — a warm task short-circuits with no extra probe
/// or log, since callers (attach, supervisor reconcile, mcp-cred) call this
/// on every tick. Serialized so those cannot triple-spawn.
///
/// A *fresh* spawn waits for a live LISTEN before returning — the caller's
/// mcp.json inject races the agent onto the socket immediately after, and a
/// task handle alone (without serve_server armed and the socket bound) was
/// the empty-tool-catalog race on Start.
pub async fn ensure_cockpit_mcp_tunnel(
    os: &OpenShell,
    board: &SharedBoard,
    sandbox: &str,
) -> Result<(), String> {
    let _guard = ensure_lock().lock().await;
    let sandbox = sandbox.trim();
    if sandbox.is_empty() {
        return Err("sandbox name required for MCP tunnel".into());
    }

    {
        let mut slot = tunnel_slot().lock();
        if let Some(state) = slot.as_mut() {
            if state.sandbox == sandbox && !state.handle.is_finished() {
                return Ok(());
            }
        }
    }
    stop_cockpit_mcp_tunnel_unlocked().await;

    let stop = Arc::new(AtomicBool::new(false));
    let os_c = os.clone();
    let board_c = board.clone();
    let sandbox_s = sandbox.to_string();
    let stop_c = stop.clone();
    let handle = tokio::spawn(async move {
        relay_sessions(os_c, board_c, sandbox_s, stop_c).await;
    });

    *tunnel_slot().lock() = Some(TunnelState {
        sandbox: sandbox.to_string(),
        stop,
        handle,
    });

    let ready = {
        let dead = || {
            tunnel_slot()
                .lock()
                .as_ref()
                .map(|s| s.handle.is_finished() || s.sandbox != sandbox)
                .unwrap_or(true)
        };
        wait_until_listen(os, sandbox, dead).await
    };

    if !ready {
        stop_cockpit_mcp_tunnel_unlocked().await;
        return Err(format!(
            "cockpit MCP relay did not bind {AGENT_SOCK_PATH} in sandbox `{sandbox}`"
        ));
    }

    tracing::info!(sandbox, sock = AGENT_SOCK_PATH, "cockpit: MCP stdio relay up");
    Ok(())
}

async fn stop_cockpit_mcp_tunnel_unlocked() {
    let prev = tunnel_slot().lock().take();
    let Some(state) = prev else {
        return;
    };
    state.stop.store(true, Ordering::Relaxed);
    state.handle.abort();
    tracing::info!(sandbox = %state.sandbox, "cockpit: MCP stdio relay stopped");
}

/// Stop the relay session (idempotent). The sandbox-side `socat` process
/// exits once its stdin (the gRPC-piped end) closes.
pub async fn stop_cockpit_mcp_tunnel(_os: &OpenShell) {
    let _guard = ensure_lock().lock().await;
    stop_cockpit_mcp_tunnel_unlocked().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relay_command_binds_configured_socket() {
        let cmd = relay_command();
        let script = cmd.last().expect("script");
        assert!(script.contains("socat UNIX-LISTEN:"));
        assert!(
            !script.contains("fork"),
            "one-shot listen so disconnect is visible: {script}"
        );
        assert!(script.contains(AGENT_SOCK_PATH));
        assert!(script.contains("rm -f"), "must clear a stale socket file: {script}");
    }

    #[test]
    fn readiness_probe_checks_socket_file() {
        let probe = readiness_probe();
        assert!(probe.contains("-S"));
        assert!(probe.contains(AGENT_SOCK_PATH));
    }

    #[test]
    fn sandbox_gone_matches_openshell_not_found() {
        assert!(sandbox_gone(
            "openshell get sandbox: code: 'Some requested entity was not found', message: \"sandbox not found\""
        ));
        assert!(!sandbox_gone("connect timed out"));
    }

    #[test]
    fn shell_single_quote_escapes_embedded_quotes() {
        assert_eq!(shell_single_quote("a'b"), "'a'\"'\"'b'");
    }
}
