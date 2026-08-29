export type State =
  | "draft" | "shaping" | "backlog" | "claimed" | "running" | "splitting"
  | "needs_human" | "review" | "done" | "retired"
  /** Legacy wire value — treat as backlog. */
  | "ready";

export type ColumnKey =
  | "intake" | "shaping" | "backlog" | "running"
  | "needs_you" | "review" | "done" | "retired"
  | "ready";

export interface Lease {
  agent_id: string;
  granted_at: string;
  last_heartbeat: string;
  expires_at: string;
}

export interface EscalationOption { label: string; detail: string }

export interface Escalation {
  question: string;
  options: EscalationOption[];
  recommended: number;
  blocked_since: string;
  answer: string | null;
}

export interface GateRun {
  name: string;
  status: "pending" | "running" | "passed" | "failed";
  detail: string | null;
}

export interface Note { at: string; author: string; text: string }

export interface Transition {
  at: string; from: State; to: State; by: string; reason: string | null;
}

export type Origin =
  | { kind: "human" }
  | { kind: "planner" }
  | { kind: "split"; from: number }
  | { kind: "reflection" };

export interface BlockerSummary {
  id: number;
  title: string;
  state: State;
}

/** Board-owned OpenShell policy (Settings → OpenShell → Policies). */
export interface OpenShellPolicy {
  id: string;
  name: string;
  /** Inline OpenShell policy YAML text. */
  yaml: string;
}

/** GET /api/openshell/policies */
export interface OpenShellPoliciesOut {
  policies: OpenShellPolicy[];
  /** Prefill id for create forms (seeded minimal policy). */
  create_default_policy_id: string;
}

/** MCP server audience — which sandboxes may receive the inject. */
export type McpAudience = "cockpit" | "worker" | "both";

export type McpHttpAuth =
  | { kind: "none" }
  | { kind: "cockpit_bearer" }
  | { kind: "oauth"; provider: string; env: string };

export type McpTransport =
  | { kind: "http"; url: string; auth?: McpHttpAuth }
  | { kind: "stdio"; command: string; args?: string[]; cwd?: string | null };

/** Board catalog MCP server (Settings → MCP servers). */
export interface McpServerDesired {
  id: string;
  name: string;
  transport: McpTransport;
  policy_fragment_yaml?: string | null;
  provider_names?: string[];
  env?: Record<string, string>;
  audience?: McpAudience;
  shipped?: boolean;
}

/** GET /api/openshell/mcp-servers */
export interface McpServersOut {
  servers: McpServerDesired[];
}

/** Named OpenShell create-spec from the board catalog (Settings → OpenShell → Sandbox specs). */
export interface SandboxProfile {
  id: string;
  name: string;
  image: string;
  /** Policies catalog id. Resolved to YAML at sandbox create. */
  policy_id: string;
  cpu?: string | null;
  memory?: string | null;
  /** Agent CLI (`cursor` / `agy` / `claude` / `opencode` / `hermes`). Unset → Agent runtime default. */
  engine?: string | null;
  /** Model passed to the agent CLI when set. Unset → card.model → engine default. */
  model?: string | null;
  /** Providers to attach on sandbox create (`[]` = none). */
  provider_names?: string[];
  /** MCP server catalog ids to attach (`[]` = none; cockpit always attaches shipped sandboard). */
  mcp_server_ids?: string[];
  /** Non-secret env overlaid at sandbox create (profile wins on key clash). */
  env?: Record<string, string>;
  /** Seat notes injected into cold/Cockpit briefing when non-empty. */
  prompt?: string | null;
}

/** Prefill for Settings → Sandbox specs → Create. */
export interface SandboxProfileCreateDefaults {
  name: string;
  image: string;
  /** Prefill: seeded minimal Policies catalog id. */
  policy_id: string;
  cpu?: string | null;
  memory?: string | null;
  engine?: string | null;
}

export interface SandboxProfilesOut {
  profiles: SandboxProfile[];
  default_sandbox_profile_id: string | null;
  cockpit_sandbox_profile_id: string | null;
  create_defaults: SandboxProfileCreateDefaults;
}

/**
 * Per-install forge binding (Settings → Forge).
 * Work remotes live on each card's `pull_requests` after the agent reports.
 */
export interface WorkspaceBinding {
  forge: string;
}

/** Settings → Forge: poll GitHub in addition to webhooks. */
export interface WebhookPollConfig {
  enabled: boolean;
  /** Seconds between ticks (server clamps to ≥ 15). */
  interval_secs: number;
  /**
   * OpenShell provider that supplies the host poll token.
   * Required when enabled — `github-app` (mint) or a `GH_TOKEN` provider.
   */
  provider_name?: string | null;
}

