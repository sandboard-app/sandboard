import { useCallback, useEffect, useState } from "react";
import { api } from "../api.js";
import type { SettingsSection } from "../location.js";
import type {
  AgentRuntimeConfig,
  AuthSettings,
  GitHubAppTokenStatus,
  GitHubRepoAccessView,
  OpenShellProviderView,
  WebhookPollConfig,
  WorkspaceBinding,
} from "../types.js";
import { OpenShellMcpServersPanel } from "./OpenShellMcpServers.js";
import {
  OpenShellPanel,
  type OpenShellTab,
} from "./OpenShellSettings.js";

export { OpenShellPanelView } from "./OpenShellSettings.js";
export { OpenShellProvidersPanelView } from "./OpenShellProviders.js";
export { OpenShellPoliciesPanelView } from "./OpenShellPolicies.js";
export { OpenShellProviderTypesPanelView } from "./OpenShellProviderTypes.js";
export { OpenShellMcpServersPanelView } from "./OpenShellMcpServers.js";
export {
  OpenShellReadinessStripView,
  gatewayReady,
  gatewayMtlsReady,
  sandboxSpecReady,
} from "./OpenShellReadiness.js";
export {
  ProjectSandboxPicker,
  SandboxesPanelView,
  sandboxHasNoProviders,
} from "./OpenShellProfiles.js";
export type { SettingsSection } from "../location.js";

const SECTIONS: { id: SettingsSection; label: string; stub?: boolean }[] = [
  { id: "openshell", label: "OpenShell" },
  { id: "mcp-servers", label: "MCP servers" },
  { id: "access", label: "Access" },
  // Nav label is Forge — "Workspace" implied a single work repo (upstream/fork).
  { id: "workspace", label: "Forge" },
  { id: "repo-access", label: "Repo access" },
  { id: "agent-runtime", label: "Agent runtime" },
];

const emptyWorkspace = (): WorkspaceBinding => ({
  forge: "github",
});

const emptyWebhookPoll = (): WebhookPollConfig => ({
  enabled: false,
  interval_secs: 60,
  provider_name: null,
});

/** Providers Forge can mint/read a host GitHub token from. */
export function forgePollCredentialOptions(
  providers: OpenShellProviderView[],
): OpenShellProviderView[] {
  return providers.filter(
    (p) =>
      p.type === "github-app" ||
      p.name === "github-app" ||
      p.type === "github" ||
      (p.credential_keys ?? []).some((k) => k === "GH_TOKEN" || k === "GITHUB_TOKEN"),
  );
}

export function forgePollCredentialReady(
  provider: OpenShellProviderView | undefined,
  githubAppConfigured: boolean,
): boolean {
  if (!provider) return false;
  if (provider.type === "github-app" || provider.name === "github-app") {
    return githubAppConfigured;
  }
  return !!(
    provider.has_credentials &&
    (provider.credential_keys ?? []).some((k) => k === "GH_TOKEN" || k === "GITHUB_TOKEN")
  );
}

const emptyAgentRuntime = (): AgentRuntimeConfig => ({
  engine: "cursor",
  max_concurrent: 2,
  agent_timeout_secs: 1800,
  max_attempts: 3,
  sweep_interval_ms: 2000,
  standing_prompt: "",
});

/**
 * Settings shell — OpenShell (incl. shipped github-app provider on
 * Providers), MCP servers, Access, Forge, Agent runtime.
 * Section / OpenShell tab may be controlled by the URL location contract.
 */
