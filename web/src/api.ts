import type {
  AgentRuntimeConfig,
  AuthSettings,
  AuthStatus,
  GitHubAppSettings,
  GitHubRepoAccessView,
  McpAudience,
  McpServerDesired,
  McpServersOut,
  McpTransport,
  OpenShellPoliciesOut,
  OpenShellPolicy,
  OpenShellProviderView,
  OpenShellProviderWrite,
  OpenShellProvidersOut,
  OpenShellProviderTypeEntry,
  OpenShellProviderTypeWrite,
  OpenShellSettings,
  OpenShellStatus,
  CockpitSession,
  CockpitSessionOut,
  ProviderTypeProfile,
  SandboxProfile,
  SandboxProfilesOut,
  Snapshot,
  SyncProvidersOut,
  WebhookPollConfig,
  WorkItem,
  WorkspaceBinding,
} from "./types";

export class AuthRequiredError extends Error {
  bootstrap: boolean;
  constructor(message: string, bootstrap = false) {
    super(message);
    this.name = "AuthRequiredError";
    this.bootstrap = bootstrap;
  }
}

const fetchOpts: RequestInit = { credentials: "include" };

async function jsonOrThrow(r: Response) {
  const body = await r.json().catch(() => ({}));
  if (r.status === 401) {
    throw new AuthRequiredError(
      body?.error ?? "authentication required",
      !!body?.bootstrap,
    );
  }
  if (!r.ok) throw new Error(body?.error ?? `${r.status} ${r.statusText}`);
  return body;
}

const post = (path: string, body?: unknown) =>
  fetch(`/api${path}`, {
    ...fetchOpts,
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body ?? {}),
  }).then(jsonOrThrow);

const put = (path: string, body?: unknown) =>
  fetch(`/api${path}`, {
    ...fetchOpts,
    method: "PUT",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body ?? {}),
  }).then(jsonOrThrow);

const del = (path: string) =>
  fetch(`/api${path}`, { ...fetchOpts, method: "DELETE" }).then(async (r) => {
    if (r.status === 204) return null;
    return jsonOrThrow(r);
  });

