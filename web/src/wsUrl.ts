/**
 * Host for board / cockpit WebSockets.
 *
 * Vite proxies HTTP `/api/*` fine, but WSS upgrades often stall when Vite sits
 * behind Tailscale Serve (HTTPS/HTTP2 edge → Vite → sandboard). Session cookies are
 * host-scoped and ignore port, so on the Vite ports we dial the API port on the
 * same hostname and skip the proxy.
 *
 * Override with `VITE_SANDBOARD_WS_HOST` (host[:port], no scheme) when the API is
 * not on `:8080`.
 */
export function sandboardWsHost(
  loc: Pick<Location, "hostname" | "port" | "host"> = window.location,
): string {
  const override =
    typeof import.meta !== "undefined" &&
    typeof import.meta.env?.VITE_SANDBOARD_WS_HOST === "string"
      ? import.meta.env.VITE_SANDBOARD_WS_HOST.trim()
      : "";
  if (override) {
    return override.replace(/^https?:\/\//i, "").replace(/\/$/, "");
  }
  if (loc.port === "5173" || loc.port === "4173") {
    const apiPort =
      typeof import.meta !== "undefined" &&
      typeof import.meta.env?.VITE_SANDBOARD_PORT === "string" &&
      import.meta.env.VITE_SANDBOARD_PORT.trim()
        ? import.meta.env.VITE_SANDBOARD_PORT.trim()
        : "8080";
    return `${loc.hostname}:${apiPort}`;
  }
  return loc.host;
}

export function sandboardWsUrl(
  path: string,
  loc: Pick<Location, "protocol" | "hostname" | "port" | "host"> = window.location,
): string {
  const proto = loc.protocol === "https:" ? "wss:" : "ws:";
  const p = path.startsWith("/") ? path : `/${path}`;
  return `${proto}//${sandboardWsHost(loc)}${p}`;
}
