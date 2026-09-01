//! Explicit agent-engine registry.
//!
//! Launch argv, resume support, conversation-id JSON pointers, and optional
//! pre-start auth live in one table. Unknown engine ids fail loudly — they must
//! never fall through to a silent `claude` default.

use std::fmt;

/// How a parked conversation/session id is passed on the CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumeFlag {
    /// `agy … --conversation "$SANDBOARD_CONVERSATION" …`
    Conversation,
    /// Cursor Agent / Hermes CLI: `… --resume "$SANDBOARD_CONVERSATION" …`
    Resume,
    /// OpenCode CLI: `opencode run … --session "$SANDBOARD_CONVERSATION" …`
    Session,
}

impl ResumeFlag {
    fn argv(self) -> &'static str {
        match self {
            Self::Conversation => "--conversation \"$SANDBOARD_CONVERSATION\"",
            Self::Resume => "--resume \"$SANDBOARD_CONVERSATION\"",
            Self::Session => "--session \"$SANDBOARD_CONVERSATION\"",
        }
    }
}

/// Optional sandbox prep before the engine binary runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreStartAuth {
    None,
    /// Write Gemini/agy settings (+ optional host oauth token) into the sandbox.
    Agy,
}

/// Where the prompt / briefing env var is placed relative to fixed flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PromptStyle {
    /// `-p "$VAR"` after fixed flags (and resume, if any).
    FlagP,
    /// Trailing positional `"$VAR"` (Cursor Agent CLI).
    Positional,
    /// Hermes' non-interactive `chat --query-file` form.
    QueryFile,
}

/// One registered engine.
#[derive(Debug, Clone, Copy)]
pub struct Engine {
    pub id: &'static str,
    /// Binary + fixed flags, excluding resume, model, and prompt.
    prefix: &'static str,
    /// Fixed flags after `--model` and before resume/prompt (agy print-timeout).
    post_model: &'static str,
    /// Flags after the prompt (claude puts format/permission flags here).
    trailing: &'static str,
    prompt: PromptStyle,
    pub resume: Option<ResumeFlag>,
    /// JSON pointer paths walked by [`conversation_id_pointers`] / parsers.
    pub conversation_id_pointers: &'static [&'static str],
    pub pre_start_auth: PreStartAuth,
}

const AGY_CONV_KEYS: &[&str] = &[
    "/conversation_id",
    "/step_update/conversation_id",
    "/result/conversation_id",
    "/message/conversation_id",
];

const CURSOR_SESSION_KEYS: &[&str] = &["/session_id", "/result/session_id"];

/// OpenCode `--format json` lines use camelCase `sessionID` (see their run stream).
const OPENCODE_SESSION_KEYS: &[&str] = &["/sessionID", "/part/sessionID"];