export const api = {
  getAuthStatus: (): Promise<AuthStatus> =>
    fetch("/auth/status", fetchOpts).then(jsonOrThrow),
  bootstrap: (body: { username: string; password: string }): Promise<AuthStatus> =>
    fetch("/auth/bootstrap", {
      ...fetchOpts,
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(body),
    }).then(jsonOrThrow),
  login: (body: { username: string; password: string }): Promise<AuthStatus> =>
    fetch("/auth/login", {
      ...fetchOpts,
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(body),
    }).then(jsonOrThrow),
  logout: (): Promise<void> =>
    fetch("/auth/logout", { ...fetchOpts, method: "POST" }).then(async (r) => {
      if (r.status === 204) return;
      await jsonOrThrow(r);
    }),
  getAuthSettings: (): Promise<AuthSettings> =>
    fetch("/api/auth/settings", fetchOpts).then(jsonOrThrow),
  putAuthSettings: (body: {
    allowed_users?: string[];
    allowed_teams?: string[];
    new_password?: string | null;
  }): Promise<AuthSettings> => put("/auth/settings", body),

  board: (): Promise<Snapshot> => fetch("/api/board", fetchOpts).then(jsonOrThrow),
  digest: () => fetch("/api/digest", fetchOpts).then(jsonOrThrow),
  detail: (id: number) => fetch(`/api/items/${id}`, fetchOpts).then(jsonOrThrow),
  logs: (id: number): Promise<{ agent: string[]; openshell: string[] }> =>
    fetch(`/api/items/${id}/logs`, fetchOpts).then(jsonOrThrow),
  /**
   * Create a Project (no parent). Server requires `clone_repo` as `owner/name`
   * and seeds Initial plan — same path as MCP `create_project` / POST /api/items.
   */
  createProject: (body: {
    title: string;
    intent: string;
    clone_repo: string;
    project_prompt?: string;
  }): Promise<WorkItem> =>
    post("/items", {
      title: body.title,
      intent: body.intent,
      clone_repo: body.clone_repo,
      ...(body.project_prompt !== undefined
        ? { project_prompt: body.project_prompt }
        : {}),
    }),
  /**
   * Create a Task under a Project. Server requires `definition_of_done`,
   * refuses non-Project parents, stamps Project clone into intent when prose
   * omits it, applies optional `blocked_by`, and lands the card in Backlog.
   */
  createTask: (body: {
    parent: number;
    title: string;
    intent: string;
    definition_of_done: string;
    blocked_by?: number[];
  }): Promise<WorkItem> =>
    post("/items", {
      parent: body.parent,
      title: body.title,
      intent: body.intent,
      definition_of_done: body.definition_of_done,
      blocked_by: body.blocked_by ?? [],
    }),
  // The human verbs. Each costs the system something different.
  steer: (id: number, text: string): Promise<WorkItem> =>
    post(`/items/${id}/steer`, { text }),
  /** Write / revise Initial plan proposal (does not materialize Tasks). */
  savePlan: (
    id: number,
    body: {
      summary?: string;
      tasks: {
        key: string;
        title: string;
        intent: string;
        definition_of_done: string;
        blocked_by_keys: string[];
        capability?: string | null;
      }[];
      cancel_keys?: string[];
    },
  ): Promise<import("./types").TaskProposal> => post(`/items/${id}/plan`, body),
  park: (id: number, reason?: string): Promise<WorkItem> =>
    post(`/items/${id}/park`, { reason }),
  unpark: (id: number): Promise<WorkItem> => post(`/items/${id}/unpark`),
  halt: (id: number, reason?: string): Promise<WorkItem> =>
    post(`/items/${id}/halt`, { reason }),
  answer: (id: number, choice: string): Promise<WorkItem> =>
    post(`/items/${id}/answer`, { choice }),
  approve: (id: number): Promise<WorkItem> => post(`/items/${id}/approve`),
  /** Approve Initial plan proposal → Backlog Tasks. Id = Project or Initial plan. */
  approvePlan: (id: number): Promise<number[]> => post(`/items/${id}/approve-plan`),
  /** Seed Initial plan if missing (usually auto-seeded on create). Id = Project. */
  initPlan: (id: number): Promise<WorkItem> =>
    post(`/items/${id}/init-plan`, {}),
  /** Queue a Backlog card for the supervisor to claim. Explicit start. */
  dispatch: (id: number): Promise<WorkItem> => post(`/items/${id}/dispatch`),
  /** Play/pause Project auto mode (queue claimable Backlog leaves). */
  setAutoDispatch: (id: number, enabled: boolean): Promise<WorkItem> =>
    post(`/items/${id}/auto-dispatch`, { enabled }),
  requestChanges: (id: number, text: string): Promise<WorkItem> =>
    post(`/items/${id}/request-changes`, { text }),
  transition: (id: number, to: string, reason?: string): Promise<WorkItem> =>
    post(`/items/${id}/transition`, { to, reason }),
  update: (
    id: number,
    fields: {
      title?: string;
      intent?: string;
      definition_of_done?: string;
      engine?: string;
      project_prompt?: string;
    },
  ): Promise<WorkItem> => post(`/items/${id}/update`, fields),
  cut: (id: number, reason?: string): Promise<number[]> =>
    post(`/items/${id}/cut`, { reason }),
  unarchive: (id: number, reason?: string): Promise<number[]> =>
    post(`/items/${id}/unarchive`, { reason }),
  deleteItem: (id: number): Promise<void> =>
    fetch(`/api/items/${id}`, { ...fetchOpts, method: "DELETE" }).then(jsonOrThrow),

  listSandboxProfiles: (): Promise<SandboxProfilesOut> =>
    fetch("/api/sandbox-profiles", fetchOpts).then(jsonOrThrow),
  upsertSandboxProfile: (profile: {
    /** Omit on create — server derives a slug from `name`. */
    id?: string;
    name: string;
    image: string;
    policy_id: string;
    cpu?: string | null;
    memory?: string | null;
    engine?: string | null;
    model?: string | null;
    provider_names?: string[];
    mcp_server_ids?: string[];
    env?: Record<string, string>;
    prompt?: string | null;
  }): Promise<SandboxProfile> => post("/sandbox-profiles", profile),
  deleteSandboxProfile: (id: string): Promise<{ ok: boolean }> =>
    del(`/sandbox-profiles/${encodeURIComponent(id)}`),
  setDefaultSandboxProfile: (id: string): Promise<SandboxProfilesOut> =>
    post(`/sandbox-profiles/${encodeURIComponent(id)}/default`),
  setCockpitSandboxProfile: (id: string): Promise<SandboxProfilesOut> =>
    post(`/sandbox-profiles/${encodeURIComponent(id)}/cockpit`),
  /** Clear Cockpit override — Cockpit uses the global default again. */
  clearCockpitSandboxProfile: (): Promise<SandboxProfilesOut> =>
    post("/sandbox-profiles/cockpit/clear"),
  /** Project only. Pass `null` (or omit) to inherit the global default. */
  setProjectSandboxProfile: (
    id: number,
    sandbox_profile_id: string | null,
  ): Promise<WorkItem> =>
    post(`/items/${id}/sandbox-profile`, { sandbox_profile_id }),

  getWorkspace: (): Promise<WorkspaceBinding> =>
    fetch("/api/workspace", fetchOpts).then(jsonOrThrow),
  putWorkspace: (binding: WorkspaceBinding): Promise<WorkspaceBinding> =>
    put("/workspace", binding),

  getWebhookPoll: (): Promise<WebhookPollConfig> =>
    fetch("/api/webhook-poll", fetchOpts).then(jsonOrThrow),
  putWebhookPoll: (cfg: WebhookPollConfig): Promise<WebhookPollConfig> =>
    put("/webhook-poll", cfg),

  getAgentRuntime: (): Promise<AgentRuntimeConfig> =>
    fetch("/api/settings/agent-runtime", fetchOpts).then(jsonOrThrow),
  putAgentRuntime: (settings: AgentRuntimeConfig): Promise<AgentRuntimeConfig> =>
    put("/settings/agent-runtime", settings),

  getOpenShell: (): Promise<OpenShellSettings> =>
    fetch("/api/settings/openshell", fetchOpts).then(jsonOrThrow),
  putOpenShell: (settings: OpenShellSettings): Promise<OpenShellSettings> =>
    put("/settings/openshell", settings),
  getOpenShellStatus: (): Promise<OpenShellStatus> =>
    fetch("/api/settings/openshell/status", fetchOpts).then(jsonOrThrow),
  openshellOidcLogin: (): Promise<{
    authorize_url: string;
    redirect_uri: string;
  }> => post("/settings/openshell/oidc/login"),
  openshellOidcComplete: (body: {
    redirect: string;
  }): Promise<{ ok: boolean }> => post("/settings/openshell/oidc/complete", body),
  openshellOidcLogout: (): Promise<{ ok: boolean; error?: string | null }> =>
    post("/settings/openshell/oidc/logout"),

  getGitHubApp: (): Promise<GitHubAppSettings> =>
    fetch("/api/settings/github-app", fetchOpts).then(jsonOrThrow),
  putGitHubApp: (settings: GitHubAppSettings): Promise<GitHubAppSettings> =>
    put("/settings/github-app", settings),
  syncGitHubAppToken: (): Promise<GitHubAppSettings> =>
    post("/settings/github-app/sync-token"),
  getGitHubRepoAccess: (): Promise<GitHubRepoAccessView> =>
    fetch("/api/settings/github-app/repo-access", fetchOpts).then(jsonOrThrow),
  refreshGitHubRepoAccess: (): Promise<GitHubRepoAccessView> =>
    post("/settings/github-app/repo-access/refresh"),

  listOpenShellPolicies: (): Promise<OpenShellPoliciesOut> =>
    fetch("/api/settings/openshell/policies", fetchOpts).then(jsonOrThrow),
  getOpenShellPolicy: (id: string): Promise<OpenShellPolicy> =>
    fetch(`/api/settings/openshell/policies/${encodeURIComponent(id)}`, fetchOpts).then(
      jsonOrThrow,
    ),
  upsertOpenShellPolicy: (body: {
    /** Omit on create — server derives a slug from `name`. */
    id?: string;
    name: string;
    yaml: string;
  }): Promise<OpenShellPolicy> => post("/settings/openshell/policies", body),
  deleteOpenShellPolicy: (id: string): Promise<{ ok: boolean }> =>
    del(`/settings/openshell/policies/${encodeURIComponent(id)}`),

  listMcpServers: (): Promise<McpServersOut> =>
    fetch("/api/settings/openshell/mcp-servers", fetchOpts).then(jsonOrThrow),
  getMcpServer: (id: string): Promise<McpServerDesired> =>
    fetch(`/api/settings/openshell/mcp-servers/${encodeURIComponent(id)}`, fetchOpts).then(
      jsonOrThrow,
    ),
  upsertMcpServer: (body: {
    id?: string;
    name: string;
    transport: McpTransport;
    policy_fragment_yaml?: string | null;
    provider_names?: string[];
    env?: Record<string, string>;
    audience?: McpAudience;
  }): Promise<McpServerDesired> => post("/settings/openshell/mcp-servers", body),
  deleteMcpServer: (id: string): Promise<{ ok: boolean }> =>
    del(`/settings/openshell/mcp-servers/${encodeURIComponent(id)}`),
  discoverMcpOAuth: (body: {
    url: string;
  }): Promise<{ supported: boolean; error?: string }> =>
    post("/settings/openshell/mcp-servers/oauth/discover", body),
  startMcpOAuth: (body: {
    url: string;
    server_id?: string;
    name?: string;
    return_path?: string;
  }): Promise<{ authorize_url: string; server_id: string }> =>
    post("/settings/openshell/mcp-servers/oauth/start", body),
  disconnectMcpOAuth: (server_id: string): Promise<{ ok: boolean }> =>
    post("/settings/openshell/mcp-servers/oauth/disconnect", { server_id }),

  listOpenShellProviders: (): Promise<OpenShellProvidersOut> =>
    fetch("/api/settings/openshell/providers", fetchOpts).then(jsonOrThrow),
  createOpenShellProvider: (body: OpenShellProviderWrite): Promise<OpenShellProviderView> =>
    post("/settings/openshell/providers", body),
  updateOpenShellProvider: (
    name: string,
    body: OpenShellProviderWrite,
  ): Promise<OpenShellProviderView> =>
    put(`/settings/openshell/providers/${encodeURIComponent(name)}`, body),
  deleteOpenShellProvider: (name: string): Promise<null> =>
    del(`/settings/openshell/providers/${encodeURIComponent(name)}`),
  syncOpenShellProviders: (): Promise<SyncProvidersOut> =>
    post("/settings/openshell/providers/sync"),
  startAntigravityOAuth: (body?: {
    return_path?: string;
  }): Promise<{ authorize_url: string; redirect_uri: string }> =>
    post("/settings/openshell/providers/antigravity/oauth/start", body ?? {}),
  completeAntigravityOAuth: (body: {
    authorization_code: string;
  }): Promise<{
    ok: boolean;
    projects: { id: string; name?: string }[];
    needs_project: boolean;
    selected_project?: string;
  }> => post("/settings/openshell/providers/antigravity/oauth/complete", body),
  selectAntigravityProject: (body: {
    project_id: string;
  }): Promise<{ ok: boolean; project_id: string }> =>
    post("/settings/openshell/providers/antigravity/oauth/select-project", body),
  disconnectAntigravityOAuth: (): Promise<{ ok: boolean }> =>
    post("/settings/openshell/providers/antigravity/oauth/disconnect", {}),
  listOpenShellProviderProfiles: (): Promise<ProviderTypeProfile[]> =>
    fetch("/api/settings/openshell/provider-profiles", fetchOpts).then(jsonOrThrow),
  listOpenShellProviderTypes: (): Promise<OpenShellProviderTypeEntry[]> =>
    fetch("/api/settings/openshell/provider-types", fetchOpts).then(jsonOrThrow),
  putOpenShellProviderType: (
    body: OpenShellProviderTypeWrite,
  ): Promise<{
    id: string;
    yaml: string;
    shipped: boolean;
    form_config_keys: string[];
  }> => put("/settings/openshell/provider-types", body),
  deleteOpenShellProviderType: (id: string): Promise<null> =>
    del(`/settings/openshell/provider-types/${encodeURIComponent(id)}`),

  /** Board cockpit-session singleton — Cockpit polls this; no local lifecycle. */
  getCockpitSession: (): Promise<CockpitSessionOut> =>
    fetch("/api/cockpit-session", fetchOpts).then(jsonOrThrow),
  startCockpitSession: (body?: {
    environment?: string | null;
    conversation_id?: string | null;
  }): Promise<CockpitSession> => post("/cockpit-session", body ?? {}),
  parkCockpitSession: (): Promise<CockpitSession> => post("/cockpit-session/park"),
  resumeCockpitSession: (): Promise<CockpitSession> => post("/cockpit-session/resume"),
  stopCockpitSession: (): Promise<null> => del("/cockpit-session"),

  /**
   * Mint sandboard-cockpit MCP tokens for the logged-in user and inject mcp.json into
   * the Board cockpit sandbox. Does not return secrets to the browser.
   */
  provisionCockpitMcp: (): Promise<{
    ok: boolean;
    environment: string;
    resource: string;
    client_id: string;
    sub: string;
    expires_at: number;
    injected: boolean;
  }> => post("/cockpit-session/mcp-cred"),

  /**
   * Legacy host-mediated cockpit chat — POST /api/cockpit-chat, SSE agent lines.
   * Cockpit attach uses `/api/cockpit-attach` WebSocket instead.
   */
  streamOpsChat,
};

