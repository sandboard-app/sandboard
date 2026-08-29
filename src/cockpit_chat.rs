//! Host-mediated cockpit chat bridge.
//!
//! Authenticated browser prompts are forwarded into the Board-named cockpit sandbox
//! conversation and streamed back. Board `cockpit_session` stays authoritative for
//! environment / conversation_id / status — this module is a thin face: it
//! reads those fields, refuses when the seat is not ready, and never parks,
//! resumes, or stops the session.

use crate::model::CockpitSessionStatus;
use crate::store::SharedBoard;
use crate::supervisor::{parse_conversation_id, setup_agy_auth, shell_quote};

use axum::extract::State as AxState;
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use futures::stream::{self, StreamExt};
use serde::Deserialize;
use std::convert::Infallible;
use std::time::Duration;
use tokio::sync::{mpsc, Mutex};
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::StreamExt as TokioStreamExt;

/// Serialize chat turns so two browsers cannot fight over the same seat.
static TURN_LOCK: Mutex<()> = Mutex::const_new(());

const AGENT_PID: &str = "/tmp/agent.pid";
const WORKDIR: &str = "/sandbox/repo";

pub fn routes() -> Router<SharedBoard> {
    Router::new().route("/cockpit-chat", post(cockpit_chat))
}

#[derive(Debug, Deserialize)]
pub struct OpsChatReq {
    pub prompt: String,
}

#[derive(Debug)]
struct BridgeError {
    status: StatusCode,
    message: String,
}

impl BridgeError {
    fn bad(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: msg.into(),
        }
    }

    fn conflict(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::CONFLICT,
            message: msg.into(),
        }
    }
}

impl IntoResponse for BridgeError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(serde_json::json!({ "error": self.message })),
        )
            .into_response()
    }
}

/// Preconditions for a chat turn. Read-only over Board — no lifecycle writes.
fn ready_target(board: &SharedBoard) -> Result<(String, Option<String>, String), BridgeError> {
    let session = board
        .cockpit_session()
        .ok_or_else(|| BridgeError::conflict("no cockpit session"))?;
    match session.status {
        CockpitSessionStatus::Parked => {
            return Err(BridgeError::conflict("cockpit session is parked"));
        }
        CockpitSessionStatus::Running => {}
    }
    let environment = session
        .environment
        .filter(|e| !e.trim().is_empty())
        .ok_or_else(|| BridgeError::conflict("cockpit session has no environment yet"))?;
    let conversation_id = session.conversation_id.filter(|c| !c.trim().is_empty());
    let engine = board.resolve_cockpit_engine();
    Ok((environment, conversation_id, engine))
}

/// Foreground (not detached) agent turn — stdout is what we stream to the browser.
fn turn_script(
    engine: &str,
    prompt: &str,
    conversation_id: Option<&str>,
    agy_cloud_exports: &str,
    model: Option<&str>,
) -> Result<String, crate::engine::UnknownEngine> {
    let secs = 3600u64;
    // Resume only when the registry says this engine supports it — same gate as
    // the supervisor (claude never gets `--conversation` / `--resume`).
    let resume_id = conversation_id.filter(|_| crate::engine::supports_resume(engine));
    let cmd = crate::engine::command_line(
        engine,
        crate::engine::PromptEnv::Prompt,
        resume_id,
        model,
    )?;
    let conv_export = resume_id
        .map(|c| format!("export SANDBOARD_CONVERSATION={}\n", shell_quote(c)))
        .unwrap_or_default();
    let inference_exports = crate::engine::anthropic_inference_exports(engine);
    let hermes_inference_exports = crate::engine::hermes_inference_exports(engine);
    let agy_cloud = if engine.trim() == "agy" {
        format!(
            "{agy_cloud_exports}export PATH=/sandbox/.local/bin:$PATH\n\
             set -a\n\
             [ -f /sandbox/.gemini/antigravity-cli/sandboard-cloud.env ] && \
             . /sandbox/.gemini/antigravity-cli/sandboard-cloud.env\n\
             set +a\n"
        )
    } else {
        String::new()
    };
    let hermes_query_setup = if engine.trim() == "hermes" {
        format!(
            "printf '%s' \"$SANDBOARD_PROMPT\" > {}\n",
            crate::engine::HERMES_QUERY_FILE
        )
    } else {
        String::new()
    };
    Ok(format!(
        r#"set -e
export SANDBOARD_PROMPT={prompt}
{hermes_query_setup}{agy_cloud}{inference_exports}{hermes_inference_exports}{conv_export}cd {WORKDIR} 2>/dev/null || cd /
timeout --foreground {secs} {cmd}"#,
        prompt = shell_quote(prompt),
    ))
}

