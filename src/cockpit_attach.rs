//! Host-mediated cockpit attach (real interactive TTY).
//!
//! Cockpit opens an authenticated WebSocket; we open
//! `ExecSandboxInteractive` into the Board-named cockpit sandbox and relay bytes.
//! Board `cockpit_session` stays authoritative — this module never parks, resumes,
//! or stops the session. Host `openshell sandbox connect` remains a manual CLI
//! path; sandboard does not shell out to launch editors.
//!
//! Attach launches the **Cockpit sandbox-spec engine** (Settings → OpenShell →
//! Sandbox specs), not a bare shell. Cursor still uses interactive `agent`
//! with optional `--resume`; OpenCode / Claude / agy get their own TUI argv.

use crate::model::CockpitSessionStatus;
use crate::openshell::InteractiveEvent;
use crate::store::SharedBoard;
use crate::supervisor::{
    cockpit_briefing, setup_agy_auth, shell_quote, stop_agent, wait_until_sandbox_ready,
};
use crate::ws::{read_frame, write_frame, WsFrame};

use axum::extract::{Request, State as AxState};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use axum_extra::extract::cookie::CookieJar;
use hyper::upgrade;
use hyper_util::rt::TokioIo;
use serde::Deserialize;
use serde_json::json;
use std::time::Duration;

/// Where the cockpit agent works inside the sandbox (same as supervisor).
const WORKDIR: &str = "/sandbox/repo";

pub fn routes() -> Router<SharedBoard> {
    Router::new().route("/cockpit-attach", get(cockpit_attach_ws))
}

#[derive(Debug)]
struct AttachError {
    status: StatusCode,
    message: String,
}

impl AttachError {
    fn conflict(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::CONFLICT,
            message: msg.into(),
        }
    }
}

impl IntoResponse for AttachError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(json!({ "error": self.message })),
        )
            .into_response()
    }
}

/// Read-only Board gate — Running + named environment required.
fn ready_environment(board: &SharedBoard) -> Result<String, AttachError> {
    let session = board
        .cockpit_session()
        .ok_or_else(|| AttachError::conflict("no cockpit session"))?;
    match session.status {
        CockpitSessionStatus::Parked => {
            return Err(AttachError::conflict(
                "cockpit session is parked — Stop then Start again",
            ));
        }
        CockpitSessionStatus::Running => {}
    }
    session
        .environment
        .filter(|e| !e.trim().is_empty())
        .ok_or_else(|| AttachError::conflict("cockpit session has no environment yet"))
}

/// Whether a Board `conversation_id` can be resumed on this engine.
///
/// Cursor ids are UUID-shaped; OpenCode uses `ses_*`; Hermes ids are opaque
/// timestamped session handles. Mixing them after a profile engine switch would
/// hang or error — start fresh instead.
pub(crate) fn conversation_usable(engine: &str, id: &str) -> bool {
    let id = id.trim();
    if id.is_empty() {
        return false;
    }
    match engine.trim() {
        "cursor" => !id.starts_with("ses_"),
        "opencode" => id.starts_with("ses_"),
        "agy" => true,
        "hermes" => true,
        "claude" => false,
        _ => false,
    }
}