/** Presence flags for sealed OpenShell mTLS material (never returns PEMs). */
export interface OpenShellMtlsStatus {
  ca: boolean;
  client_cert: boolean;
  client_key: boolean;
  complete: boolean;
}

/** Explicit gateway auth mode — operator must pick; never inferred. */
export type OpenShellAuthMode = "mtls" | "oidc";

export interface OpenShellOidcConfig {
  issuer: string;
  client_id: string;
  audience: string;
}

export interface OpenShellOidcStatus {
  logged_in: boolean;
}

/** Settings → OpenShell connectivity (gateway endpoint + mTLS or OIDC). */
export interface OpenShellSettings {
  gateway_endpoint?: string | null;
  auth_mode?: OpenShellAuthMode | null;
  oidc?: OpenShellOidcConfig | null;
  /** Write-only on PUT. */
  ca_pem?: string | null;
  client_cert_pem?: string | null;
  client_key_pem?: string | null;
  clear_mtls?: boolean;
  clear_oidc?: boolean;
  mtls?: OpenShellMtlsStatus;
  oidc_status?: OpenShellOidcStatus;
}

/** Presence flags for sealed GitHub App credentials (GET never returns secrets). */
export interface GitHubAppStatus {
  app_id: boolean;
  private_key: boolean;
  webhook_secret: boolean;
  client_id: boolean;
  client_secret: boolean;
  complete: boolean;
}

export interface GitHubAppInstallation {
  id: number;
  account_login: string;
  account_type?: string;
}

export interface GitHubAppTokenStatus {
  configured: boolean;
  provider_attached: boolean;
  expires_at?: string | null;
  error?: string | null;
}

/** Compatibility view for `/api/github-app` (material lives on provider `github-app`). */
export interface GitHubAppSettings {
  /** Non-secret — returned on GET when configured. */
  app_id?: string | null;
  client_id?: string | null;
  /** Write-only on PUT. */
  private_key_pem?: string | null;
  webhook_secret?: string | null;
  client_secret?: string | null;
  installation_id?: number | null;
  clear_installation_id?: boolean;
  clear?: boolean;
  status?: GitHubAppStatus;
  installations?: GitHubAppInstallation[];
  token_status?: GitHubAppTokenStatus;
}

/** GET `/api/github-app/repo-access` — cached App installations and repos. */
export interface GitHubRepoAccessRepo {
  full_name: string;
  installation_id: number;
  permissions?: Record<string, string>;
  last_seen_at: string;
}

export interface GitHubRepoAccessInstallation {
  id: number;
  account_login: string;
  account_type?: string;
  manage_url: string;
  repos: GitHubRepoAccessRepo[];
}

export interface GitHubRepoAccessView {
  refreshed_at?: string | null;
  last_error?: string | null;
  install_url: string;
  token_installation_id?: number | null;
  installations: GitHubRepoAccessInstallation[];
}

export interface AuthUser {
  kind: "admin" | "github";
  login: string;
}

/** GET /auth/status */
export interface AuthStatus {
  enabled: boolean;
  bootstrap: boolean;
  github_login_enabled: boolean;
  user?: AuthUser | null;
}

/** GET/PUT /api/auth/settings (local admin only). */
export interface AuthSettings {
  admin_username: string;
  allowed_users: string[];
  allowed_teams: string[];
  github_login_enabled: boolean;
  has_client_secret: boolean;
}

/** Settings → Agent runtime (process knobs; empty boards seed from compiled defaults). */
export interface AgentRuntimeConfig {
  engine: string;
  max_concurrent: number;
  agent_timeout_secs: number;
  max_attempts: number;
  /** Supervisor lease/reconcile tick (ms). */
  sweep_interval_ms: number;
  /** Board-wide standing agent policy (briefing). */
  standing_prompt: string;
}

/** GET /api/openshell/status — gateway health for Settings. */
export interface OpenShellStatus {
  healthy: boolean;
  summary: string;
  not_configured: boolean;
  error?: string | null;
  gateway_endpoint?: string | null;
  auth_mode?: OpenShellAuthMode | null;
  mtls?: OpenShellMtlsStatus;
  oidc_status?: OpenShellOidcStatus;
}

/** GET /api/openshell/providers — desired provider (secrets never included). */
export interface OpenShellProviderView {
  name: string;
  type: string;
  config: Record<string, string>;
  credential_keys: string[];
  has_credentials: boolean;
  has_refresh: boolean;
  gateway_synced?: boolean | null;
}

export interface OpenShellProvidersOut {
  providers: OpenShellProviderView[];
  gateway_reachable: boolean;
}

export interface OpenShellProviderWrite {
  name: string;
  type: string;
  config?: Record<string, string>;
  /** Write-only. Omit on update to keep sealed credentials. */
  credentials?: Record<string, string> | null;
}