/// Known engines. Order is display-stable; lookup is by `id`.
pub const ENGINES: &[Engine] = &[
    Engine {
        id: "cursor",
        // --approve-mcps: Cursor 2026.08+ leaves project mcp.json servers as
        // "needs approval" / unloaded unless this flag (or `agent mcp enable`)
        // runs. Cockpit attach already passes it; print/headless must too or
        // GetMcpTools returns "MCP server sandboard not found" while the socat
        // relay is healthy.
        prefix: "agent -p --force --trust --approve-mcps --sandbox disabled --output-format stream-json",
        post_model: "",
        trailing: "",
        prompt: PromptStyle::Positional,
        resume: Some(ResumeFlag::Resume),
        conversation_id_pointers: CURSOR_SESSION_KEYS,
        pre_start_auth: PreStartAuth::None,
    },
    Engine {
        id: "agy",
        // `--model` is injected from resolved card/spec/default — before `-p`
        // (FlagP appends `-p …` last). Default: [`crate::antigravity::DEFAULT_SEAT_MODEL`].
        prefix: "agy --dangerously-skip-permissions",
        post_model: "--print-timeout 24h --output-format stream-json",
        trailing: "",
        prompt: PromptStyle::FlagP,
        resume: Some(ResumeFlag::Conversation),
        conversation_id_pointers: AGY_CONV_KEYS,
        pre_start_auth: PreStartAuth::Agy,
    },
    Engine {
        id: "claude",
        // `--bare` skips Claude Code's OAuth and MCP auto-discovery. Auth is
        // OpenShell inference.local (see [`anthropic_inference_env`]); MCP is
        // the injected cockpit/seat file via `--mcp-config`.
        prefix: "claude --bare --strict-mcp-config --mcp-config /sandbox/.sandboard/mcp/claude_mcp.json",
        post_model: "",
        trailing: "--output-format stream-json --verbose --permission-mode bypassPermissions",
        prompt: PromptStyle::FlagP,
        resume: None,
        conversation_id_pointers: &[],
        pre_start_auth: PreStartAuth::None,
    },
    Engine {
        id: "opencode",
        // Headless one-shot: JSONL events on stdout, auto-approve tool perms
        // (sandbox is already the containment boundary). Prompt is positional.
        prefix: "opencode run --format json --auto",
        post_model: "",
        trailing: "",
        prompt: PromptStyle::Positional,
        resume: Some(ResumeFlag::Session),
        conversation_id_pointers: OPENCODE_SESSION_KEYS,
        pre_start_auth: PreStartAuth::None,
    },
    Engine {
        id: "hermes",
        // Hermes `chat --query-file` path keeps tool progress observable to the
        // supervisor. The OpenRouter provider is supplied by the attached
        // OpenShell provider; `--yolo` and `--accept-hooks` are required because
        // a card run has no human available to answer approval prompts.
        prefix: "hermes --yolo --accept-hooks --provider openrouter",
        post_model: "chat",
        trailing: "",
        prompt: PromptStyle::QueryFile,
        resume: Some(ResumeFlag::Resume),
        // Hermes prints `session_id:` on stderr rather than embedding its id in
        // the JSONL stream; supervisor::parse_conversation_id handles that
        // textual footer alongside the JSON pointers above.
        conversation_id_pointers: &[],
        pre_start_auth: PreStartAuth::None,
    },
    Engine {
        id: "pi",
        // Headless mode: JSON events on stdout, auto-approve tool perms
        // (sandbox is already the containment boundary). Prompt is positional.
        prefix: "pi --mode json",
        post_model: "",
        trailing: "",
        prompt: PromptStyle::Positional,
        resume: None,
        conversation_id_pointers: &[],
        pre_start_auth: PreStartAuth::None,
    },
];

/// Env var name substituted into the engine command for the prompt/briefing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptEnv {
    /// Supervisor detached start: `SANDBOARD_BRIEFING`.
    Briefing,
    /// Cockpit foreground turn: `SANDBOARD_PROMPT`.
    Prompt,
}

