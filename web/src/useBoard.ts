import { useEffect, useReducer, useRef, useState } from "react";
import { api } from "./api.js";
import type { BoardEvent, GoalView, Snapshot, StoryLine, WorkItem } from "./types.js";
import { sandboardWsUrl } from "./wsUrl.js";

export type BoardEventListener = (ev: BoardEvent) => void;

const listeners = new Set<BoardEventListener>();

/**
 * Subscribe to real-time board events emitted by SSE/WebSocket stream.
 * Returns an unsubscribe function that cleans up the listener when called.
 */
export function subscribeBoardEvents(listener: BoardEventListener): () => void {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

/**
 * Emit a board event to all active subscribers.
 */
export function emitBoardEvent(ev: BoardEvent): void {
  for (const listener of listeners) {
    try {
      listener(ev);
    } catch {
      /* ignore subscriber errors */
    }
  }
}

export interface BoardState {
  items: Map<number, WorkItem>;
  goals: GoalView[];
  stories: Map<number, StoryLine[]>;
  serverTime: string | null;
  agentTimeout: number;
  loaded: boolean;
  connected: boolean;
  defaultEngine: string;
  defaultModel: string;
  /** When the last successful load happened. Drives the staleness warning. */
  lastLoadedAt: number | null;
  /** Monotonic event sequence number. */
  lastSeenSeq: number;
}

export type Action =
  | { type: "snapshot"; snap: Snapshot }
  | { type: "event"; ev: BoardEvent }
  | { type: "connected"; ok: boolean };

export const initial: BoardState = {
  items: new Map(),
  goals: [],
  stories: new Map(),
  serverTime: null,
  agentTimeout: 1800,
  loaded: false,
  connected: false,
  defaultEngine: "",
  defaultModel: "",
  lastLoadedAt: null,
  lastSeenSeq: 0,
};

/**
 * Live events can beat a concurrent REST snapshot by a handful of seqs.
 * A gap larger than this after a successful `/api/board` means the server
 * restarted (seq counter reset) — take the snapshot as authoritative.
 */
const STALE_SNAPSHOT_RACE_MAX_GAP = 32;

/**
 * Pure reducer for board state updates.
 * Guards against stale REST snapshots with older sequence numbers overwriting
 * newer live event state — unless the stream is down or seq clearly rewound
 * (sandboard restart), in which case REST wins.
 */
export function reduce(s: BoardState, a: Action): BoardState {
  switch (a.type) {
    case "snapshot": {
      const snapSeq = a.snap.seq ?? 0;
      if (s.lastSeenSeq > 0 && snapSeq < s.lastSeenSeq) {
        const gap = s.lastSeenSeq - snapSeq;
        // Connected + tiny gap: keep live state; still mark reachable so the
        // NOT LIVE banner clears when retry/poll succeeds.
        if (s.connected && gap <= STALE_SNAPSHOT_RACE_MAX_GAP) {
          return { ...s, lastLoadedAt: Date.now() };
        }
        // Disconnected retry, or seq rewound past a race → apply below.
      }

      const items = new Map(a.snap.items.map((i) => [i.id, i]));
      const stories = new Map(a.snap.goals.map((g) => [g.id, g.story]));
      return {
        ...s,
        items,
        stories,
        goals: a.snap.goals,
        serverTime: a.snap.server_time,
        agentTimeout: a.snap.agent_timeout_secs,
        defaultEngine: a.snap.default_engine ?? "",
        defaultModel: a.snap.default_model ?? "",
        loaded: true,
        lastLoadedAt: Date.now(),
        // After a rewind, snapSeq is the truth — do not keep the old high-water.
        lastSeenSeq: snapSeq,
      };
    }
    case "event": {
      // Server tells us our last_seq is ahead of its counter (restart).
      if (a.ev.type === "reset") {
        return { ...s, lastSeenSeq: a.ev.seq ?? 0 };
      }

      const evSeq = a.ev.seq ?? (s.lastSeenSeq + 1);
      // Ignore duplicate or out-of-order events with older/equal sequence numbers
      if (s.lastSeenSeq > 0 && evSeq <= s.lastSeenSeq) {
        return s;
      }

      const lastSeenSeq = Math.max(s.lastSeenSeq, evSeq);

      if (a.ev.type === "upsert") {
        const items = new Map(s.items);
        items.set(a.ev.item.id, a.ev.item);
        return { ...s, items, lastSeenSeq };
      }
      if (a.ev.type === "delete") {
        const items = new Map(s.items);
        items.delete(a.ev.id);
        return { ...s, items, lastSeenSeq };
      }
      if (a.ev.type === "story") {
        const stories = new Map(s.stories);
        const prev = stories.get(a.ev.goal) ?? [];
        stories.set(a.ev.goal, [...prev, { at: a.ev.at, text: a.ev.text }]);
        return { ...s, stories, lastSeenSeq };
      }
      return { ...s, lastSeenSeq };
    }
    case "connected":
      return { ...s, connected: a.ok };
  }
}

export function isSequenceGap(lastSeenSeq: number, incomingSeq: number): boolean {
  return lastSeenSeq > 0 && incomingSeq > lastSeenSeq + 1;
}

/** Past this with no successful load, what you are looking at is history. */
export const STALE_AFTER_MS = 12_000;

/**
 * Snapshot once, then apply deltas. Goal rollups are recomputed server-side on
 * a slower cadence — the deltas keep the cards live in between.
 *
 * A failed poll deliberately leaves the last snapshot on screen rather than
 * blanking the board — but it must then *say so*. Silently rendering stale
 * state as though it were current is the worst thing a control plane can do:
 * it looks healthy while you make decisions against a frozen picture.
 */
export function useBoard() {
  const [state, dispatch] = useReducer(reduce, initial);
  const [error, setError] = useState<string | null>(null);
  /** Bumped by `refresh` so the stream effect tears down and reconnects. */
  const [streamGen, setStreamGen] = useState(0);
  const wsRef = useRef<WebSocket | EventSource | null>(null);
  const lastSeenSeqRef = useRef<number>(0);
  const wasConnectedRef = useRef<boolean>(false);

  // Keep lastSeenSeqRef updated synchronously with state.lastSeenSeq
  useEffect(() => {
    lastSeenSeqRef.current = state.lastSeenSeq;
  }, [state.lastSeenSeq]);

  useEffect(() => {
    let alive = true;
    let socket: WebSocket | EventSource | null = null;

    const load = () =>
      api
        .board()
        .then((snap) => {
          if (!alive) return;
          dispatch({ type: "snapshot", snap });
          setError(null);
        })
        .catch((e) => alive && setError(String(e)));

    load();

    const attachEventSource = () => {
      // Same-origin so the session cookie is sent (auth middleware).
      const es = new EventSource("/api/events", { withCredentials: true });
      wsRef.current = es;
      socket = es;

      es.onopen = () => {
        if (!alive) {
          es.close();
          return;
        }
        dispatch({ type: "connected", ok: true });
        if (wasConnectedRef.current) {
          load();
        }
        wasConnectedRef.current = true;
      };

      es.onerror = () => {
        if (!alive) return;
        dispatch({ type: "connected", ok: false });
      };

      es.onmessage = (m) => {
        if (!alive) return;
        try {
          const ev = JSON.parse(m.data) as BoardEvent;
          if (ev && typeof ev.seq === "number") {
            if (isSequenceGap(lastSeenSeqRef.current, ev.seq)) {
              load();
            }
          }
          if (ev && ev.type === "reset") {
            load();
          }
          dispatch({ type: "event", ev });
          emitBoardEvent(ev);
        } catch {
          /* keep-alive frames */
        }
      };
    };

    const attachWebSocket = () => {
      // Prefer API host in Vite dev — see `sandboardWsUrl` (Tailscale→Vite WSS stalls).
      const ws = new WebSocket(sandboardWsUrl("/api/ws"));
      wsRef.current = ws;
      socket = ws;

      ws.onopen = () => {
        if (!alive) {
          ws.close();
          return;
        }
        dispatch({ type: "connected", ok: true });
        ws.send(JSON.stringify({ type: "subscribe", last_seq: lastSeenSeqRef.current || null }));
        if (wasConnectedRef.current) {
          load();
        }
        wasConnectedRef.current = true;
      };

      ws.onclose = () => {
        if (!alive) return;
        dispatch({ type: "connected", ok: false });
      };

      ws.onerror = () => {
        if (!alive) return;
        dispatch({ type: "connected", ok: false });
      };

      ws.onmessage = (m) => {
        if (!alive) return;
        try {
          const data = typeof m.data === "string" ? JSON.parse(m.data) : null;
          if (!data) return;

          if (data.type === "ping") {
            ws.send(JSON.stringify({ type: "pong" }));
            return;
          }
          if (data.type === "pong") {
            return;
          }

          const ev = data as BoardEvent;
          if (ev && typeof ev.seq === "number") {
            if (isSequenceGap(lastSeenSeqRef.current, ev.seq)) {
              load();
            }
          }
          if (ev && ev.type === "reset") {
            load();
          }
          dispatch({ type: "event", ev });
          emitBoardEvent(ev);
        } catch {
          /* ignore non-json frames */
        }
      };
    };

    // React Strict Mode (dev) runs effect → cleanup → effect. Opening a
    // WebSocket synchronously means cleanup closes a CONNECTING socket, and
    // Safari always logs that. Defer so the first pass cancels the timer and
    // never constructs a socket.
    const startTimer = window.setTimeout(() => {
      if (!alive) return;
      if (typeof WebSocket !== "undefined") {
        attachWebSocket();
      } else {
        attachEventSource();
      }
    }, 0);

    // Rollups (progress, chunk summaries, spend) are derived server-side, so
    // re-pull them periodically. Card state itself arrives over WebSocket.
    const poll = setInterval(load, 4000);

    return () => {
      alive = false;
      window.clearTimeout(startTimer);
      clearInterval(poll);
      dispatch({ type: "connected", ok: false });
      if (!socket) return;
      if (socket instanceof WebSocket) {
        // Never close() while CONNECTING — Safari treats that as an error.
        // Mark dead via `alive` so onopen closes immediately if it wins the race.
        if (socket.readyState === WebSocket.OPEN) {
          socket.close();
        }
        return;
      }
      socket.close();
    };
  }, [streamGen]);

  return {
    ...state,
    error,
    refresh: () => {
      // Mark disconnected first so a seq-rewound snapshot (sandboard restart) is
      // applied instead of discarded; then remount the WS/SSE effect.
      dispatch({ type: "connected", ok: false });
      setStreamGen((g) => g + 1);
    },
  };
}

/** A ticking clock so relative times and countdowns stay honest between events. */
export function useNow(intervalMs = 1000) {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    const t = setInterval(() => setNow(Date.now()), intervalMs);
    return () => clearInterval(t);
  }, [intervalMs]);
  return now;
}

