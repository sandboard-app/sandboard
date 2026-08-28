import { useCallback, useEffect, useId, useRef, useState } from "react";
import { api } from "../api.js";
import {
  openCockpitAttach,
  type CockpitAttachHandle,
} from "../cockpitAttachClient.js";
import type { CockpitSandboxPhase, CockpitSession } from "../types.js";

const POLL_IDLE_MS = 4000;
const POLL_ACTIVE_MS = 1000;

/** Phases where the seat is mid create/delete — poll faster for UI feedback. */
export function cockpitPollIntervalMs(
  session: CockpitSession | null,
): number {
  const phase = session?.sandbox_phase;
  if (
    phase === "starting" ||
    phase === "waiting_for_delete" ||
    phase === "provisioning" ||
    phase === "stopping" ||
    phase === "error"
  ) {
    return POLL_ACTIVE_MS;
  }
  return POLL_IDLE_MS;
}

/** Short label for panel / titlebar (no environment dump). */
export function cockpitPhaseLabel(
  session: CockpitSession | null,
): string | null {
  if (session == null) return null;
  const phase = session.sandbox_phase ?? "idle";
  const detail = session.phase_detail?.trim();
  switch (phase) {
    case "starting":
      return detail || "Starting cockpit sandbox…";
    case "waiting_for_delete":
      return detail || "Reclaiming previous sandbox…";
    case "provisioning":
      return detail || "Provisioning sandbox…";
    case "ready":
      return "Ready";
    case "stopping":
      return detail || "Stopping cockpit…";
    case "error":
      return detail ? `Error: ${detail}` : "Sandbox error";
    case "idle":
    default:
      return session.status === "parked" ? "Parked" : null;
  }
}

/** Compact bar chip word (closed drop still shows lifecycle). */
export function cockpitBarChip(
  session: CockpitSession | null,
): { text: string; busy: boolean } | null {
  if (session == null) return null;
  const phase = session.sandbox_phase ?? "idle";
  switch (phase) {
    case "starting":
      return { text: "starting", busy: true };
    case "waiting_for_delete":
      return { text: "reclaiming", busy: true };
    case "provisioning":
      return { text: "provisioning", busy: true };
    case "stopping":
      return { text: "stopping", busy: true };
    case "error":
      return { text: "error", busy: false };
    case "ready":
      return session.status === "parked"
        ? { text: "parked", busy: false }
        : { text: "ready", busy: false };
    default:
      return session.status === "parked"
        ? { text: "parked", busy: false }
        : null;
  }
}

/** Elapsed whole seconds since `phase_since` (or 0). */
export function cockpitPhaseElapsedSecs(
  session: CockpitSession | null,
  nowMs: number = Date.now(),
): number {
  const since = session?.phase_since;
  if (!since) return 0;
  const t = Date.parse(since);
  if (Number.isNaN(t)) return 0;
  return Math.max(0, Math.floor((nowMs - t) / 1000));
}

/** Why attach is locked — Board session + sandbox phase. */
export function cockpitAttachGate(
  session: CockpitSession | null,
): { canAttach: boolean; reason: string | null } {
  if (session == null) {
    return { canAttach: false, reason: "Start a cockpit session to open the terminal." };
  }
  if (session.status === "parked") {
    return {
      canAttach: false,
      reason: "Cockpit session is parked. Stop it, then Start again.",
    };
  }
  const phase = session.sandbox_phase;
  if (phase === "error") {
    const detail = session.phase_detail?.trim();
    return {
      canAttach: false,
      reason: detail
        ? `Cockpit sandbox failed: ${detail}`
        : "Cockpit sandbox failed. Stop and Start again.",
    };
  }
  if (phase === "waiting_for_delete") {
    return {
      canAttach: false,
      reason:
        session.phase_detail?.trim() ||
        "Waiting for the previous sandbox to finish deleting…",
    };
  }
  if (phase === "starting" || phase === "provisioning" || phase === "stopping") {
    return {
      canAttach: false,
      reason: cockpitPhaseLabel(session) ?? "Waiting for the cockpit sandbox…",
    };
  }
  const environment = session.environment?.trim();
  if (!environment) {
    return {
      canAttach: false,
      reason: "Waiting for the supervisor to provision the cockpit environment…",
    };
  }
  return { canAttach: true, reason: null };
}