impl PromptEnv {
    fn shell_ref(self) -> &'static str {
        match self {
            Self::Briefing => "\"$SANDBOARD_BRIEFING\"",
            Self::Prompt => "\"$SANDBOARD_PROMPT\"",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownEngine {
    pub id: String,
}

impl fmt::Display for UnknownEngine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "unknown agent engine {:?}; expected one of: {}",
            self.id,
            ENGINES
                .iter()
                .map(|e| e.id)
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

impl std::error::Error for UnknownEngine {}

/// Look up a registered engine. Unknown ids fail — no Claude fallthrough.
pub fn lookup(id: &str) -> Result<&'static Engine, UnknownEngine> {
    let id = id.trim();
    ENGINES
        .iter()
        .find(|e| e.id == id)
        .ok_or_else(|| UnknownEngine { id: id.to_string() })
}

/// Whether this engine can park/resume via a conversation or session id.
pub fn supports_resume(id: &str) -> bool {
    lookup(id).map(|e| e.resume.is_some()).unwrap_or(false)
}

/// Pre-start auth hook for this engine, if any.
pub fn pre_start_auth(id: &str) -> Result<PreStartAuth, UnknownEngine> {
    Ok(lookup(id)?.pre_start_auth)
}

/// Sandbox env so Anthropic-shaped CLIs hit OpenShell `inference.local`.
///
/// The gateway holds Vertex (or other) credentials and injects them on egress.
/// Do not set `CLAUDE_CODE_USE_VERTEX` — that forces direct ADC/metadata discovery.
/// OpenCode needs the `/v1` suffix; Claude Code appends `/v1/messages` itself.
pub fn anthropic_inference_env(engine_id: &str) -> Vec<(String, String)> {
    match engine_id.trim() {
        "opencode" => vec![
            (
                "ANTHROPIC_BASE_URL".into(),
                "https://inference.local/v1".into(),
            ),
            ("ANTHROPIC_API_KEY".into(), "unused".into()),
        ],
        "claude" => vec![
            (
                "ANTHROPIC_BASE_URL".into(),
                "https://inference.local".into(),
            ),
            ("ANTHROPIC_API_KEY".into(), "unused".into()),
        ],
        _ => Vec::new(),
    }
}

/// Shell exports mirrored into start/turn scripts so a reused sandbox still
/// picks up the route without recreate.
pub fn anthropic_inference_exports(engine_id: &str) -> &'static str {
    match engine_id.trim() {
        "opencode" => {
            "export ANTHROPIC_BASE_URL=https://inference.local/v1\n\
             export ANTHROPIC_API_KEY=unused\n\
             unset CLAUDE_CODE_USE_VERTEX\n"
        }
        "claude" => {
            "export ANTHROPIC_BASE_URL=https://inference.local\n\
             export ANTHROPIC_API_KEY=unused\n\
             unset CLAUDE_CODE_USE_VERTEX\n"
        }
        _ => "",
    }
}

/// Shell exports for Hermes' OpenRouter provider.
pub fn hermes_inference_exports(engine_id: &str) -> &'static str {
    match engine_id.trim() {
        // The built-in Hermes OpenRouter provider owns its endpoint. The API key
        // itself is supplied by the attached OpenShell provider as a placeholder.
        "hermes" => "",
        _ => "",
    }
}

/// Create-time env for Hermes' OpenRouter provider.
pub fn hermes_inference_env(engine_id: &str) -> Vec<(String, String)> {
    if engine_id.trim() != "hermes" {
        return Vec::new();
    }
    vec![
        ("HERMES_HOME".into(), "/sandbox/.hermes".into()),
        ("PYTHONUNBUFFERED".into(), "1".into()),
        ("PYTHONDONTWRITEBYTECODE".into(), "1".into()),
    ]
}

/// Engine default when card and sandbox spec omit `model`.
pub fn default_model_for_engine(engine_id: &str) -> Option<&'static str> {
    match engine_id.trim() {
        "agy" => Some(crate::antigravity::DEFAULT_SEAT_MODEL),
        _ => None,
    }
}

/// Whether this engine's CLI accepts a `--model` flag on launch argv.
///
/// Verified via each engine's help output. Claude reaches models through
/// OpenShell `inference.local` instead — see [`anthropic_inference_env`].
/// Hermes and OpenCode accept provider/model values from the resolved sandbox
/// profile.
pub fn engine_accepts_cli_model(engine_id: &str) -> bool {
    matches!(engine_id.trim(), "agy" | "cursor" | "opencode" | "hermes")
}

fn shell_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\"'\"'"))
}

/// `--model …` segment when the engine accepts it and a model is resolved.
pub fn cli_model_argv(engine_id: &str, model: Option<&str>) -> Option<String> {
    if !engine_accepts_cli_model(engine_id) {
        return None;
    }
    let m = model
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or_else(|| default_model_for_engine(engine_id))?;
    Some(format!("--model {}", shell_single_quote(m)))
}