export function Settings({
  section: sectionProp,
  openShellTab,
  onSectionChange,
  onOpenShellTabChange,
}: {
  section?: SettingsSection;
  openShellTab?: OpenShellTab;
  onSectionChange?: (section: SettingsSection) => void;
  onOpenShellTabChange?: (tab: OpenShellTab) => void;
} = {}) {
  const [internalSection, setInternalSection] =
    useState<SettingsSection>("openshell");
  const section = sectionProp ?? internalSection;
  const setSection = (next: SettingsSection) => {
    onSectionChange?.(next);
    if (sectionProp === undefined) setInternalSection(next);
  };

  return (
    <div className="settings" data-testid="settings">
      <header className="settings-hero">
        <h1>Settings</h1>
        <p className="settings-lede">
          OpenShell, MCP servers, access, forge polling, GitHub App repo access, and
          agent runtime.
        </p>
      </header>

      <div className="settings-body">
        <nav className="settings-nav" aria-label="Settings sections">
          {SECTIONS.map((s) => (
            <button
              key={s.id}
              type="button"
              className={`settings-nav-btn ${section === s.id ? "active" : ""}`}
              aria-current={section === s.id ? "page" : undefined}
              onClick={() => setSection(s.id)}
              data-testid={`settings-nav-${s.id}`}
            >
              {s.label}
              {s.stub && <span className="dim settings-stub-tag">soon</span>}
            </button>
          ))}
        </nav>

        <div className="settings-panel" data-testid={`settings-panel-${section}`}>
          {section === "openshell" ? (
            <OpenShellPanel
              activeTab={openShellTab}
              onTabChange={onOpenShellTabChange}
            />
          ) : section === "mcp-servers" ? (
            <OpenShellMcpServersPanel />
          ) : section === "access" ? (
            <AccessPanel />
          ) : section === "workspace" ? (
            <WorkspacePanel />
          ) : section === "repo-access" ? (
            <RepoAccessPanel />
          ) : (
            <AgentRuntimePanel />
          )}
        </div>
      </div>
    </div>
  );
}

/** Presentational Access form — local admin allowlists + password. */
export function AccessPanelView({
  adminUsername,
  allowedUsers,
  allowedTeams,
  newPassword,
  githubLoginEnabled,
  hasClientSecret,
  busy,
  error,
  savedHint,
  onAllowedUsersChange,
  onAllowedTeamsChange,
  onNewPasswordChange,
  onSave,
}: {
  adminUsername: string;
  allowedUsers: string;
  allowedTeams: string;
  newPassword: string;
  githubLoginEnabled: boolean;
  hasClientSecret: boolean;
  busy?: boolean;
  error?: string | null;
  savedHint?: string | null;
  onAllowedUsersChange: (next: string) => void;
  onAllowedTeamsChange: (next: string) => void;
  onNewPasswordChange: (next: string) => void;
  onSave: () => void;
}) {
  return (
    <section aria-labelledby="access-title" data-testid="access-panel">
      <h2 id="access-title">Access</h2>
      <p className="dim">
        Local admin <strong>{adminUsername || "…"}</strong> can always sign in.
        GitHub sign-in is limited to the users and org teams below. Any signed-in
        operator can edit this for now.
      </p>

      {error && <div className="err">{error}</div>}
      {savedHint && (
        <p className="dim" data-testid="access-saved-hint">
          {savedHint}
        </p>
      )}

      <div className="openshell-health" data-testid="access-github-status">
        <div className="openshell-health-row">
          <span className="dim">GitHub login</span>
          <strong>
            {githubLoginEnabled
              ? "Enabled"
              : hasClientSecret
                ? "Incomplete App config"
                : "Needs Client secret (GitHub App)"}
          </strong>
        </div>
      </div>

      <form
        className="sandbox-profile-form workspace-form"
        data-testid="access-form"
        onSubmit={(e) => {
          e.preventDefault();
          onSave();
        }}
      >
        <label>
          Allowed GitHub users
          <textarea
            className="search-input"
            rows={3}
            value={allowedUsers}
            disabled={busy}
            placeholder="one login per line, e.g. shanemcd"
            onChange={(e) => onAllowedUsersChange(e.target.value)}
            data-testid="access-field-users"
          />
        </label>
        <label>
          Allowed org teams
          <textarea
            className="search-input"
            rows={3}
            value={allowedTeams}
            disabled={busy}
            placeholder="one org/team_slug per line"
            onChange={(e) => onAllowedTeamsChange(e.target.value)}
            data-testid="access-field-teams"
          />
        </label>
        <label>
          New admin password
          <input
            className="search-input"
            type="password"
            autoComplete="new-password"
            value={newPassword}
            disabled={busy}
            placeholder="leave blank to keep current"
            onChange={(e) => onNewPasswordChange(e.target.value)}
            data-testid="access-field-password"
          />
        </label>
        <div className="btns">
          <button type="submit" className="primary" disabled={busy} data-testid="access-save">
            Save
          </button>
        </div>
      </form>
    </section>
  );
}