/// Interactive agent argv for Cockpit attach (profile engine).
///
/// Login shell so PATH finds the binary; `exec` replaces the shell. Cursor
/// keeps human-in-the-loop flags (no `--force`). Anthropic-shaped engines get
/// `inference.local` exports from [`crate::engine::anthropic_inference_exports`].
pub(crate) fn attach_agent_command(
    engine: &str,
    conversation_id: Option<&str>,
    initial_prompt: Option<&str>,
    // Prepended shell exports (agy: antigravity GCP project over Vertex).
    extra_exports: &str,
    model: Option<&str>,
) -> Vec<String> {
    let engine = engine.trim();
    let cid = conversation_id
        .map(str::trim)
        .filter(|s| conversation_usable(engine, s));
    let prompt = initial_prompt.map(str::trim).filter(|s| !s.is_empty());
    let inference = crate::engine::anthropic_inference_exports(engine);
    let hermes_inference = crate::engine::hermes_inference_exports(engine);
    let agy_cloud = if engine == "agy" {
        // Prefer a seat-local agy when the image bake lags (binary integrity
        // pins /usr/local/bin/agy to the image hash).
        "export PATH=/sandbox/.local/bin:$PATH\n\
         set -a\n\
         [ -f /sandbox/.gemini/antigravity-cli/sandboard-cloud.env ] && \
         . /sandbox/.gemini/antigravity-cli/sandboard-cloud.env\n\
         set +a\n"
    } else {
        ""
    };

    let agent = match engine {
        "opencode" => {
            let mut cmd = String::from("opencode");
            if let Some(argv) = crate::engine::cli_model_argv("opencode", model) {
                cmd.push(' ');
                cmd.push_str(&argv);
            }
            if let Some(id) = cid {
                // Interactive TUI: continue a prior session when we have one.
                cmd.push_str(" --session ");
                cmd.push_str(&shell_quote(id));
            }
            cmd
        }
        // `--bare` skips MCP auto-discovery; point at the injected seat config.
        "claude" => format!(
            "claude --bare --strict-mcp-config --mcp-config {}",
            crate::cockpit_mcp::COCKPIT_CLAUDE_MCP_CONFIG
        ),
        "agy" => {
            let mut cmd = String::from("agy --dangerously-skip-permissions");
            if let Some(argv) = crate::engine::cli_model_argv("agy", model) {
                cmd.push(' ');
                cmd.push_str(&argv);
            }
            if let Some(id) = cid {
                cmd.push_str(" --conversation ");
                cmd.push_str(&shell_quote(id));
            }
            if let Some(p) = prompt {
                cmd.push_str(" -p ");
                cmd.push_str(&shell_quote(p));
            }
            cmd
        }
        "hermes" => {
            let mut cmd = String::from("hermes --cli --provider openrouter");
            if let Some(argv) = crate::engine::cli_model_argv("hermes", model) {
                cmd.push(' ');
                cmd.push_str(&argv);
            }
            if let Some(id) = cid {
                cmd.push_str(" --resume ");
                cmd.push_str(&shell_quote(id));
            }
            cmd
        }
        // cursor (default): human-in-the-loop Cursor Agent CLI.
        _ => {
            let mut cmd = String::from("agent --trust --approve-mcps --sandbox disabled");
            if let Some(argv) = crate::engine::cli_model_argv("cursor", model) {
                cmd.push(' ');
                cmd.push_str(&argv);
            }
            if let Some(id) = cid {
                cmd.push_str(" --resume ");
                cmd.push_str(&shell_quote(id));
            }
            if let Some(p) = prompt {
                cmd.push(' ');
                cmd.push_str(&shell_quote(p));
            }
            cmd
        }
    };

    let script = format!(
        "{extra_exports}{agy_cloud}{inference}{hermes_inference}cd {WORKDIR} 2>/dev/null || cd /sandbox; exec {agent}"
    );
    vec!["bash".into(), "-lc".into(), script]
}

fn session_conversation_id(board: &SharedBoard) -> Option<String> {
    board
        .cockpit_session()
        .and_then(|s| s.conversation_id)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Mint a chat id in the sandbox when the Board has none, and persist it.
pub(crate) fn parse_create_chat_id(stdout: &str) -> Option<String> {
    stdout
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.contains(' '))
        .map(|s| s.to_string())
}