/** @deprecated alias — prefer cockpitAttachGate */
export function cockpitChatGate(session: CockpitSession | null): {
  canSend: boolean;
  reason: string | null;
} {
  const g = cockpitAttachGate(session);
  return { canSend: g.canAttach, reason: g.reason };
}

/** Exponential backoff for attach reconnect after sandboard/proxy drops the socket. */
export function cockpitAttachRetryDelayMs(attempt: number): number {
  const n = Math.max(0, Math.min(attempt, 5));
  return Math.min(1000 * 2 ** n, 15_000);
}

/**
 * Start / Stop + lifecycle phase strip. Session metadata stays on the Board;
 * we show phase (not env/conversation dumps).
 */
export function CockpitSessionView({
  session,
  busy,
  error,
  nowMs,
  onStart,
  onStop,
}: {
  session: CockpitSession | null;
  busy?: boolean;
  error?: string | null;
  nowMs?: number;
  onStart: () => void;
  onStop: () => void;
}) {
  const absent = session == null;
  const phaseLabel = cockpitPhaseLabel(session);
  const elapsed = cockpitPhaseElapsedSecs(session, nowMs ?? Date.now());
  const showElapsed =
    session != null &&
    (session.sandbox_phase === "starting" ||
      session.sandbox_phase === "waiting_for_delete" ||
      session.sandbox_phase === "provisioning" ||
      session.sandbox_phase === "stopping") &&
    elapsed > 0;

  return (
    <div className="cockpit-session" data-testid="cockpit-session">
      {error && (
        <div className="err" data-testid="cockpit-session-error">
          {error}
        </div>
      )}
      <div className="cockpit-session-actions" data-testid="cockpit-session-actions">
        <button
          type="button"
          className="primary"
          disabled={busy || !absent}
          onClick={onStart}
          data-testid="cockpit-session-start"
        >
          Start
        </button>
        <button
          type="button"
          className="danger"
          disabled={busy || absent}
          onClick={onStop}
          data-testid="cockpit-session-stop"
        >
          Stop
        </button>
      </div>
      {phaseLabel && (
        <p
          className={`cockpit-session-phase${
            session?.sandbox_phase === "error" ? " err" : " dim"
          }`}
          data-testid="cockpit-session-phase"
        >
          {phaseLabel}
          {showElapsed ? ` ${elapsed}s` : null}
        </p>
      )}
    </div>
  );
}

/**
 * Real attach face — xterm.js over `/api/cockpit-attach` (ExecSandboxInteractive).
 * SSR-safe: terminal + WebSocket only mount in the browser when attachable.
 */