/// Build the inner agent command line (no `timeout` / `setsid` wrapper).
///
/// `conversation_id` is only honored when the engine declares a [`ResumeFlag`];
/// callers that pass `Some` for a non-resumable engine still get a fresh argv
/// (resume gate belongs at the call site).
///
/// `model` is the resolved card → spec → engine-default chain from the board.
pub fn command_line(
    engine_id: &str,
    prompt: PromptEnv,
    conversation_id: Option<&str>,
    model: Option<&str>,
) -> Result<String, UnknownEngine> {
    let engine = lookup(engine_id)?;
    let resume = conversation_id.is_some().then_some(engine.resume).flatten();

    let mut parts: Vec<String> = Vec::new();
    parts.push(engine.prefix.to_string());
    if let Some(argv) = cli_model_argv(engine.id, model) {
        parts.push(argv);
    }
    if !engine.post_model.is_empty() {
        parts.push(engine.post_model.to_string());
    }
    if let Some(flag) = resume {
        parts.push(flag.argv().to_string());
    }
    match engine.prompt {
        PromptStyle::FlagP => {
            parts.push("-p".to_string());
            parts.push(prompt.shell_ref().to_string());
        }
        PromptStyle::Positional => {
            parts.push(prompt.shell_ref().to_string());
        }
        PromptStyle::QueryFile => {
            parts.push("--query-file".to_string());
            parts.push(HERMES_QUERY_FILE.to_string());
        }
    }
    if !engine.trailing.is_empty() {
        parts.push(engine.trailing.to_string());
    }
    Ok(parts.join(" "))
}

/// Prompt file materialized by the start/turn shell wrapper for Hermes.
pub const HERMES_QUERY_FILE: &str = "/tmp/sandboard-hermes-query";