/// Shell that mints a Cursor chat id without deadlocking.
///
/// `agent create-chat` prints an id then keeps running. Piped to `head` it often
/// fully-buffers stdout (no TTY) so `head -n1` waits forever. Write to a file,
/// poll for the first token line, then kill the process.
pub(crate) fn create_chat_script() -> String {
    format!(
        r#"cd {WORKDIR} 2>/dev/null || cd /sandbox
out=$(mktemp)
agent create-chat >"$out" 2>/dev/null &
pid=$!
id=""
for _ in $(seq 1 80); do
  if [ -s "$out" ]; then
    id=$(tr -d '\r' <"$out" | awk 'NF && $0 !~ / / {{print; exit}}')
    if [ -n "$id" ]; then
      kill "$pid" 2>/dev/null || true
      wait "$pid" 2>/dev/null || true
      printf '%s\n' "$id"
      rm -f "$out"
      exit 0
    fi
  fi
  kill -0 "$pid" 2>/dev/null || break
  sleep 0.25
done
kill "$pid" 2>/dev/null || true
wait "$pid" 2>/dev/null || true
if [ -s "$out" ]; then
  tr -d '\r' <"$out" | awk 'NF && $0 !~ / / {{print; exit}}'
fi
rm -f "$out"
exit 1
"#
    )
}

/// `(conversation_id, freshly_minted)`.
///
/// Cursor can mint via `agent create-chat`. Other engines either reuse a
/// compatible Board id or start with none (TUI creates its own session).
async fn ensure_conversation_id(
    board: &SharedBoard,
    os: &crate::openshell::OpenShell,
    environment: &str,
    engine: &str,
) -> Option<(String, bool)> {
    if let Some(id) = session_conversation_id(board) {
        if conversation_usable(engine, &id) {
            return Some((id, false));
        }
        // Engine switched (e.g. cursor → opencode): drop the stale id.
        if let Err(e) = board.update_cockpit_session(None, Some(String::new())) {
            tracing::warn!("cockpit-attach clear stale conversation_id: {e}");
        }
    }
    if engine.trim() != "cursor" {
        return None;
    }
    let out = match os
        .exec(environment, &create_chat_script(), Duration::from_secs(30))
        .await
    {
        Ok(o) => o,
        Err(e) => {
            tracing::warn!("cockpit-attach create-chat failed: {e}");
            return None;
        }
    };
    let Some(id) = parse_create_chat_id(&out.stdout) else {
        tracing::warn!(
            "cockpit-attach create-chat: no id in stdout (exit {}): {:?}",
            out.code,
            out.stdout.trim()
        );
        return None;
    };
    if let Err(e) = board.update_cockpit_session(None, Some(id.clone())) {
        tracing::warn!("cockpit-attach persist conversation_id: {e}");
    }
    Some((id, true))
}

#[derive(Debug, Deserialize)]
struct ClientCtrl {
    #[serde(rename = "type")]
    kind: String,
    cols: Option<u32>,
    rows: Option<u32>,
}

/// Authenticated WebSocket → interactive profile engine in the Board cockpit sandbox.
async fn cockpit_attach_ws(
    AxState(board): AxState<SharedBoard>,
    headers: HeaderMap,
    mut req: Request,
) -> Response {
    let environment = match ready_environment(&board) {
        Ok(e) => e,
        Err(e) => return e.into_response(),
    };

    // Capture login before upgrade — MCP inject runs after the socket is up so
    // the browser is not stuck on "connecting…" during sandbox uploads.
    let jar = CookieJar::from_headers(req.headers());
    let login = crate::auth::session_user_from_jar(&board, &jar).map(|u| u.login);

    let key = match headers.get("sec-websocket-key").and_then(|v| v.to_str().ok()) {
        Some(k) => k,
        None => {
            return (StatusCode::BAD_REQUEST, "Missing Sec-WebSocket-Key").into_response();
        }
    };
    let accept_key = crate::ws::compute_ws_accept(key);
    let on_upgrade = upgrade::on(&mut req);

    tokio::spawn(async move {
        match on_upgrade.await {
            Ok(upgraded) => {
                let io = TokioIo::new(upgraded);
                if let Err(e) = handle_attach(io, board, environment, login).await {
                    tracing::warn!("cockpit-attach session ended: {e}");
                }
            }
            Err(e) => tracing::debug!("cockpit-attach upgrade error: {e}"),
        }
    });

    Response::builder()
        .status(StatusCode::SWITCHING_PROTOCOLS)
        .header(header::UPGRADE, "websocket")
        .header(header::CONNECTION, "Upgrade")
        .header("sec-websocket-accept", accept_key)
        .body(axum::body::Body::empty())
        .unwrap_or_else(|_| {
            (StatusCode::INTERNAL_SERVER_ERROR, "Response build error").into_response()
        })
}