export function CockpitAttachView({
  canAttach,
  disabledReason,
  environment,
  sessionStatus,
  sandboxPhase,
  phaseLabel,
  reconnectKey = 0,
  /** When the drop re-opens, refit xterm (attach stays mounted while collapsed). */
  panelOpen = true,
}: {
  canAttach: boolean;
  disabledReason?: string | null;
  environment?: string | null;
  sessionStatus?: CockpitSession["status"] | null;
  sandboxPhase?: CockpitSandboxPhase | null;
  phaseLabel?: string | null;
  /** Bump to force a fresh WebSocket (e.g. after Stop/Start). */
  reconnectKey?: number;
  panelOpen?: boolean;
}) {
  const hostRef = useRef<HTMLDivElement>(null);
  const titleId = useId();
  const [attachError, setAttachError] = useState<string | null>(null);
  const [connected, setConnected] = useState(false);
  /** Internal remount counter — bumped on unexpected socket close for backoff retry. */
  const [attachGen, setAttachGen] = useState(0);
  const retryAttemptRef = useRef(0);
  const titleEnv = environment?.trim() || "cockpit";
  const titleStatus =
    sessionStatus === "parked"
      ? "parked"
      : !canAttach
        ? phaseLabel?.replace(/\…$/, "") ||
          (sandboxPhase === "waiting_for_delete"
            ? "reclaiming…"
            : sandboxPhase === "provisioning" || sandboxPhase === "starting"
              ? "provisioning…"
              : sandboxPhase === "error"
                ? "error"
                : "waiting…")
        : sessionStatus === "running"
          ? connected
            ? "attached"
            : attachError
              ? "reconnecting…"
              : "connecting…"
          : "offline";

  // Parent Start/Stop (or env change) should not inherit a prior backoff streak.
  useEffect(() => {
    retryAttemptRef.current = 0;
    setAttachGen(0);
    setAttachError(null);
  }, [reconnectKey, environment, canAttach]);

  useEffect(() => {
    if (!canAttach || !hostRef.current) {
      setConnected(false);
      return;
    }

    let disposed = false;
    let handle: CockpitAttachHandle | null = null;
    let retryTimer: ReturnType<typeof setTimeout> | null = null;
    let reconnectScheduled = false;
    setAttachError(null);
    setConnected(false);

    const scheduleReconnect = (hint?: string) => {
      if (disposed || reconnectScheduled) return;
      reconnectScheduled = true;
      const attempt = retryAttemptRef.current;
      retryAttemptRef.current = attempt + 1;
      const delay = cockpitAttachRetryDelayMs(attempt);
      const secs = Math.max(1, Math.round(delay / 1000));
      setAttachError(
        hint
          ? `${hint} — retrying in ${secs}s…`
          : `attach disconnected — retrying in ${secs}s…`,
      );
      setConnected(false);
      retryTimer = setTimeout(() => {
        retryTimer = null;
        if (disposed) return;
        setAttachGen((g) => g + 1);
      }, delay);
    };

    // React StrictMode (Vite dev) mount→unmount→remounts effects. Opening the
    // attach WebSocket in the phantom first mount still hits the server, which
    // pkills the agent on the real remount — death spiral of exit 143. Defer
    // past that cleanup so only the surviving mount connects.
    let startTimer: ReturnType<typeof setTimeout> | null = setTimeout(() => {
      startTimer = null;
      if (disposed || !hostRef.current) return;
      const host = hostRef.current;
      void openCockpitAttach(host, {
        isDisposed: () => disposed,
        onOpen: () => {
          retryAttemptRef.current = 0;
          setConnected(true);
          setAttachError(null);
        },
        onClose: (hint) => {
          setConnected(false);
          scheduleReconnect(hint);
        },
        onError: (message) => {
          setAttachError(message);
        },
      }).then((h) => {
        if (disposed) {
          h?.dispose();
          return;
        }
        if (!h) {
          scheduleReconnect("attach failed to start");
          return;
        }
        handle = h;
      });
    }, 0);

    return () => {
      disposed = true;
      if (startTimer) clearTimeout(startTimer);
      if (retryTimer) clearTimeout(retryTimer);
      handle?.dispose();
    };
  }, [canAttach, attachGen, reconnectKey, environment]);

  // Refit when the drop opens (was display:none / zero height while collapsed).
  useEffect(() => {
    if (!panelOpen || !canAttach || !hostRef.current) return;
    const host = hostRef.current;
    requestAnimationFrame(() => {
      host.dispatchEvent(new Event("resize"));
      // FitAddon listens via ResizeObserver; nudge by toggling a tiny style.
      const prev = host.style.width;
      host.style.width = "99.9%";
      host.style.width = prev;
    });
  }, [panelOpen, canAttach, attachGen]);

  return (
    <section
      className="cockpit-term"
      aria-labelledby={titleId}
      data-testid="cockpit-attach"
    >
      <div className="cockpit-term-window" data-testid="cockpit-term-window">
        <div className="cockpit-term-titlebar">
          <span className="cockpit-term-traffic" aria-hidden="true">
            <i /><i /><i />
          </span>
          <h2 id={titleId} className="cockpit-term-title">
            {titleEnv}
            <span className="cockpit-term-title-status"> — {titleStatus}</span>
          </h2>
        </div>

        {attachError && (
          <div className="err cockpit-term-error" data-testid="cockpit-attach-error">
            {attachError}
          </div>
        )}

        {!canAttach && (
          <p className="dim cockpit-term-gate" data-testid="cockpit-attach-gate">
            {disabledReason ?? "Start a cockpit session to attach."}
          </p>
        )}

        <div
          className="cockpit-xterm"
          ref={hostRef}
          data-testid="cockpit-xterm"
          // Keep a mount point even when gated so layout stays stable; effect
          // only opens the WebSocket when canAttach.
          style={{ display: canAttach ? undefined : "none" }}
        />
      </div>
    </section>
  );
}