fn stop_live_agent_script() -> String {
    format!(
        r#"if [ -s {AGENT_PID} ]; then kill -TERM -"$(cat {AGENT_PID})" 2>/dev/null || true; sleep 0.5; fi"#
    )
}

async fn cockpit_chat(
    AxState(board): AxState<SharedBoard>,
    Json(req): Json<OpsChatReq>,
) -> Response {
    let prompt = req.prompt.trim().to_string();
    if prompt.is_empty() {
        return BridgeError::bad("prompt required").into_response();
    }

    let (environment, conversation_id, engine) = match ready_target(&board) {
        Ok(t) => t,
        Err(e) => return e.into_response(),
    };

    let (tx, rx) = mpsc::channel::<Result<Event, Infallible>>(64);
    let board2 = board.clone();
    let env = environment.clone();
    let cid = conversation_id.clone();
    let eng = engine.clone();

    tokio::spawn(async move {
        let _guard = TURN_LOCK.lock().await;
        if let Err(e) = run_turn(board2, &env, cid.as_deref(), &eng, &prompt, &tx).await {
            let _ = tx
                .send(Ok(Event::default().event("error").data(e)))
                .await;
        }
        let _ = tx
            .send(Ok(Event::default().event("done").data("{}")))
            .await;
    });

    let ready_data = serde_json::json!({
        "environment": environment,
        "conversation_id": conversation_id,
        "engine": engine,
    })
    .to_string();

    let hello = stream::once(async move {
        Ok::<_, Infallible>(Event::default().event("ready").data(ready_data))
    });

    let body = TokioStreamExt::map(ReceiverStream::new(rx), |msg| msg);
    let sse = Sse::new(StreamExt::chain(hello, body)).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("keep-alive"),
    );

    let mut res = sse.into_response();
    res.headers_mut().insert(
        header::HeaderName::from_static("x-accel-buffering"),
        HeaderValue::from_static("no"),
    );
    res.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-cache"),
    );
    res
}

