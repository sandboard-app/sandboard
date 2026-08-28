/** Stored preference — what the dropdown shows. */
export type ThemePreference = "system" | "light" | "dark";

/** Concrete theme applied to `document.documentElement.dataset.theme`. */
export type ResolvedTheme = "light" | "dark";

const STORAGE_KEY = "sandboard-theme";

export function systemResolvedTheme(): ResolvedTheme {
  if (typeof window !== "undefined" && window.matchMedia("(prefers-color-scheme: dark)").matches) {
    return "dark";
  }
  return "light";
}

export function resolveTheme(preference: ThemePreference): ResolvedTheme {
  return preference === "system" ? systemResolvedTheme() : preference;
}

export function readThemePreference(): ThemePreference {
  try {
    const stored = localStorage.getItem(STORAGE_KEY);
    if (stored === "system" || stored === "light" || stored === "dark") return stored;
    // Migrate older light/dark-only storage (already covered above).
  } catch {
    /* private mode */
  }
  return "system";
}

export function applyThemePreference(preference: ThemePreference) {
  const resolved = resolveTheme(preference);
  document.documentElement.dataset.theme = resolved;
  try {
    localStorage.setItem(STORAGE_KEY, preference);
  } catch {
    /* ignore */
  }
}

/** What `data-theme` currently resolves to on the document. */
export function readDocumentTheme(): ResolvedTheme {
  if (typeof document === "undefined") return "light";
  return document.documentElement.dataset.theme === "dark" ? "dark" : "light";
}

/** xterm.js ITheme fields — sourced from `--term-*` CSS vars when possible. */
export type XtermTheme = {
  background: string;
  foreground: string;
  cursor: string;
  cursorAccent: string;
  selectionBackground: string;
  black: string;
  red: string;
  green: string;
  yellow: string;
  blue: string;
  magenta: string;
  cyan: string;
  white: string;
  brightBlack: string;
  brightRed: string;
  brightGreen: string;
  brightYellow: string;
  brightBlue: string;
  brightMagenta: string;
  brightCyan: string;
  brightWhite: string;
  /**
   * Colors 16–255. Cursor Agent's follow-up chrome uses index **254**
   * (`xterm-bg-254`), which ignores the 16 ANSI slots — override it per theme.
   */
  extendedAnsi?: string[];
};

/**
 * Resolve a CSS custom property to a concrete color xterm can parse.
 * `getPropertyValue('--x')` often returns `var(...)` / `color-mix(...)` literals
 * that ThemeService silently drops — which is why live theme switches looked wrong
 * while a hard reload (fallbacks / already-computed cascade) looked fine.
 */
function cssColor(name: string, fallback: string): string {
  if (typeof document === "undefined") return fallback;
  const probe = document.createElement("div");
  probe.style.cssText =
    "position:absolute;left:-9999px;width:1px;height:1px;pointer-events:none;" +
    `background-color:var(${name})`;
  document.documentElement.appendChild(probe);
  const resolved = getComputedStyle(probe).backgroundColor.trim();
  probe.remove();
  if (
    !resolved ||
    resolved === "transparent" ||
    resolved === "rgba(0, 0, 0, 0)"
  ) {
    return fallback;
  }
  return resolved;
}

/**
 * Sparse overrides for xterm colors 16–255 (`extendedAnsi[i]` → color `i+16`).
 * Cursor's follow-up chrome paints with index **254** (`xterm-bg-254`) — a
 * near-white grey that ignores the 16 ANSI slots. Remap only that slot (and
 * its immediate neighbor) to `--term-followup-bg`.
 *
 * Do not pull 232–236 or 255 into that surface: OpenCode (and others) use
 * those as ordinary greys — 255 as bright foreground text — so remapping them
 * made typed input the same color as the background.
 */
function extendedAnsiForDocument(dark: boolean): string[] {
  const ext: string[] = [];
  const follow = dark
    ? cssColor("--term-followup-bg", "#2a3540")
    : cssColor("--term-followup-bg", "#e8eee9");
  for (const idx of [253, 254]) {
    ext[idx - 16] = follow;
  }
  return ext;
}

/** Build an xterm theme from live `--term-*` CSS vars (light and dark). */
export function xtermThemeFromDocument(): XtermTheme {
  const dark = readDocumentTheme() === "dark";
  const bg = cssColor("--term-bg", dark ? "#0f1a21" : "#eef3ef");
  const fg = cssColor("--term-fg", dark ? "#e1e5e8" : "#29353c");
  const cursor = cssColor("--term-cursor", dark ? "#4d95e0" : "#2377d2");
  return {
    background: bg,
    foreground: fg,
    cursor,
    cursorAccent: bg,
    selectionBackground: cssColor(
      "--term-selection",
      dark ? "rgba(77, 149, 224, 0.32)" : "rgba(35, 119, 210, 0.28)",
    ),
    black: cssColor("--term-black", dark ? "#0b151b" : "#29353c"),
    red: cssColor("--term-red", dark ? "#e86a54" : "#dd5942"),
    green: cssColor("--term-green", dark ? "#3daf6e" : "#19874d"),
    yellow: cssColor("--term-yellow", dark ? "#e4c35a" : "#c9a227"),
    blue: cssColor("--term-blue", dark ? "#4d95e0" : "#2377d2"),
    magenta: cssColor("--term-magenta", dark ? "#d4785a" : "#a34b2e"),
    cyan: cssColor("--term-cyan", dark ? "#8fb4c4" : "#5a7a88"),
    white: cssColor("--term-white", dark ? "#e1e5e8" : "#f7faf8"),
    brightBlack: cssColor("--term-bright-black", dark ? "#7a848a" : "#6d767b"),
    brightRed: cssColor("--term-red", dark ? "#e86a54" : "#dd5942"),
    brightGreen: cssColor("--term-green", dark ? "#3daf6e" : "#19874d"),
    brightYellow: cssColor("--term-yellow", dark ? "#e4c35a" : "#c9a227"),
    brightBlue: cssColor("--term-blue", dark ? "#4d95e0" : "#2377d2"),
    brightMagenta: cssColor("--term-magenta", dark ? "#d4785a" : "#a34b2e"),
    brightCyan: cssColor("--term-cyan", dark ? "#8fb4c4" : "#5a7a88"),
    brightWhite: cssColor("--term-bright-white", "#ffffff"),
    extendedAnsi: extendedAnsiForDocument(dark),
  };
}