function AccessPanel() {
  const [adminUsername, setAdminUsername] = useState("");
  const [allowedUsers, setAllowedUsers] = useState("");
  const [allowedTeams, setAllowedTeams] = useState("");
  const [newPassword, setNewPassword] = useState("");
  const [githubLoginEnabled, setGithubLoginEnabled] = useState(false);
  const [hasClientSecret, setHasClientSecret] = useState(false);
  const [loading, setLoading] = useState(true);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [savedHint, setSavedHint] = useState<string | null>(null);

  const apply = useCallback((cfg: AuthSettings) => {
    setAdminUsername(cfg.admin_username);
    setAllowedUsers(cfg.allowed_users.join("\n"));
    setAllowedTeams(cfg.allowed_teams.join("\n"));
    setGithubLoginEnabled(cfg.github_login_enabled);
    setHasClientSecret(cfg.has_client_secret);
    setNewPassword("");
  }, []);

  const refresh = useCallback(() => {
    setBusy(true);
    return api
      .getAuthSettings()
      .then((cfg) => {
        apply(cfg);
        setError(null);
      })
      .catch((e) => setError(String(e)))
      .finally(() => {
        setBusy(false);
        setLoading(false);
      });
  }, [apply]);

  useEffect(() => {
    refresh();
  }, [refresh]);

  return (
    <AccessPanelView
      adminUsername={adminUsername}
      allowedUsers={allowedUsers}
      allowedTeams={allowedTeams}
      newPassword={newPassword}
      githubLoginEnabled={githubLoginEnabled}
      hasClientSecret={hasClientSecret}
      busy={busy || loading}
      error={error}
      savedHint={savedHint}
      onAllowedUsersChange={(next) => {
        setSavedHint(null);
        setAllowedUsers(next);
      }}
      onAllowedTeamsChange={(next) => {
        setSavedHint(null);
        setAllowedTeams(next);
      }}
      onNewPasswordChange={(next) => {
        setSavedHint(null);
        setNewPassword(next);
      }}
      onSave={() => {
        setBusy(true);
        setError(null);
        setSavedHint(null);
        const users = allowedUsers
          .split(/[\n,]+/)
          .map((s) => s.trim())
          .filter(Boolean);
        const teams = allowedTeams
          .split(/[\n,]+/)
          .map((s) => s.trim())
          .filter(Boolean);
        const body: {
          allowed_users: string[];
          allowed_teams: string[];
          new_password?: string;
        } = { allowed_users: users, allowed_teams: teams };
        if (newPassword.trim()) body.new_password = newPassword.trim();
        api
          .putAuthSettings(body)
          .then((cfg) => {
            apply(cfg);
            setSavedHint("Saved access settings.");
          })
          .catch((e) => setError(String(e)))
          .finally(() => setBusy(false));
      }}
    />
  );
}

