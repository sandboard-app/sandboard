import type { Terminal as XTerm } from "@xterm/xterm";
import type { FitAddon as FitAddonType } from "@xterm/addon-fit";
import { xtermThemeFromDocument } from "./theme.js";
import { sandboardWsUrl } from "./wsUrl.js";

export type CockpitAttachHandle = {
  /** Tear down xterm + WebSocket. Safe to call more than once. */
  dispose: () => void;
};

export type CockpitAttachCallbacks = {
  onOpen: () => void;
  onClose: (hint?: string) => void;
  onError: (message: string) => void;
  /** True when the React effect has already cleaned up. */
  isDisposed: () => boolean;
};

/**
 * Open xterm + `/api/cockpit-attach` WebSocket into `host`.
 *
 * Lives in its own module so production Rollup cannot DCE the dynamic xterm
 * imports (it was dropping a `void (async () => import(...))()` nested inside
 * the component `setTimeout`, leaving the UI stuck on "connecting…").
 */
export async function openCockpitAttach(
  host: HTMLElement,
  cbs: CockpitAttachCallbacks,
): Promise<CockpitAttachHandle | null> {
  if (cbs.isDisposed()) return null;

  const [{ Terminal }, { FitAddon }] = await Promise.all([
    import("@xterm/xterm"),
    import("@xterm/addon-fit"),
  ]);
  if (cbs.isDisposed()) return null;

  host.replaceChildren();

  const term: XTerm = new Terminal({
    cursorBlink: true,
    fontSize: 13,
    fontFamily: "ui-monospace, SFMono-Regular, Menlo, Consolas, monospace",
    theme: xtermThemeFromDocument(),
    allowProposedApi: true,
  });
  const fit: FitAddonType = new FitAddon();
  term.loadAddon(fit);
  term.open(host);
  fit.fit();
  if (cbs.isDisposed()) {
    term.dispose();
    return null;
  }

  let ws: WebSocket | null = null;
  try {
    ws = new WebSocket(sandboardWsUrl("/api/cockpit-attach"));
  } catch (e) {
    term.dispose();
    cbs.onError(e instanceof Error ? e.message : String(e));
    return null;
  }

  const onResize = () => {
    fit.fit();
    if (ws && ws.readyState === WebSocket.OPEN) {
      ws.send(
        JSON.stringify({
          type: "resize",
          cols: term.cols,
          rows: term.rows,
        }),
      );
    }
  };
  const ro = new ResizeObserver(onResize);
  ro.observe(host);

  ws.binaryType = "arraybuffer";
  ws.onopen = () => {
    if (cbs.isDisposed()) return;
    cbs.onOpen();
    ws?.send(
      JSON.stringify({
        type: "resize",
        cols: term.cols,
        rows: term.rows,
      }),
    );
  };
  ws.onmessage = (ev) => {
    if (typeof ev.data === "string") {
      try {
        const msg = JSON.parse(ev.data) as { type?: string; message?: string };
        if (msg.type === "error" && msg.message) {
          cbs.onError(msg.message);
        }
      } catch {
        /* ignore non-JSON text */
      }
      return;
    }
    const bytes = new Uint8Array(ev.data as ArrayBuffer);
    term.write(bytes);
  };
  ws.onerror = () => {
    if (!cbs.isDisposed()) cbs.onError("attach WebSocket error");
  };
  ws.onclose = () => {
    if (cbs.isDisposed()) return;
    cbs.onClose("attach disconnected");
  };

  const dataDisp = term.onData((data) => {
    if (ws && ws.readyState === WebSocket.OPEN) {
      ws.send(new TextEncoder().encode(data));
    }
  });

  let disposed = false;
  return {
    dispose: () => {
      if (disposed) return;
      disposed = true;
      dataDisp.dispose();
      ro.disconnect();
      if (ws) {
        ws.onopen = null;
        ws.onmessage = null;
        ws.onerror = null;
        ws.onclose = null;
        if (
          ws.readyState === WebSocket.OPEN ||
          ws.readyState === WebSocket.CONNECTING
        ) {
          ws.close();
        }
      }
      term.dispose();
    },
  };
}