async fn handle_attach<S>(
    stream: S,
    board: SharedBoard,
    environment: String,
    login: Option<String>,
) -> Result<(), String>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (mut reader, mut writer) = tokio::io::split(stream);
    let os = board.openshell_client();

    let engine = board.resolve_cockpit_engine();

    // Board may publish `environment` right as Ready settles, or attach can
    // reconnect while OpenShell is still bouncing the relay after create.
    // Wait here so we do not spam exec with "sandbox is not ready".
    wait_until_sandbox_ready(&os, &environment)
        .await
        .map_err(|e| e.to_string())?;

    if let Err(e) =
        crate::cockpit_mcp_tunnel::ensure_cockpit_mcp_tunnel(&os, &board, &environment).await
    {
        tracing::warn!("cockpit-attach MCP tunnel: {e}");
    }

    if engine.trim() == "agy" {
        if let Err(e) = setup_agy_auth(&os, &environment, &board).await {
            tracing::warn!("cockpit-attach agy auth: {e}");
        }
    }

    // Free leftover detached / hung create-chat / prior interactive attach so
    // the new TTY is uncontested. `stop_agent` only knows the supervisor pidfile.
    stop_agent(&os, &environment).await;
    let _ = os
        .exec(
            &environment,
            "pkill -f '/usr/local/bin/agent' 2>/dev/null || pkill -f 'cursor-agent' 2>/dev/null || pkill -f '/usr/local/bin/opencode' 2>/dev/null || pkill -f '/opt/opencode/bin/opencode' 2>/dev/null || pkill -f 'claude --bare' 2>/dev/null || pkill -f '/usr/local/bin/agy' 2>/dev/null || pkill -f '/usr/local/bin/hermes' 2>/dev/null || pkill -f '/opt/hermes/.venv/bin/hermes' 2>/dev/null || true",
            Duration::from_secs(10),
        )
        .await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Refresh MCP inject from the browser session when present (ties tools to
    // the human). Supervisor already injected `cockpit` fallback on sandbox ready.
    if let Some(sub) = login.as_deref() {
        if let Err(e) =
            crate::cockpit_mcp::provision_cockpit_mcp(&board, &os, &environment, sub).await
        {
            tracing::warn!("cockpit-attach MCP provision: {e}");
        }
    }

    let ensured = ensure_conversation_id(&board, &os, &environment, &engine).await;
    let (conversation_id, fresh) = match &ensured {
        Some((id, fresh)) => (Some(id.as_str()), *fresh),
        None => (None, false),
    };
    // Cursor fresh chats get the briefing as the initial prompt; other engines
    // open their TUI without a forced first message.
    let briefing = if fresh && engine == "cursor" {
        let resolved = board.resolve_cockpit_sandbox_create();
        Some(cockpit_briefing(resolved.prompt.as_deref()))
    } else {
        None
    };
    let agy_exports = if engine.trim() == "agy" {
        crate::antigravity::cloud_env_exports(&board)
    } else {
        String::new()
    };
    let command = attach_agent_command(
        &engine,
        conversation_id,
        briefing.as_deref(),
        &agy_exports,
        board.resolve_cockpit_model().as_deref(),
    );

    // Initial size; client sends resize ASAP after ready.
    let mut session = os
        .exec_interactive(&environment, command, 80, 24)
        .await
        .map_err(|e| e.to_string())?;

    write_frame(
        &mut writer,
        WsFrame::Text(
            json!({
                "type": "ready",
                "environment": environment,
                "engine": engine,
                "resumed": conversation_id.is_some() && !fresh,
                "conversation_id": conversation_id,
            })
            .to_string(),
        ),
    )
    .await
    .map_err(|e| e.to_string())?;

    loop {
        tokio::select! {
            frame = read_frame(&mut reader) => {
                match frame {
                    Ok(Some(WsFrame::Binary(data))) => {
                        if !data.is_empty() {
                            session.write_stdin(data).await.map_err(|e| e.to_string())?;
                        }
                    }
                    Ok(Some(WsFrame::Text(text))) => {
                        if let Ok(ctrl) = serde_json::from_str::<ClientCtrl>(&text) {
                            match ctrl.kind.as_str() {
                                "resize" => {
                                    let cols = ctrl.cols.unwrap_or(80).max(1);
                                    let rows = ctrl.rows.unwrap_or(24).max(1);
                                    session.resize(cols, rows).await.map_err(|e| e.to_string())?;
                                }
                                "ping" => {
                                    write_frame(
                                        &mut writer,
                                        WsFrame::Text(r#"{"type":"pong"}"#.into()),
                                    )
                                    .await
                                    .map_err(|e| e.to_string())?;
                                }
                                _ => {}
                            }
                        }
                    }
                    Ok(Some(WsFrame::Ping(data))) => {
                        write_frame(&mut writer, WsFrame::Pong(data))
                            .await
                            .map_err(|e| e.to_string())?;
                    }
                    Ok(Some(WsFrame::Pong(_))) => {}
                    Ok(Some(WsFrame::Close)) | Ok(None) | Err(_) => break,
                }
            }
            ev = session.next_event() => {
                match ev {
                    Some(InteractiveEvent::Stdout(data)) | Some(InteractiveEvent::Stderr(data)) => {
                        if write_frame(&mut writer, WsFrame::Binary(data)).await.is_err() {
                            break;
                        }
                    }
                    Some(InteractiveEvent::Exit(code)) => {
                        let _ = write_frame(
                            &mut writer,
                            WsFrame::Text(json!({ "type": "exit", "code": code }).to_string()),
                        )
                        .await;
                        let _ = write_frame(&mut writer, WsFrame::Close).await;
                        break;
                    }
                    None => break,
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::Schema;
    use crate::store::Board;
    use axum::body::Body;
    use axum::http::Request;
    use std::sync::Arc;
    use tower_service::Service;

    fn board() -> SharedBoard {
        Arc::new(
            Board::new(
                Schema::default(),
                std::env::temp_dir().join(format!(
                    "sandboard-cockpit-attach-test-{}-{}.json",
                    std::process::id(),
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_nanos()
                )),
            )
            .with_buffer_capacity(8),
        )
    }

    #[test]
    fn ready_environment_requires_running_named_session() {
        let b = board();
        assert!(ready_environment(&b).is_err());

        let _ = b
            .create_cockpit_session(Some("sandboard-cockpit".into()), None)
            .expect("create");
        assert_eq!(ready_environment(&b).expect("env"), "sandboard-cockpit");

        let _ = b.park_cockpit_session().expect("park");
        let err = ready_environment(&b).unwrap_err();
        assert_eq!(err.status, StatusCode::CONFLICT);
        assert!(err.message.contains("parked"));
    }

    #[test]
    fn attach_agent_command_cursor_injects_resolved_model() {
        let cmd = attach_agent_command(
            "cursor",
            None,
            Some("be the cockpit"),
            "",
            Some("gpt-5"),
        );
        let script = &cmd[2];
        assert!(
            script.contains("exec agent --trust --approve-mcps --sandbox disabled --model 'gpt-5'"),
            "{script}"
        );
        assert!(script.contains("'be the cockpit'"), "{script}");
    }

    #[test]
    fn attach_agent_command_cursor_omits_model_when_unset() {
        let cmd = attach_agent_command("cursor", None, Some("be the cockpit"), "", None);
        let script = &cmd[2];
        assert!(!script.contains("--model"), "{script}");
    }

    #[test]
    fn attach_agent_command_cold_start() {
        let cmd = attach_agent_command("cursor", None, Some("be the cockpit"), "", None);
        assert_eq!(cmd[0], "bash");
        assert_eq!(cmd[1], "-lc");
        let script = &cmd[2];
        assert!(
            script.contains("exec agent --trust --approve-mcps --sandbox disabled"),
            "{script}"
        );
        assert!(
            !script.contains("--force"),
            "Cockpit must not enable run-everything: {script}"
        );
        assert!(!script.contains("--resume"), "{script}");
        assert!(script.contains("'be the cockpit'"), "{script}");
        assert!(script.contains(WORKDIR), "{script}");
    }

    #[test]
    fn attach_agent_command_resumes_board_conversation() {
        let cmd = attach_agent_command(
            "cursor",
            Some("22096329-228f-47a1-a16f-cbde6da8fe5b"),
            None,
            "",
            None,
        );
        let script = &cmd[2];
        assert!(
            script.contains("--resume '22096329-228f-47a1-a16f-cbde6da8fe5b'"),
            "{script}"
        );
        assert!(
            script.contains("exec agent --trust --approve-mcps --sandbox disabled"),
            "{script}"
        );
        assert!(
            !script.contains("--force"),
            "Cockpit must not enable run-everything: {script}"
        );
        assert!(!script.contains("be the cockpit"), "{script}");
    }

    #[test]
    fn attach_agent_command_fresh_chat_seeds_briefing() {
        let cmd = attach_agent_command("cursor", Some("new-chat-id"), Some("hello seat"), "", None);
        let script = &cmd[2];
        assert!(script.contains("--resume 'new-chat-id'"), "{script}");
        assert!(script.contains("'hello seat'"), "{script}");
    }

    #[test]
    fn attach_agent_command_ignores_blank_conversation() {
        let cmd = attach_agent_command("cursor", Some("  "), None, "", None);
        assert!(!cmd[2].contains("--resume"), "{}", cmd[2]);
    }

    #[test]
    fn attach_agent_command_opencode_uses_inference_local() {
        let cmd = attach_agent_command("opencode", None, None, "", None);
        let script = &cmd[2];
        assert!(script.contains("exec opencode"), "{script}");
        assert!(
            script.contains("ANTHROPIC_BASE_URL=https://inference.local/v1"),
            "{script}"
        );
        assert!(!script.contains("agent --trust"), "{script}");
    }

    #[test]
    fn attach_agent_command_opencode_injects_resolved_model() {
        let cmd = attach_agent_command(
            "opencode",
            None,
            None,
            "",
            Some("openrouter/deepseek/deepseek-v4-flash-0731"),
        );
        let script = &cmd[2];
        assert!(
            script.contains("exec opencode --model 'openrouter/deepseek/deepseek-v4-flash-0731'"),
            "{script}"
        );
    }

    #[test]
    fn attach_agent_command_hermes_uses_classic_cli_and_openrouter() {
        let cmd = attach_agent_command("hermes", None, None, "", None);
        let script = &cmd[2];
        assert!(script.contains("exec hermes --cli --provider openrouter"), "{script}");
        assert!(!script.contains("--yolo"), "interactive attach should not be yolo: {script}");
        assert!(!script.contains("CUSTOM_BASE_URL"), "{script}");
        assert!(!script.contains("OPENAI_BASE_URL"), "{script}");
    }

    #[test]
    fn hermes_attach_accepts_hermes_session_ids() {
        assert!(conversation_usable("hermes", "20260828_120000_a1b2c3"));
        assert!(!conversation_usable("hermes", ""));
        let cmd = attach_agent_command(
            "hermes",
            Some("20260828_120000_a1b2c3"),
            None,
            "",
            Some("gpt-5.6-luna"),
        );
        let script = &cmd[2];
        assert!(script.contains("--resume '20260828_120000_a1b2c3'"), "{script}");
        assert!(script.contains("--model 'gpt-5.6-luna'"), "{script}");
    }

    #[test]
    fn attach_agent_command_agy_launches_tui() {
        let cmd = attach_agent_command(
            "agy",
            None,
            None,
            "export GOOGLE_CLOUD_PROJECT='shanemcd-rh'\n",
            None,
        );
        let script = &cmd[2];
        assert!(script.contains("exec agy"), "{script}");
        assert!(
            script.contains("--dangerously-skip-permissions"),
            "{script}"
        );
        assert!(
            script.contains(&format!("--model '{}'", crate::antigravity::DEFAULT_SEAT_MODEL))
                || script.contains(&format!("--model {}", crate::antigravity::DEFAULT_SEAT_MODEL)),
            "{script}"
        );
        assert!(script.contains("PATH=/sandbox/.local/bin:$PATH"), "{script}");
        assert!(
            script.contains("GOOGLE_CLOUD_PROJECT='shanemcd-rh'"),
            "{script}"
        );
        assert!(script.contains("sandboard-cloud.env"), "{script}");
    }

    #[test]
    fn attach_agent_command_claude_loads_injected_mcp() {
        let cmd = attach_agent_command("claude", None, None, "", None);
        let script = &cmd[2];
        assert!(
            script.contains("exec claude --bare --strict-mcp-config --mcp-config"),
            "{script}"
        );
        assert!(
            script.contains(crate::cockpit_mcp::COCKPIT_CLAUDE_MCP_CONFIG),
            "{script}"
        );
        assert!(
            script.contains("ANTHROPIC_BASE_URL=https://inference.local\n"),
            "{script}"
        );
    }

    #[test]
    fn attach_ignores_cursor_id_on_opencode() {
        assert!(!conversation_usable(
            "opencode",
            "22096329-228f-47a1-a16f-cbde6da8fe5b"
        ));
        assert!(conversation_usable("opencode", "ses_abc"));
        let cmd = attach_agent_command(
            "opencode",
            Some("22096329-228f-47a1-a16f-cbde6da8fe5b"),
            None,
            "",
            None,
        );
        assert!(
            !cmd[2].contains("--session"),
            "stale cursor id must not resume: {}",
            cmd[2]
        );
    }

    #[test]
    fn parse_create_chat_id_takes_first_token_line() {
        assert_eq!(
            parse_create_chat_id("22096329-228f-47a1-a16f-cbde6da8fe5b\n").as_deref(),
            Some("22096329-228f-47a1-a16f-cbde6da8fe5b")
        );
        assert_eq!(
            parse_create_chat_id("Created chat\nabc-123\n").as_deref(),
            Some("abc-123")
        );
        assert_eq!(parse_create_chat_id("hello world\n"), None);
    }

    #[test]
    fn create_chat_script_polls_file_and_kills_hanging_agent() {
        let s = create_chat_script();
        assert!(s.contains("agent create-chat"), "{s}");
        assert!(s.contains("mktemp"), "{s}");
        assert!(s.contains("kill \"$pid\""), "{s}");
        assert!(
            !s.contains("| head"),
            "piped head deadlocks when create-chat fully-buffers: {s}"
        );
    }

    #[tokio::test]
    async fn cockpit_attach_route_refuses_without_session() {
        let b = board();
        let mut app = Router::new().nest("/api", routes()).with_state(b);
        let req = Request::builder()
            .method("GET")
            .uri("/api/cockpit-attach")
            .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
            .header("upgrade", "websocket")
            .header("connection", "Upgrade")
            .body(Body::empty())
            .unwrap();
        let res = app.call(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::CONFLICT);
    }
}