/** Presentational Forge form — exported for UI tests without fetch. */
export function WorkspacePanelView({
  draft,
  poll,
  credentialOptions = [],
  githubAppConfigured = false,
  githubAppTokenStatus,
  busy,
  error,
  savedHint,
  onDraftChange,
  onPollChange,
  onSave,
}: {
  draft: WorkspaceBinding;
  poll: WebhookPollConfig;
  /** OpenShell providers that can supply a host poll token. */
  credentialOptions?: OpenShellProviderView[];
  /** True when shipped github-app has App id + key + installation. */
  githubAppConfigured?: boolean;
  githubAppTokenStatus?: GitHubAppTokenStatus;
  busy?: boolean;
  error?: string | null;
  savedHint?: string | null;
  onDraftChange: (next: WorkspaceBinding) => void;
  onPollChange: (next: WebhookPollConfig) => void;
  onSave: () => void;
}) {
  const selectedName = (poll.provider_name ?? "").trim();
  const selected = credentialOptions.find((p) => p.name === selectedName);
  const selectedReady = forgePollCredentialReady(selected, githubAppConfigured);
  const hasCandidates = credentialOptions.length > 0;
  const tokenError = githubAppTokenStatus?.error?.trim();

  let authLabel = "credential not selected";
  let authClass = "openshell-health-bad";
  let authDetail =
    "Choose which OpenShell provider supplies the GitHub token for polling.";
  if (!hasCandidates) {
    authLabel = "no GitHub credentials";
    authDetail =
      "Create a provider under OpenShell → Providers: either shipped type github-app (App ID, private key, installation — mints GH_TOKEN) or type github with a pasted GH_TOKEN (PAT). Then select it here.";
  } else if (selectedName && !selected) {
    authLabel = "selected provider missing";
    authDetail = `Provider “${selectedName}” is not on the board anymore. Pick another credential or recreate it under OpenShell → Providers.`;
  } else if (selected && selectedReady) {
    authLabel = `${selected.name} ready`;
    authClass = "openshell-health-ok";
    authDetail =
      selected.type === "github-app" || selected.name === "github-app"
        ? "App installation token will be minted on each poll tick when needed."
        : "Sealed GH_TOKEN on this provider will be used for poll REST calls.";
  } else if (selected && !selectedReady) {
    authLabel = `${selected.name} not ready`;
    authDetail =
      selected.type === "github-app" || selected.name === "github-app"
        ? "Finish github-app: App ID, private key, and installation under OpenShell → Providers."
        : "This provider has no sealed GH_TOKEN yet — edit it under OpenShell → Providers and paste a token.";
  }

  return (
    <section aria-labelledby="workspace-title" data-testid="workspace-panel">
      <h2 id="workspace-title">Forge</h2>
      <p className="dim">
        Choose a forge and optionally poll for PR check updates. Polling needs a
        GitHub credential from OpenShell → Providers (
        <code>github-app</code> or a <code>GH_TOKEN</code>).
      </p>

      {error && <div className="err">{error}</div>}
      {savedHint && (
        <p className="dim" data-testid="workspace-saved-hint">
          {savedHint}
        </p>
      )}

      <div className="openshell-health" data-testid="workspace-poll-auth">
        <div className="openshell-health-row">
          <span>Poll credential</span>
          <strong className={authClass} data-testid="workspace-poll-auth-label">
            {authLabel}
          </strong>
        </div>
        <p className="dim" data-testid="workspace-poll-auth-detail">
          {authDetail}{" "}
          <a href="/settings/openshell/providers">OpenShell → Providers</a>
        </p>
        {poll.enabled && (!selectedName || !selectedReady) && (
          <p className="err" data-testid="workspace-poll-auth-warn">
            Polling is enabled but will skip until you select a ready credential
            and save.
          </p>
        )}
        {selected?.type === "github-app" && tokenError && (
          <p className="err" data-testid="workspace-poll-auth-error">
            Last mint error: {tokenError}
          </p>
        )}
      </div>

      <form
        className="sandbox-profile-form workspace-form"
        data-testid="workspace-form"
        onSubmit={(e) => {
          e.preventDefault();
          onSave();
        }}
      >
        <label>
          Forge
          <select
            className="search-input"
            value={draft.forge}
            disabled={busy}
            onChange={(e) => onDraftChange({ ...draft, forge: e.target.value })}
            data-testid="workspace-field-forge"
          >
            <option value="github">GitHub</option>
            <option value="gitlab" disabled>
              GitLab (future)
            </option>
          </select>
        </label>
        <fieldset className="workspace-poll-fieldset" data-testid="workspace-poll">
          <legend>Webhook polling fallback</legend>
          <label className="workspace-poll-enabled">
            <input
              type="checkbox"
              checked={poll.enabled}
              disabled={busy}
              onChange={(e) =>
                onPollChange({ ...poll, enabled: e.target.checked })
              }
              data-testid="workspace-poll-enabled"
            />
            Poll GitHub on an interval (in addition to webhooks)
          </label>
          <label>
            Credential provider
            <select
              className="search-input"
              value={selectedName}
              disabled={busy || !hasCandidates}
              onChange={(e) =>
                onPollChange({
                  ...poll,
                  provider_name: e.target.value.trim() || null,
                })
              }
              data-testid="workspace-poll-credential"
            >
              <option value="">
                {hasCandidates
                  ? "Select a credential…"
                  : "No GitHub credentials yet — create one first"}
              </option>
              {credentialOptions.map((p) => {
                const ready = forgePollCredentialReady(p, githubAppConfigured);
                return (
                  <option key={p.name} value={p.name}>
                    {p.name} ({p.type})
                    {ready ? "" : " — not ready"}
                  </option>
                );
              })}
            </select>
            <span className="dim sandbox-field-hint">
              Required when polling is on. Use <code>github-app</code> for
              App-minted tokens, or a <code>github</code> provider with a
              pasted <code>GH_TOKEN</code>.
            </span>
          </label>
          <label>
            Interval (seconds)
            <input
              className="search-input"
              type="number"
              min={15}
              step={1}
              value={poll.interval_secs}
              disabled={busy || !poll.enabled}
              onChange={(e) =>
                onPollChange({
                  ...poll,
                  interval_secs: Number(e.target.value) || 60,
                })
              }
              data-testid="workspace-poll-interval"
            />
            <span className="dim sandbox-field-hint">
              Minimum 15s. Completes merged Review cards and advances main when
              the tip moves.
            </span>
          </label>
        </fieldset>

        <div className="btns">
          <button type="submit" className="primary" disabled={busy} data-testid="workspace-save">
            Save
          </button>
        </div>
      </form>
    </section>
  );
}