/// Union of conversation/session JSON pointers across registered engines.
///
/// Stream parsers stay engine-agnostic (one log line may arrive before the
/// supervisor knows which shape applies); OpenCode and friends add keys on
/// their registry row and they appear here automatically.
pub fn conversation_id_pointers() -> Vec<&'static str> {
    let mut out = Vec::new();
    for engine in ENGINES {
        for key in engine.conversation_id_pointers {
            if !out.contains(key) {
                out.push(*key);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_engines_are_registered() {
        for id in ["cursor", "agy", "claude", "opencode", "hermes"] {
            assert_eq!(lookup(id).unwrap().id, id);
        }
    }

    #[test]
    fn unknown_engine_is_rejected() {
        let err = lookup("nope").unwrap_err();
        assert_eq!(err.id, "nope");
        let msg = err.to_string();
        assert!(msg.contains("unknown agent engine"), "{msg}");
        assert!(msg.contains("cursor"), "{msg}");
        assert!(msg.contains("agy"), "{msg}");
        assert!(msg.contains("claude"), "{msg}");
        assert!(msg.contains("opencode"), "{msg}");
        assert!(msg.contains("hermes"), "{msg}");
        assert!(command_line("nope", PromptEnv::Briefing, None, None).is_err());
        assert!(!supports_resume("nope"));
    }

    #[test]
    fn hermes_argv_uses_openrouter_chat_and_resumes_sessions() {
        let fresh = command_line("hermes", PromptEnv::Briefing, None, None).unwrap();
        assert_eq!(
            fresh,
            "hermes --yolo --accept-hooks --provider openrouter chat --query-file /tmp/sandboard-hermes-query"
        );

        let custom = command_line(
            "hermes",
            PromptEnv::Briefing,
            None,
            Some("gpt-5.6-luna"),
        )
        .unwrap();
        assert!(custom.contains("--model 'gpt-5.6-luna'"), "{custom}");
        assert!(custom.contains("--provider openrouter"), "{custom}");
        assert!(custom.find("--model").unwrap() < custom.find(" chat ").unwrap());

        let resume = command_line("hermes", PromptEnv::Prompt, Some("session-1"), None).unwrap();
        assert!(
            resume.contains("--resume \"$SANDBOARD_CONVERSATION\""),
            "{resume}"
        );
        assert!(resume.ends_with("--query-file /tmp/sandboard-hermes-query"), "{resume}");
        assert!(supports_resume("hermes"));
        assert_eq!(pre_start_auth("hermes").unwrap(), PreStartAuth::None);
    }

    #[test]
    fn opencode_argv_includes_resolved_model() {
        let cmd = command_line(
            "opencode",
            PromptEnv::Briefing,
            None,
            Some("openrouter/deepseek/deepseek-v4-flash-0731"),
        )
        .unwrap();
        assert!(
            cmd.contains("opencode run --format json --auto --model 'openrouter/deepseek/deepseek-v4-flash-0731'"),
            "{cmd}"
        );
    }

    #[test]
    fn claude_argv_fresh_no_resume() {
        let cmd = command_line("claude", PromptEnv::Briefing, None, None).unwrap();
        assert_eq!(
            cmd,
            "claude --bare --strict-mcp-config --mcp-config /sandbox/.sandboard/mcp/claude_mcp.json -p \"$SANDBOARD_BRIEFING\" --output-format stream-json --verbose --permission-mode bypassPermissions"
        );
        // Claude has no resume flag — conversation id must not change argv.
        let ignored = command_line("claude", PromptEnv::Briefing, Some("cid"), None).unwrap();
        assert_eq!(ignored, cmd);
        // Model is chosen on the gateway via inference.local, not CLI argv.
        let with_model = command_line("claude", PromptEnv::Briefing, None, Some("ignored")).unwrap();
        assert_eq!(with_model, cmd);
        assert!(!supports_resume("claude"));
    }

    #[test]
    fn anthropic_inference_env_splits_opencode_v1_from_claude() {
        let oc = anthropic_inference_env("opencode");
        assert!(oc.iter().any(|(k, v)| {
            k == "ANTHROPIC_BASE_URL" && v == "https://inference.local/v1"
        }));
        let cl = anthropic_inference_env("claude");
        assert!(cl
            .iter()
            .any(|(k, v)| k == "ANTHROPIC_BASE_URL" && v == "https://inference.local"));
        assert!(anthropic_inference_env("cursor").is_empty());
        assert!(anthropic_inference_exports("opencode").contains("/v1"));
        assert!(!anthropic_inference_exports("claude").contains("/v1"));
        assert!(anthropic_inference_exports("cursor").is_empty());
    }

    #[test]
    fn hermes_uses_openrouter_provider_env() {
        assert_eq!(hermes_inference_exports("hermes"), "");
        assert_eq!(
            hermes_inference_env("hermes"),
            vec![
                ("HERMES_HOME".into(), "/sandbox/.hermes".into()),
                ("PYTHONUNBUFFERED".into(), "1".into()),
                ("PYTHONDONTWRITEBYTECODE".into(), "1".into()),
            ]
        );
    }

    #[test]
    fn agy_argv_fresh_and_resume() {
        let fresh = command_line("agy", PromptEnv::Briefing, None, None).unwrap();
        assert_eq!(
            fresh,
            format!(
                "agy --dangerously-skip-permissions --model '{}' --print-timeout 24h --output-format stream-json -p \"$SANDBOARD_BRIEFING\"",
                crate::antigravity::DEFAULT_SEAT_MODEL
            )
        );
        // `-p` must not precede `--model` or the model flag becomes the prompt.
        let model_at = fresh.find("--model").expect("model");
        let p_at = fresh.find(" -p ").expect("-p");
        assert!(model_at < p_at, "{fresh}");
        assert!(!fresh.contains("--conversation"));

        let custom = command_line("agy", PromptEnv::Briefing, None, Some("gemini-pro")).unwrap();
        assert!(custom.contains("--model 'gemini-pro'"), "{custom}");

        let resume = command_line("agy", PromptEnv::Briefing, Some("cid"), None).unwrap();
        assert!(resume.contains("--conversation \"$SANDBOARD_CONVERSATION\""), "{resume}");
        assert!(resume.contains("-p \"$SANDBOARD_BRIEFING\""), "{resume}");
        assert!(supports_resume("agy"));
        assert_eq!(pre_start_auth("agy").unwrap(), PreStartAuth::Agy);
    }

    #[test]
    fn engine_accepts_cli_model_matrix() {
        assert!(engine_accepts_cli_model("agy"));
        assert!(engine_accepts_cli_model("cursor"));
        assert!(!engine_accepts_cli_model("claude"));
        assert!(engine_accepts_cli_model("opencode"));
    }

    #[test]
    fn cursor_argv_fresh_and_resume() {
        let fresh = command_line("cursor", PromptEnv::Briefing, None, None).unwrap();
        assert_eq!(
            fresh,
            "agent -p --force --trust --approve-mcps --sandbox disabled --output-format stream-json \"$SANDBOARD_BRIEFING\""
        );
        assert!(!fresh.contains("--model"), "{fresh}");
        assert!(!fresh.contains("--resume"));

        let custom = command_line("cursor", PromptEnv::Briefing, None, Some("gpt-5")).unwrap();
        assert!(custom.contains("--model 'gpt-5'"), "{custom}");
        let model_at = custom.find("--model").expect("model");
        let prompt_at = custom.find("\"$SANDBOARD_BRIEFING\"").expect("prompt");
        assert!(model_at < prompt_at, "{custom}");

        let resume = command_line("cursor", PromptEnv::Prompt, Some("sid"), None).unwrap();
        assert!(
            resume.contains("--resume \"$SANDBOARD_CONVERSATION\""),
            "{resume}"
        );
        assert!(resume.ends_with("\"$SANDBOARD_PROMPT\""), "{resume}");
        assert!(!resume.contains("--model"), "{resume}");

        let resume_model =
            command_line("cursor", PromptEnv::Prompt, Some("sid"), Some("sonnet-4")).unwrap();
        assert!(resume_model.contains("--model 'sonnet-4'"), "{resume_model}");
        assert!(
            resume_model.contains("--resume \"$SANDBOARD_CONVERSATION\""),
            "{resume_model}"
        );
        assert!(supports_resume("cursor"));
        assert_eq!(pre_start_auth("cursor").unwrap(), PreStartAuth::None);
    }

    #[test]
    fn conversation_id_pointers_cover_agy_and_cursor() {
        let keys = conversation_id_pointers();
        assert!(keys.contains(&"/conversation_id"));
        assert!(keys.contains(&"/session_id"));
        assert!(keys.contains(&"/step_update/conversation_id"));
        assert!(keys.contains(&"/sessionID"));
    }

    #[test]
    fn opencode_argv_fresh_and_resume() {
        let fresh = command_line("opencode", PromptEnv::Briefing, None, None).unwrap();
        assert_eq!(
            fresh,
            "opencode run --format json --auto \"$SANDBOARD_BRIEFING\""
        );
        assert!(!fresh.contains("--model"), "{fresh}");
        assert!(!fresh.contains("--session"));

        let with_model =
            command_line("opencode", PromptEnv::Briefing, None, Some("ignored")).unwrap();
        assert_eq!(
            with_model,
            "opencode run --format json --auto --model 'ignored' \"$SANDBOARD_BRIEFING\""
        );

        let resume = command_line("opencode", PromptEnv::Prompt, Some("ses_abc"), None).unwrap();
        assert!(
            resume.contains("--session \"$SANDBOARD_CONVERSATION\""),
            "{resume}"
        );
        assert!(resume.ends_with("\"$SANDBOARD_PROMPT\""), "{resume}");
        assert!(supports_resume("opencode"));
        assert_eq!(pre_start_auth("opencode").unwrap(), PreStartAuth::None);
    }
}