async fn run_turn(
    board: SharedBoard,
    environment: &str,
    conversation_id: Option<&str>,
    engine: &str,
    prompt: &str,
    tx: &mpsc::Sender<Result<Event, Infallible>>,
) -> Result<(), String> {
    // Re-check Board after acquiring the turn lock — session may have parked.
    let (env_now, cid_now, engine_now) = match ready_target(&board) {
        Ok(t) => t,
        Err(e) => return Err(e.message),
    };
    if env_now != environment {
        return Err(format!(
            "cockpit session environment changed ({environment} → {env_now})"
        ));
    }
    let conversation_id = cid_now.as_deref().or(conversation_id);
    let engine = if engine_now.is_empty() {
        engine
    } else {
        engine_now.as_str()
    };

    let os = board.openshell_client();
    let timeout = Duration::from_secs(board.effective_agents().agent_timeout_secs.max(60))
        + Duration::from_secs(120);

    // Free the seat's detached print process so this turn injects into the
    // same conversation rather than racing a parallel agent. Does not touch
    // Board status — park/stop stay on /api/cockpit-session*.
    let _ = os
        .exec(
            environment,
            &stop_live_agent_script(),
            Duration::from_secs(30),
        )
        .await;

    if crate::engine::pre_start_auth(engine).map_err(|e| e.to_string())?
        == crate::engine::PreStartAuth::Agy
    {
        let _ = setup_agy_auth(&os, environment, &board).await;
    }

    let agy_exports = if engine.trim() == "agy" {
        crate::antigravity::cloud_env_exports(&board)
    } else {
        String::new()
    };
    let model = board.resolve_cockpit_model();
    let script = turn_script(
        engine,
        prompt,
        conversation_id,
        &agy_exports,
        model.as_deref(),
    )
        .map_err(|e| e.to_string())?;
    let board_lines = board.clone();
    let tx_lines = tx.clone();
    let result = os
        .exec_streaming(environment, &script, timeout, move |line| {
            if let Some(cid) = parse_conversation_id(line) {
                let _ = board_lines.update_cockpit_session(None, Some(cid));
            }
            let ev = Event::default().event("agent").data(line);
            // Non-blocking: drop if the client is slow rather than stalling the
            // sandbox read loop (same constraint as supervisor on_line).
            let _ = tx_lines.try_send(Ok(ev));
        })
        .await
        .map_err(|e| e.to_string())?;

    if !result.ok() {
        let detail = {
            let mut s = result.stderr.trim().to_string();
            if !result.stdout.trim().is_empty() {
                if !s.is_empty() {
                    s.push('\n');
                }
                s.push_str(result.stdout.trim());
            }
            if s.len() > 800 {
                s = s[s.len() - 800..].to_string();
            }
            s
        };
        return Err(format!("cockpit chat turn exited {}: {detail}", result.code));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::openshell::OpenShell;
    use crate::schema::Schema;
    use crate::store::Board;
    use axum::body::Body;
    use axum::http::Request;
    use std::sync::Arc;
    use tower_service::Service;

    fn board() -> SharedBoard {
        Arc::new(Board::new(
            Schema::default(),
            std::env::temp_dir().join(format!(
                "sandboard-test-cockpit-chat-{}.json",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            )),
        ))
    }

    #[test]
    fn turn_script_resumes_cursor_conversation() {
        let s = turn_script("cursor", "triage Needs You", Some("conv-abc"), "", None).unwrap();
        assert!(s.contains("--resume \"$SANDBOARD_CONVERSATION\""), "{s}");
        assert!(s.contains("SANDBOARD_CONVERSATION='conv-abc'"), "{s}");
        assert!(s.contains("SANDBOARD_PROMPT='triage Needs You'"), "{s}");
        assert!(s.contains("timeout --foreground"), "{s}");
        assert!(!s.contains("setsid nohup"), "chat turns are foreground: {s}");
    }

    #[test]
    fn turn_script_resumes_agy_conversation() {
        let s = turn_script(
            "agy",
            "hello",
            Some("cid-1"),
            "export GOOGLE_CLOUD_PROJECT='p'\n",
            None,
        )
        .unwrap();
        assert!(s.contains("GOOGLE_CLOUD_PROJECT='p'"), "{s}");
        assert!(s.contains("PATH=/sandbox/.local/bin:$PATH"), "{s}");
        assert!(
            s.contains(&format!("--model '{}'", crate::antigravity::DEFAULT_SEAT_MODEL)),
            "{s}"
        );
        assert!(s.contains("--conversation \"$SANDBOARD_CONVERSATION\""), "{s}");
        assert!(s.contains("-p \"$SANDBOARD_PROMPT\""), "{s}");
        let model_at = s.find("--model").expect("model");
        let p_at = s.find("-p \"$SANDBOARD_PROMPT\"").expect("-p");
        assert!(model_at < p_at, "{s}");
    }

    #[test]
    fn turn_script_materializes_hermes_query_file() {
        let s = turn_script("hermes", "quotes ' and $(not shell)", None, "", None).unwrap();
        assert!(
            s.contains("printf '%s' \"$SANDBOARD_PROMPT\" > /tmp/sandboard-hermes-query"),
            "{s}"
        );
        assert!(
            s.contains("hermes --yolo --accept-hooks --provider openrouter chat --query-file /tmp/sandboard-hermes-query"),
            "{s}"
        );
        assert!(
            s.contains(&format!(
                "SANDBOARD_PROMPT={}",
                shell_quote("quotes ' and $(not shell)")
            )),
            "{s}"
        );
    }

    #[test]
    fn turn_script_cursor_injects_resolved_model() {
        let s = turn_script("cursor", "hello", None, "", Some("gpt-5")).unwrap();
        assert!(s.contains("--model 'gpt-5'"), "{s}");
        let model_at = s.find("--model").expect("model");
        let prompt_at = s.find("\"$SANDBOARD_PROMPT\"").expect("prompt");
        assert!(model_at < prompt_at, "{s}");
    }

    #[test]
    fn turn_script_without_conversation_starts_fresh_in_seat() {
        let s = turn_script("cursor", "first prompt", None, "", None).unwrap();
        assert!(!s.contains("--resume"), "{s}");
        assert!(!s.contains("SANDBOARD_CONVERSATION="), "{s}");
        assert!(s.contains("SANDBOARD_PROMPT='first prompt'"), "{s}");
    }

    #[test]
    fn turn_script_shell_quotes_prompt_apostrophes() {
        let s = turn_script("cursor", "it's a test", None, "", None).unwrap();
        assert!(s.contains(r"it'\''s a test"), "{s}");
    }

    #[test]
    fn turn_script_rejects_unknown_engine() {
        let err = turn_script("nope", "hi", None, "", None).unwrap_err();
        assert!(err.to_string().contains("unknown agent engine"), "{err}");
    }

    #[test]
    fn turn_script_opencode_fresh_and_resume() {
        let fresh = turn_script("opencode", "hi", None, "", None).unwrap();
        assert!(
            fresh.contains("opencode run --format json --auto \"$SANDBOARD_PROMPT\""),
            "{fresh}"
        );
        assert!(!fresh.contains("--session"), "{fresh}");

        let resume = turn_script("opencode", "again", Some("ses_1"), "", None).unwrap();
        assert!(
            resume.contains("--session \"$SANDBOARD_CONVERSATION\""),
            "{resume}"
        );
        assert!(resume.contains("SANDBOARD_CONVERSATION='ses_1'"), "{resume}");
    }

    #[test]
    fn turn_script_claude_ignores_conversation_id() {
        let s = turn_script("claude", "hello", Some("cid"), "", None).unwrap();
        assert!(!s.contains("--conversation"), "{s}");
        assert!(!s.contains("--resume"), "{s}");
        assert!(!s.contains("SANDBOARD_CONVERSATION="), "{s}");
        assert!(
            s.contains(
                "claude --bare --strict-mcp-config --mcp-config /sandbox/.sandboard/mcp/claude_mcp.json -p \"$SANDBOARD_PROMPT\""
            ),
            "{s}"
        );
        assert!(
            s.contains("ANTHROPIC_BASE_URL=https://inference.local\n"),
            "{s}"
        );
    }

    #[test]
    fn ready_target_refuses_absent_parked_and_missing_environment() {
        let b = board();
        let err = ready_target(&b).expect_err("no session");
        assert!(err.message.contains("no cockpit session"), "{}", err.message);

        b.create_cockpit_session(None, None).expect("create");
        let err = ready_target(&b).expect_err("no environment");
        assert!(
            err.message.contains("no environment"),
            "{}",
            err.message
        );

        b.update_cockpit_session(Some("sandboard-cockpit".into()), None)
            .expect("env");
        b.park_cockpit_session().expect("park");
        let err = ready_target(&b).expect_err("parked");
        assert!(err.message.contains("parked"), "{}", err.message);

        b.resume_cockpit_session().expect("resume");
        let (env, cid, engine) = ready_target(&b).expect("running");
        assert_eq!(env, "sandboard-cockpit");
        assert!(cid.is_none());
        assert!(!engine.is_empty());
    }

    #[test]
    fn ready_target_passes_conversation_id_through() {
        let b = board();
        b.create_cockpit_session(Some("sandboard-cockpit".into()), Some("conv-9".into()))
            .expect("create");
        let (env, cid, _) = ready_target(&b).expect("ready");
        assert_eq!(env, "sandboard-cockpit");
        assert_eq!(cid.as_deref(), Some("conv-9"));
    }

    #[tokio::test]
    async fn cockpit_chat_route_refuses_without_session() {
        let b = board();
        let mut app = Router::new().nest("/api", routes()).with_state(b);
        let req = Request::builder()
            .method("POST")
            .uri("/api/cockpit-chat")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"prompt":"hello"}"#))
            .unwrap();
        let res = app.call(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::CONFLICT);
        let bytes = axum::body::to_bytes(res.into_body(), 1024).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(
            v["error"].as_str().unwrap_or("").contains("no cockpit session"),
            "{v}"
        );
    }

    #[tokio::test]
    async fn cockpit_chat_route_refuses_empty_prompt() {
        let b = board();
        b.create_cockpit_session(Some("sandboard-cockpit".into()), None)
            .expect("create");
        let mut app = Router::new().nest("/api", routes()).with_state(b);
        let req = Request::builder()
            .method("POST")
            .uri("/api/cockpit-chat")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"prompt":"  "}"#))
            .unwrap();
        let res = app.call(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        let bytes = axum::body::to_bytes(res.into_body(), 1024).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(
            v["error"].as_str().unwrap_or("").contains("prompt"),
            "{v}"
        );
    }

    #[tokio::test]
    async fn cockpit_chat_route_refuses_parked_session() {
        let b = board();
        b.create_cockpit_session(Some("sandboard-cockpit".into()), Some("c1".into()))
            .expect("create");
        b.park_cockpit_session().expect("park");
        let mut app = Router::new().nest("/api", routes()).with_state(b);
        let req = Request::builder()
            .method("POST")
            .uri("/api/cockpit-chat")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"prompt":"hello"}"#))
            .unwrap();
        let res = app.call(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::CONFLICT);
        let bytes = axum::body::to_bytes(res.into_body(), 1024).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(
            v["error"].as_str().unwrap_or("").contains("parked"),
            "{v}"
        );
    }

    #[tokio::test]
    async fn cockpit_chat_route_streams_agent_lines_when_running() {
        let path = std::env::temp_dir().join(format!(
            "sandboard-test-cockpit-chat-stream-{}.json",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut board = Board::new(Schema::default(), path);
        board.openshell = Some(OpenShell::mock(
            |args| {
                let joined = args.join(" ");
                if joined.contains("kill -TERM") {
                    return crate::openshell::Output {
                        code: 0,
                        stdout: String::new(),
                        stderr: String::new(),
                    };
                }
                crate::openshell::Output {
                    code: 0,
                    stdout: concat!(
                        r#"{"type":"assistant","session_id":"conv-from-stream","text":"pong"}"#,
                        "\n",
                        r#"{"type":"result","session_id":"conv-from-stream"}"#,
                        "\n",
                    )
                    .into(),
                    stderr: String::new(),
                }
            },
            Duration::from_secs(5),
        ));
        let b: SharedBoard = Arc::new(board);
        b.create_cockpit_session(Some("sandboard-cockpit".into()), Some("conv-old".into()))
            .expect("create");

        let mut app = Router::new().nest("/api", routes()).with_state(b.clone());
        let req = Request::builder()
            .method("POST")
            .uri("/api/cockpit-chat")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"prompt":"ping"}"#))
            .unwrap();
        let res = app.call(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let ct = res
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(ct.contains("text/event-stream"), "content-type={ct}");

        let bytes = axum::body::to_bytes(res.into_body(), 64 * 1024)
            .await
            .unwrap();
        let body = String::from_utf8_lossy(&bytes);
        assert!(
            body.contains("event:ready") || body.contains("event: ready"),
            "{body}"
        );
        assert!(body.contains("sandboard-cockpit"), "{body}");
        assert!(
            body.contains("event:agent")
                || body.contains("event: agent")
                || body.contains("pong"),
            "{body}"
        );
        assert!(
            body.contains("event:done") || body.contains("event: done"),
            "{body}"
        );

        let cid = b.cockpit_session().and_then(|s| s.conversation_id);
        assert_eq!(cid.as_deref(), Some("conv-from-stream"));
    }

    #[tokio::test]
    async fn cockpit_chat_does_not_create_or_stop_session() {
        let path = std::env::temp_dir().join(format!(
            "sandboard-test-cockpit-chat-lifecycle-{}.json",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut board = Board::new(Schema::default(), path);
        board.openshell = Some(OpenShell::mock(
            |_| crate::openshell::Output {
                code: 0,
                stdout: "{\"type\":\"assistant\"}\n".into(),
                stderr: String::new(),
            },
            Duration::from_secs(5),
        ));
        let b: SharedBoard = Arc::new(board);
        b.create_cockpit_session(Some("sandboard-cockpit".into()), None)
            .expect("create");

        let mut app = Router::new().nest("/api", routes()).with_state(b.clone());
        let req = Request::builder()
            .method("POST")
            .uri("/api/cockpit-chat")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"prompt":"stay running"}"#))
            .unwrap();
        let res = app.call(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let _ = axum::body::to_bytes(res.into_body(), 64 * 1024)
            .await
            .unwrap();

        let session = b.cockpit_session().expect("session must remain");
        assert_eq!(session.status, CockpitSessionStatus::Running);
        assert_eq!(session.environment.as_deref(), Some("sandboard-cockpit"));
    }
}