function WorkspacePanel() {
  const [draft, setDraft] = useState<WorkspaceBinding>(emptyWorkspace);
  const [poll, setPoll] = useState<WebhookPollConfig>(emptyWebhookPoll);
  const [providers, setProviders] = useState<OpenShellProviderView[]>([]);
  const [githubAppConfigured, setGithubAppConfigured] = useState(false);
  const [githubAppTokenStatus, setGithubAppTokenStatus] =
    useState<GitHubAppTokenStatus>();
  const [loading, setLoading] = useState(true);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [savedHint, setSavedHint] = useState<string | null>(null);

  const refresh = useCallback(() => {
    setLoading(true);
    return Promise.all([
      api.getWorkspace(),
      api.getWebhookPoll(),
      api.listOpenShellProviders().catch(() => ({ providers: [], gateway_reachable: false })),
      api.getGitHubApp().catch(() => null),
    ])
      .then(([ws, wp, os, gh]) => {
        setDraft({
          forge: ws.forge || "github",
        });
        setPoll({
          enabled: !!wp.enabled,
          interval_secs: wp.interval_secs || 60,
          provider_name: wp.provider_name?.trim() || null,
        });
        setProviders(os.providers ?? []);
        setGithubAppConfigured(!!gh?.token_status?.configured);
        setGithubAppTokenStatus(gh?.token_status);
        setError(null);
      })
      .catch((e) => setError(String(e)))
      .finally(() => setLoading(false));
  }, []);

  useEffect(() => {
    refresh();
  }, [refresh]);

  if (loading && !error) {
    return (
      <section aria-labelledby="workspace-title" data-testid="workspace-panel">
        <h2 id="workspace-title">Forge</h2>
        <p className="dim">loading…</p>
      </section>
    );
  }

  return (
    <WorkspacePanelView
      draft={draft}
      poll={poll}
      credentialOptions={forgePollCredentialOptions(providers)}
      githubAppConfigured={githubAppConfigured}
      githubAppTokenStatus={githubAppTokenStatus}
      busy={busy}
      error={error}
      savedHint={savedHint}
      onDraftChange={(next) => {
        setSavedHint(null);
        setDraft(next);
      }}
      onPollChange={(next) => {
        setSavedHint(null);
        setPoll(next);
      }}
      onSave={() => {
        setBusy(true);
        setError(null);
        setSavedHint(null);
        const body: WorkspaceBinding = {
          forge: draft.forge.trim() || "github",
        };
        const pollBody: WebhookPollConfig = {
          enabled: poll.enabled,
          interval_secs: Math.max(15, Number(poll.interval_secs) || 60),
          provider_name: poll.provider_name?.trim() || null,
        };
        Promise.all([api.putWorkspace(body), api.putWebhookPoll(pollBody)])
          .then(([saved, savedPoll]) => {
            setDraft({
              forge: saved.forge,
            });
            setPoll({
              enabled: !!savedPoll.enabled,
              interval_secs: savedPoll.interval_secs || 60,
              provider_name: savedPoll.provider_name?.trim() || null,
            });
            setSavedHint("Saved. Forge and poll settings update board state.");
            return refresh();
          })
          .catch((e) => setError(String(e)))
          .finally(() => setBusy(false));
      }}
    />
  );
}