/** Ready payload from the cockpit-chat bridge `ready` SSE event. */
export type OpsChatReady = {
  environment: string;
  conversation_id?: string | null;
  engine: string;
};

export type OpsChatHandlers = {
  onReady?: (info: OpsChatReady) => void;
  onAgentLine?: (line: string) => void;
  onError?: (message: string) => void;
  signal?: AbortSignal;
};

/**
 * Authenticated prompt into the Running cockpit; streams SSE
 * (`ready` / `agent` / `error` / `done`). Refuses with HTTP error when the
 * Board session is absent, parked, or missing environment.
 */
export async function streamOpsChat(
  prompt: string,
  handlers: OpsChatHandlers = {},
): Promise<void> {
  const r = await fetch("/api/cockpit-chat", {
    ...fetchOpts,
    method: "POST",
    headers: {
      "Content-Type": "application/json",
      Accept: "text/event-stream",
    },
    body: JSON.stringify({ prompt }),
    signal: handlers.signal,
  });

  if (!r.ok) {
    const body = await r.json().catch(() => ({}));
    if (r.status === 401) {
      throw new AuthRequiredError(
        body?.error ?? "authentication required",
        !!body?.bootstrap,
      );
    }
    throw new Error(body?.error ?? `${r.status} ${r.statusText}`);
  }

  if (!r.body) {
    throw new Error("cockpit chat response had no body");
  }

  const reader = r.body.getReader();
  const decoder = new TextDecoder();
  let buffer = "";
  let eventName = "message";
  let dataLines: string[] = [];

  const dispatch = () => {
    if (dataLines.length === 0) {
      eventName = "message";
      return;
    }
    const data = dataLines.join("\n");
    dataLines = [];
    const name = eventName;
    eventName = "message";

    if (name === "ready") {
      try {
        handlers.onReady?.(JSON.parse(data) as OpsChatReady);
      } catch {
        /* ignore malformed ready */
      }
      return;
    }
    if (name === "agent") {
      handlers.onAgentLine?.(data);
      return;
    }
    if (name === "error") {
      handlers.onError?.(data);
      return;
    }
    // `done` and keep-alives: no-op for the UI transcript
  };

  while (true) {
    const { done, value } = await reader.read();
    if (done) break;
    buffer += decoder.decode(value, { stream: true });
    let nl: number;
    while ((nl = buffer.indexOf("\n")) >= 0) {
      let line = buffer.slice(0, nl);
      buffer = buffer.slice(nl + 1);
      if (line.endsWith("\r")) line = line.slice(0, -1);

      if (line === "") {
        dispatch();
        continue;
      }
      if (line.startsWith(":")) continue; // SSE comment / keep-alive
      if (line.startsWith("event:")) {
        eventName = line.slice(6).trim();
        continue;
      }
      if (line.startsWith("data:")) {
        const v = line.slice(5);
        dataLines.push(v.startsWith(" ") ? v.slice(1) : v);
        continue;
      }
    }
  }
  dispatch();
}