/** Lucide-style chevrons-down — flips when the drop is open. */
function CockpitChevrons() {
  return (
    <svg
      className="cockpit-bar-icon"
      width="18"
      height="18"
      viewBox="0 0 24 24"
      fill="none"
      xmlns="http://www.w3.org/2000/svg"
      aria-hidden="true"
    >
      <path
        d="m7 6 5 5 5-5"
        stroke="currentColor"
        strokeWidth="2"
        strokeLinecap="round"
        strokeLinejoin="round"
      />
      <path
        d="m7 13 5 5 5-5"
        stroke="currentColor"
        strokeWidth="2"
        strokeLinecap="round"
        strokeLinejoin="round"
      />
    </svg>
  );
}

/** Centered top-bar control — opens the Cockpit drop below the header. */
export function CockpitToggle({
  open,
  onToggle,
  chip,
}: {
  open: boolean;
  onToggle: () => void;
  /** Compact lifecycle word when session is active / transitional. */
  chip?: { text: string; busy: boolean } | null;
}) {
  const label = chip
    ? open
      ? `Collapse Cockpit (${chip.text})`
      : `Open Cockpit (${chip.text})`
    : open
      ? "Collapse Cockpit"
      : "Open Cockpit";
  return (
    <button
      type="button"
      className={`cockpit-bar-btn${open ? " open" : ""}${chip?.busy ? " busy" : ""}`}
      aria-expanded={open}
      aria-controls="cockpit-drop"
      aria-label={label}
      title={label}
      data-testid="cockpit-toggle"
      onClick={onToggle}
    >
      <CockpitChevrons />
      {chip && (
        <span className="cockpit-bar-chip" data-testid="cockpit-bar-chip">
          {chip.busy && (
            <span className="cockpit-bar-chip-dot" aria-hidden="true" />
          )}
          {chip.text}
        </span>
      )}
    </button>
  );
}

/**
 * Panel under the top bar. Stays mounted after the first open so collapse does
 * not tear down the attach WebSocket / interactive agent. `shown` lags one
 * frame behind `open` so open/close both slide via CSS.
 */
export function CockpitDrop({
  open,
  session,
  onSession,
  pollError,
}: {
  open: boolean;
  /** Lifted session so the bar chip can share one poll. */
  session?: CockpitSession | null;
  onSession?: (s: CockpitSession | null) => void;
  pollError?: string | null;
}) {
  const [kept, setKept] = useState(false);
  const [shown, setShown] = useState(false);

  useEffect(() => {
    if (open) {
      setKept(true);
      const id = requestAnimationFrame(() => setShown(true));
      return () => cancelAnimationFrame(id);
    }
    setShown(false);
  }, [open]);

  if (!open && !kept) return null;

  return (
    <section
      id="cockpit-drop"
      className={`cockpit-drop${shown ? " open" : ""}`}
      data-testid="cockpit-drop"
      aria-label="Cockpit"
      aria-hidden={!open}
      inert={!open || undefined}
    >
      <div className="cockpit-drop-inner">
        <Cockpit
          panelOpen={open}
          session={session}
          onSession={onSession}
          pollError={pollError}
        />
      </div>
    </section>
  );
}

/**
 * Shared cockpit session poll — bar chip + panel use the same Board record.
 * Pass `enabled: false` when a parent already owns the poll (avoid doubles).
 */