const emptyRepoAccess = (): GitHubRepoAccessView => ({
  install_url: "https://github.com/settings/installations",
  installations: [],
});

function permissionSummary(permissions?: Record<string, string>): string {
  if (!permissions) return "";
  const granted = Object.entries(permissions)
    .filter(([, v]) => v === "true" || v === "write" || v === "admin")
    .map(([k]) => k);
  return granted.length ? granted.join(", ") : "";
}

/** Presentational GitHub App repo-access cache — exported for UI tests. */
export function RepoAccessPanelView({
  view,
  busy,
  error,
  onRefresh,
}: {
  view: GitHubRepoAccessView;
  busy?: boolean;
  error?: string | null;
  onRefresh: () => void;
}) {
  const refreshed = view.refreshed_at
    ? new Date(view.refreshed_at).toLocaleString()
    : "never";
  return (
    <section aria-labelledby="repo-access-title" data-testid="repo-access-panel">
      <h2 id="repo-access-title">Repo access</h2>
      <p className="dim">
        Repositories each GitHub App installation can see. When an agent needs
        to push or calls <code>report_pull_request</code>, sandboard looks up{" "}
        <code>owner/name</code> here and mints <code>GH_TOKEN</code> for that
        installation onto the live sandbox.{" "}
        <code>GITHUB_INSTALLATION_ID</code> remains the fallback for the{" "}
        <code>github-app</code> provider — not the routing source. If a repo is
        missing, install the App, Refresh, then Unpark.
      </p>

      {error && <div className="err">{error}</div>}
      {view.last_error && (
        <p className="err" data-testid="repo-access-last-error">
          Last refresh error: {view.last_error}
        </p>
      )}

      <div className="openshell-health" data-testid="repo-access-status">
        <div className="openshell-health-row">
          <span className="dim">Last refresh</span>
          <strong data-testid="repo-access-refreshed-at">{refreshed}</strong>
        </div>
        {view.token_installation_id ? (
          <p className="dim" data-testid="repo-access-token-installation">
            Singleton <code>github-app</code> fallback uses installation #
            {view.token_installation_id}.
          </p>
        ) : (
          <p className="dim" data-testid="repo-access-token-installation">
            No installation selected for token minting yet (OpenShell → Providers).
          </p>
        )}
      </div>

      <div className="btns">
        <button
          type="button"
          className="primary"
          disabled={busy}
          onClick={onRefresh}
          data-testid="repo-access-refresh"
        >
          Refresh
        </button>
        <a
          className="button-link"
          href={view.install_url || "https://github.com/settings/installations"}
          target="_blank"
          rel="noreferrer"
          data-testid="repo-access-install-link"
        >
          Install / add repos on GitHub
        </a>
      </div>

      {view.installations.length === 0 ? (
        <div className="settings-placeholder" data-testid="repo-access-empty">
          No installations cached yet. Configure the <code>github-app</code>{" "}
          provider, then Refresh — or install the App on GitHub first.
        </div>
      ) : (
        <ul className="repo-access-install-list" data-testid="repo-access-installations">
          {view.installations.map((inst) => (
            <li
              key={inst.id}
              className="repo-access-install"
              data-testid={`repo-access-install-${inst.id}`}
            >
              <div className="openshell-health-row">
                <strong>
                  {inst.account_login || "unknown"}{" "}
                  <span className="dim">
                    ({inst.account_type || "account"}) #{inst.id}
                  </span>
                </strong>
                <a
                  href={inst.manage_url}
                  target="_blank"
                  rel="noreferrer"
                  data-testid={`repo-access-manage-${inst.id}`}
                >
                  Manage repos
                </a>
              </div>
              {inst.repos.length === 0 ? (
                <p className="dim">No repositories visible to this installation.</p>
              ) : (
                <ul className="repo-access-repo-list">
                  {inst.repos.map((repo) => (
                    <li key={repo.full_name} data-testid={`repo-access-repo-${repo.full_name}`}>
                      <code>{repo.full_name}</code>
                      {permissionSummary(repo.permissions) ? (
                        <span className="dim"> {permissionSummary(repo.permissions)}</span>
                      ) : null}
                    </li>
                  ))}
                </ul>
              )}
            </li>
          ))}
        </ul>
      )}
    </section>
  );
}