export interface ProviderTypeProfile {
  id: string;
  display_name: string;
  description: string;
  category: string;
  credential_env_vars: string[];
  config_keys: string[];
  /** Non-secret config keys for Add Provider (board types / UI merge). */
  form_config_keys?: string[];
}

/** GET /api/openshell/provider-types — board customs merged with gateway builtins. */
export interface OpenShellProviderTypeEntry {
  id: string;
  display_name: string;
  description: string;
  /** board | builtin | both */
  source: string;
  credential_env_vars: string[];
  form_config_keys: string[];
  yaml?: string | null;
  shipped?: boolean | null;
}

export interface OpenShellProviderTypeWrite {
  id: string;
  yaml: string;
  form_config_keys?: string[];
}

export interface SyncProvidersOut {
  applied: string[];
  errors: { name: string; error: string }[];
}

/** GitHub-shaped PR end (base / head). */
export interface PullRequestEnd {
  repo: string;
  ref: string;
}

/** Card pull request — url + optional base/head forge facts. */
export interface PullRequest {
  url: string;
  base?: PullRequestEnd | null;
  head?: PullRequestEnd | null;
  /** True after GitHub reports this PR merged. */
  merged?: boolean;
  /** When the agent recorded this PR on the card. */
  reported_at?: string | null;
}

export type PlanStatus = "empty" | "awaiting_approval" | "approved";

export interface PlanTaskSpec {
  key: string;
  title: string;
  intent: string;
  definition_of_done: string;
  blocked_by_keys: string[];
  capability: string | null;
  /** Legacy — ignored. Name clone targets in intent/DoD. */
  repo?: RepoConfig | null;
  item_id: number | null;
}

export interface PlanArtifact {
  revision: number;
  summary: string;
  status: PlanStatus;
  tasks: PlanTaskSpec[];
  cancel_keys: string[];
  cancel_item_ids: number[];
  approved_revision: number | null;
}

/** Proposed sibling Tasks on a card (Initial plan or impl split) awaiting Approve. */
export interface TaskProposal {
  summary: string;
  tasks: PlanTaskSpec[];
}

/** Card / Task product remotes (clone + PR target). */
export interface RepoConfig {
  /** owner/name that PRs target (required when set). */
  upstream: string;
  /** Push remote; empty/omit means same as upstream. */
  fork?: string;
  /** Default main. */
  base?: string;
}

export interface WorkItem {
  id: number;
  parent: number | null;
  level: string | null;
  title: string;
  intent: string;
  definition_of_done: string | null;
  state: State;
  origin: Origin;
  above_line: boolean;
  blocked_by: number[];
  blockers?: BlockerSummary[];
  capability: string | null;
  lease: Lease | null;
  /** Hard end of this run (claim + agent_timeout). Not renewed. */
  run_deadline_at?: string | null;
  model: string | null;
  /** Display-only: card.model → profile model → engine default. */
  resolved_model?: string | null;
  progress: number;
  escalation: Escalation | null;
  gates: GateRun[];
  gate_failures: number;
  diff_added: number;
  diff_removed: number;
  notes: Note[];
  project_prompt?: string | null;
  /** Project-only: override sandbox profile; null/unset inherits global default. */
  sandbox_profile_id?: string | null;
  /** Project auto mode — supervisor queues claimable Backlog leaves. */
  auto_dispatch?: boolean;
  last_bounce_reason?: string | null;
  last_conflict_files?: string[];
  release_target: string | null;
  environment: string | null;
  /** agy conversation id; park keeps it for resume, halt clears it (and deletes the sandbox). */
  conversation_id?: string | null;
  /** Park hold: Backlog; unpark clears this and queues dispatch. */
  parked?: boolean;
  /** Board UI asked supervisor to start this Backlog card. */
  awaiting_dispatch?: boolean;
  engine?: string | null;
  pull_requests?: PullRequest[];
  /** @deprecated legacy wire — prefer pull_requests */
  pull_request?: PullRequest | null;
  /** @deprecated legacy wire — prefer pull_requests[].url */
  pr_url?: string | null;
  /** Legacy unused field; remotes come from pull_request after report. */
  repo?: RepoConfig | null;
  plan?: PlanArtifact | null;
  proposal?: TaskProposal | null;
  created_at: string;
  entered_state_at: string;
  history: Transition[];
}

/** Listed PRs on a card (`pull_requests`, else legacy singular `pull_request`). */
export function cardPullRequests(item: {
  pull_requests?: PullRequest[] | null;
  pull_request?: PullRequest | null;
  pr_url?: string | null;
}): PullRequest[] {
  if (item.pull_requests && item.pull_requests.length > 0) {
    return item.pull_requests;
  }
  if (item.pull_request?.url?.trim()) {
    return [item.pull_request];
  }
  const legacy = item.pr_url?.trim();
  return legacy ? [{ url: legacy }] : [];
}