/** `4s`, `12m`, `3h 5m` — matches the server's own formatting. */
export function since(iso: string, now: number): string {
  const secs = Math.max(0, Math.floor((now - new Date(iso).getTime()) / 1000));
  if (secs < 60) return `${secs}s`;
  if (secs < 3600) return `${Math.floor(secs / 60)}m`;
  const h = Math.floor(secs / 3600);
  const m = Math.floor((secs % 3600) / 60);
  return m ? `${h}h ${m}m` : `${h}h`;
}

export const secsSince = (iso: string, now: number) =>
  Math.max(0, Math.floor((now - new Date(iso).getTime()) / 1000));

/** Seconds until an ISO deadline (0 if already past). */
export const secsUntil = (iso: string, now: number) =>
  Math.max(0, Math.floor((new Date(iso).getTime() - now) / 1000));

/** `12m 04s`, `4s`, `1h 02m` — countdown on a Running card. */
export function formatCountdown(secs: number): string {
  if (secs < 60) return `${secs}s`;
  if (secs < 3600) {
    const m = Math.floor(secs / 60);
    const s = secs % 60;
    return `${m}m ${String(s).padStart(2, "0")}s`;
  }
  const h = Math.floor(secs / 3600);
  const m = Math.floor((secs % 3600) / 60);
  return `${h}h ${String(m).padStart(2, "0")}m`;
}