function RepoAccessPanel() {
  const [view, setView] = useState<GitHubRepoAccessView>(emptyRepoAccess);
  const [loading, setLoading] = useState(true);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(() => {
    return api
      .getGitHubRepoAccess()
      .then((next) => {
        setView(next);
        setError(null);
      })
      .catch((e) => setError(String(e)))
      .finally(() => {
        setBusy(false);
        setLoading(false);
      });
  }, []);

  useEffect(() => {
    load();
  }, [load]);

  if (loading && !error) {
    return (
      <section aria-labelledby="repo-access-title" data-testid="repo-access-panel">
        <h2 id="repo-access-title">Repo access</h2>
        <p className="dim">loading…</p>
      </section>
    );
  }

  return (
    <RepoAccessPanelView
      view={view}
      busy={busy}
      error={error}
      onRefresh={() => {
        setBusy(true);
        setError(null);
        api
          .refreshGitHubRepoAccess()
          .then((next) => {
            setView(next);
            setError(null);
          })
          .catch((e) => setError(String(e)))
          .finally(() => setBusy(false));
      }}
    />
  );
}

/** Presentational Agent runtime form — exported for UI tests without fetch. */
export function AgentRuntimePanelView({
  draft,
  busy,
  error,
  savedHint,
  onDraftChange,
  onSave,
}: {
  draft: AgentRuntimeConfig;
  busy?: boolean;
  error?: string | null;
  savedHint?: string | null;
  onDraftChange: (next: AgentRuntimeConfig) => void;
  onSave: () => void;
}) {
  return (
    <section aria-labelledby="agent-runtime-title" data-testid="agent-runtime-panel">
      <h2 id="agent-runtime-title">Agent runtime</h2>
      <p className="dim">
        Concurrency, timeouts, sweep interval, the board standing prompt, and the
        fallback engine when a sandbox spec does not set one. Per-run engine is on
        OpenShell → Sandbox specs. Card branches are fixed <code>sandboard/card-*</code>.
      </p>

      {error && <div className="err">{error}</div>}
      {savedHint && (
        <p className="dim" data-testid="agent-runtime-saved-hint">
          {savedHint}
        </p>
      )}

      <form
        className="sandbox-profile-form workspace-form"
        data-testid="agent-runtime-form"
        onSubmit={(e) => {
          e.preventDefault();
          onSave();
        }}
      >
        <label>
          Default engine
          <select
            className="search-input"
            value={draft.engine}
            disabled={busy}
            onChange={(e) => onDraftChange({ ...draft, engine: e.target.value })}
            data-testid="agent-runtime-field-engine"
          >
            <option value="cursor">cursor</option>
            <option value="agy">agy</option>
            <option value="claude">claude</option>
            <option value="opencode">opencode</option>
            <option value="hermes">hermes</option>
          </select>
        </label>

        <div className="sandbox-profile-form-row">
          <label>
            Max concurrent
            <input
              className="search-input"
              type="number"
              min={1}
              value={draft.max_concurrent}
              disabled={busy}
              onChange={(e) =>
                onDraftChange({
                  ...draft,
                  max_concurrent: Math.max(1, Number(e.target.value) || 1),
                })
              }
              data-testid="agent-runtime-field-max-concurrent"
            />
          </label>
          <label>
            Agent timeout (secs)
            <input
              className="search-input"
              type="number"
              min={1}
              value={draft.agent_timeout_secs}
              disabled={busy}
              onChange={(e) =>
                onDraftChange({
                  ...draft,
                  agent_timeout_secs: Math.max(1, Number(e.target.value) || 1),
                })
              }
              data-testid="agent-runtime-field-timeout"
            />
          </label>
        </div>

        <label>
          Standing prompt
          <textarea
            className="search-input"
            rows={12}
            value={draft.standing_prompt}
            disabled={busy}
            onChange={(e) =>
              onDraftChange({ ...draft, standing_prompt: e.target.value })
            }
            data-testid="agent-runtime-field-standing-prompt"
          />
          <span className="dim sandbox-field-hint">
            Board-wide agent policy injected on every claim. Project{" "}
            <code>project_prompt</code> is for Project-only extras.
          </span>
        </label>

        <div className="sandbox-profile-form-row">
          <label>
            Max attempts
            <input
              className="search-input"
              type="number"
              min={1}
              value={draft.max_attempts}
              disabled={busy}
              onChange={(e) =>
                onDraftChange({
                  ...draft,
                  max_attempts: Math.max(1, Number(e.target.value) || 1),
                })
              }
              data-testid="agent-runtime-field-max-attempts"
            />
          </label>
          <label>
            Sweep interval (ms)
            <input
              className="search-input"
              type="number"
              min={100}
              value={draft.sweep_interval_ms}
              disabled={busy}
              onChange={(e) =>
                onDraftChange({
                  ...draft,
                  sweep_interval_ms: Math.max(100, Number(e.target.value) || 2000),
                })
              }
              data-testid="agent-runtime-field-sweep"
            />
            <span className="dim sandbox-field-hint">
              How often the supervisor checks overdue run deadlines.
            </span>
          </label>
        </div>

        <div className="btns">
          <button
            type="submit"
            className="primary"
            disabled={busy}
            data-testid="agent-runtime-save"
          >
            Save
          </button>
        </div>
      </form>
    </section>
  );
}