/** PR HTML URL from card (oldest unmerged, else first listed, else legacy `pr_url`). */
export function cardPrUrl(item: {
  pull_requests?: PullRequest[] | null;
  pull_request?: PullRequest | null;
  pr_url?: string | null;
}): string | null {
  const prs = cardPullRequests(item);
  const open = prs.find((p) => !p.merged && p.url?.trim());
  const u = (open ?? prs[0])?.url?.trim() || item.pr_url?.trim();
  return u ? u : null;
}

export interface ChunkSummary { count: number; text: string }
export interface ColumnView { column: ColumnKey; summary: ChunkSummary }
export interface StoryLine { at: string; text: string }

export interface ReadyCard {
  id: number;
  title: string;
}

export interface GoalView {
  id: number;
  title: string;
  intent: string;
  progress: number;
  leaves_done: number;
  leaves_total: number;
  agents_live: number;
  needs_you: number;
  /** Project auto mode — supervisor queues claimable Backlog leaves. */
  auto_dispatch?: boolean;
  ready_to_dispatch?: ReadyCard[];
  /** `no_plan` | `awaiting_approval` | `approved_vN` */
  plan_status: string;
  /** Soft-retired Project — board hides unless "Show archived". */
  archived?: boolean;
  columns: ColumnView[];
  story: StoryLine[];
}


export interface Level {
  name: string;
  horizon: string | null;
  owner: string | null;
  elaborate: string | null;
  requires: string[];
  claimable: boolean;
}

export interface Snapshot {
  items: WorkItem[];
  levels: Level[];
  goals: GoalView[];
  server_time: string;
  /** Wall-clock cap for a run (`agents.agent_timeout_secs`). */
  agent_timeout_secs: number;
  seq: number;
  default_engine?: string;
  default_model?: string;
}

/** Wire value for Board cockpit-session status (`snake_case` from Rust). */
export type CockpitSessionStatus = "running" | "parked";

/** Supervisor-owned OpenShell sandbox lifecycle (`snake_case` from Rust). */
export type CockpitSandboxPhase =
  | "idle"
  | "starting"
  | "waiting_for_delete"
  | "provisioning"
  | "ready"
  | "stopping"
  | "error";

/**
 * Board cockpit-session singleton — Cockpit is a thin face over this record.
 * Do not invent a parallel lifecycle in React.
 */
export interface CockpitSession {
  environment?: string | null;
  conversation_id?: string | null;
  status: CockpitSessionStatus;
  /** OpenShell create/delete/ready phase for UI feedback. */
  sandbox_phase?: CockpitSandboxPhase;
  phase_detail?: string | null;
  /** ISO timestamp when `sandbox_phase` last changed. */
  phase_since?: string;
  created_at: string;
  updated_at: string;
}

/** GET /api/cockpit-session */
export interface CockpitSessionOut {
  session: CockpitSession | null;
}

export type BoardEvent =
  | { type: "upsert"; seq: number; item: WorkItem }
  | { type: "story"; seq: number; goal: number; at: string; text: string }
  | { type: "delete"; seq: number; id: number }
  | { type: "main_advanced"; seq: number; ref_name: string; commit_sha?: string | null }
  | { type: "reset"; seq: number };

/** Normalize legacy `ready` wire values to `backlog`. */
export function normState(s: State): Exclude<State, "ready"> {
  return s === "ready" ? "backlog" : s;
}

export function normColumn(c: ColumnKey): Exclude<ColumnKey, "ready"> {
  return c === "ready" ? "backlog" : c;
}

/** Which board column a state renders in. Mirrors `State::column` on the server. */
export const COLUMN_OF: Record<Exclude<State, "ready">, Exclude<ColumnKey, "ready">> = {
  draft: "intake",
  shaping: "shaping",
  backlog: "backlog",
  claimed: "running",
  running: "running",
  splitting: "running",
  needs_human: "needs_you",
  review: "review",
  done: "done",
  retired: "retired",
};

/** Board columns. Intake and Shaping live off the kanban strip. */
export const BOARD_COLUMNS: { key: Exclude<ColumnKey, "ready">; label: string; question: string }[] = [
  { key: "backlog", label: "BACKLOG", question: "What should start next?" },
  { key: "running", label: "RUNNING", question: "Is it alive, and is it worth it?" },
  { key: "needs_you", label: "⚠ NEEDS YOU", question: "How fast must I act?" },
  { key: "review", label: "REVIEW", question: "Can I approve this in 30 seconds?" },
  { key: "done", label: "DONE", question: "" },
];