export function useCockpitSession(opts?: { enabled?: boolean }): {
  session: CockpitSession | null;
  setSession: (s: CockpitSession | null) => void;
  error: string | null;
  refresh: () => Promise<void>;
} {
  const enabled = opts?.enabled !== false;
  const [session, setSession] = useState<CockpitSession | null>(null);
  const [error, setError] = useState<string | null>(null);
  const sessionRef = useRef<CockpitSession | null>(null);
  sessionRef.current = session;

  const refresh = useCallback(async () => {
    try {
      const out = await api.getCockpitSession();
      setSession(out.session ?? null);
      setError(null);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  }, []);

  useEffect(() => {
    if (!enabled) return;
    let alive = true;
    let timer: ReturnType<typeof setTimeout> | null = null;

    const schedule = (ms: number) => {
      timer = setTimeout(() => {
        void tick();
      }, ms);
    };

    const tick = async () => {
      if (!alive) return;
      try {
        const out = await api.getCockpitSession();
        if (!alive) return;
        setSession(out.session ?? null);
        setError(null);
        schedule(cockpitPollIntervalMs(out.session ?? null));
      } catch (e) {
        if (!alive) return;
        setError(e instanceof Error ? e.message : String(e));
        schedule(POLL_IDLE_MS);
      }
    };

    void tick();
    return () => {
      alive = false;
      if (timer) clearTimeout(timer);
    };
  }, [enabled]);

  return { session, setSession, error, refresh };
}

/**
 * Cockpit — Start/Stop the Board cockpit session; terminal attaches when ready.
 * MCP inject stays silent in the background.
 *
 * When `session` + `onSession` are passed (App bar chip), polling is owned by
 * the parent. Standalone `<Cockpit />` (tests) polls itself.
 */
export function Cockpit({
  panelOpen = true,
  session: sessionProp,
  onSession,
  pollError,
}: {
  panelOpen?: boolean;
  session?: CockpitSession | null;
  onSession?: (s: CockpitSession | null) => void;
  /** Error from a parent-owned poll (App). */
  pollError?: string | null;
} = {}) {
  const controlled = sessionProp !== undefined && onSession !== undefined;
  const local = useCockpitSession({ enabled: !controlled });
  const session = controlled ? sessionProp! : local.session;
  const setSession = controlled ? onSession! : local.setSession;

  const refresh = useCallback(async () => {
    try {
      const out = await api.getCockpitSession();
      setSession(out.session ?? null);
      setError(null);
      if (!out.session) {
        provisionedEnv.current = null;
      }
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  }, [setSession]);

  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [reconnectKey, setReconnectKey] = useState(0);
  const [nowMs, setNowMs] = useState(() => Date.now());
  const provisionedEnv = useRef<string | null>(null);

  useEffect(() => {
    const fromPoll = controlled ? pollError : local.error;
    if (fromPoll) setError(fromPoll);
  }, [controlled, pollError, local.error]);

  // Elapsed clock while transitional.
  useEffect(() => {
    const phase = session?.sandbox_phase;
    const active =
      phase === "starting" ||
      phase === "waiting_for_delete" ||
      phase === "provisioning" ||
      phase === "stopping";
    if (!active) return;
    const id = setInterval(() => setNowMs(Date.now()), 1000);
    return () => clearInterval(id);
  }, [session?.sandbox_phase]);

  const provisionMcp = useCallback(async () => {
    const out = await api.provisionCockpitMcp();
    provisionedEnv.current = out.environment;
  }, []);

  // When the supervisor fills environment, inject MCP once for this env.
  useEffect(() => {
    const env = session?.environment?.trim();
    if (
      session?.status === "running" &&
      env &&
      provisionedEnv.current !== env
    ) {
      void provisionMcp().catch(() => {
        /* attach still works; next Start retries inject */
      });
    }
  }, [session?.status, session?.environment, provisionMcp]);

  const runAction = useCallback(
    async (action: () => Promise<unknown>, opts?: { reconnect?: boolean }) => {
      setBusy(true);
      setError(null);
      try {
        await action();
        await refresh();
        if (opts?.reconnect) setReconnectKey((k) => k + 1);
      } catch (e) {
        setError(e instanceof Error ? e.message : String(e));
        await refresh();
      } finally {
        setBusy(false);
      }
    },
    [refresh],
  );

  const gate = cockpitAttachGate(session);
  const phaseLabel = cockpitPhaseLabel(session);

  return (
    <div className="cockpit-pane" data-testid="cockpit-pane">
      <CockpitSessionView
        session={session}
        busy={busy}
        error={error}
        nowMs={nowMs}
        onStart={() =>
          void runAction(() => api.startCockpitSession(), { reconnect: true })
        }
        onStop={() =>
          void runAction(async () => {
            provisionedEnv.current = null;
            await api.stopCockpitSession();
          })
        }
      />

      <CockpitAttachView
        canAttach={gate.canAttach}
        disabledReason={gate.reason}
        environment={session?.environment}
        sessionStatus={session?.status ?? null}
        sandboxPhase={session?.sandbox_phase ?? null}
        phaseLabel={phaseLabel}
        reconnectKey={reconnectKey}
        panelOpen={panelOpen}
      />
    </div>
  );
}