function AgentRuntimePanel() {
  const [draft, setDraft] = useState<AgentRuntimeConfig>(emptyAgentRuntime);
  const [loading, setLoading] = useState(true);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [savedHint, setSavedHint] = useState<string | null>(null);

  const refresh = useCallback(() => {
    setLoading(true);
    return api
      .getAgentRuntime()
      .then((rt) => {
        setDraft({
          ...emptyAgentRuntime(),
          ...rt,
          standing_prompt: rt.standing_prompt ?? "",
        });
        setError(null);
      })
      .catch((e) => setError(String(e)))
      .finally(() => setLoading(false));
  }, []);

  useEffect(() => {
    refresh();
  }, [refresh]);

  if (loading && !error) {
    return (
      <section aria-labelledby="agent-runtime-title" data-testid="agent-runtime-panel">
        <h2 id="agent-runtime-title">Agent runtime</h2>
        <p className="dim">loading…</p>
      </section>
    );
  }

  return (
    <AgentRuntimePanelView
      draft={draft}
      busy={busy}
      error={error}
      savedHint={savedHint}
      onDraftChange={(next) => {
        setSavedHint(null);
        setDraft(next);
      }}
      onSave={() => {
        setBusy(true);
        setError(null);
        setSavedHint(null);
        api
          .putAgentRuntime(draft)
          .then((saved) => {
            setDraft({
              ...emptyAgentRuntime(),
              ...saved,
              standing_prompt: saved.standing_prompt ?? "",
            });
            setSavedHint("Saved. Next runs use this engine, standing prompt, and timeouts.");
          })
          .catch((e) => setError(String(e)))
          .finally(() => setBusy(false));
      }}
    />
  );
}
