import { useCallback, useEffect, useState } from "react";
import { api } from "../api.js";
import type {
  McpServerDesired,
  OpenShellPolicy,
  OpenShellProviderView,
  SandboxProfile,
  SandboxProfileCreateDefaults,
} from "../types.js";

type ProfileDraft = {
  id: string;
  name: string;
  image: string;
  policy_id: string;
  cpu: string;
  memory: string;
  engine: string;
  model: string;
  /** Explicit attach list (always sent on save). */
  provider_names: string[];
  mcp_server_ids: string[];
  /** Non-secret env overlaid at sandbox create (profile wins on key clash). */
  env: Record<string, string>;
  /** Seat notes injected into cold/Cockpit briefing when non-empty. */
  prompt: string;
};

const emptyDraft = (defaults?: SandboxProfileCreateDefaults | null): ProfileDraft => ({
  id: "",
  name: defaults?.name ?? "Default",
  image: defaults?.image ?? "",
  policy_id: defaults?.policy_id ?? "",
  cpu: defaults?.cpu ?? "",
  memory: defaults?.memory ?? "",
  engine: defaults?.engine?.trim() || "cursor",
  model: "",
  provider_names: [],
  mcp_server_ids: [],
  env: {},
  prompt: "",
});

function draftFrom(p: SandboxProfile): ProfileDraft {
  return {
    id: p.id,
    name: p.name,
    image: p.image,
    policy_id: p.policy_id,
    cpu: p.cpu ?? "",
    memory: p.memory ?? "",
    engine: p.engine?.trim() || "cursor",
    model: p.model?.trim() ?? "",
    provider_names: [...(p.provider_names ?? [])],
    mcp_server_ids: [...(p.mcp_server_ids ?? [])],
    env: { ...(p.env ?? {}) },
    prompt: p.prompt?.trim() ?? "",
  };
}

function envEntries(env: Record<string, string>): [string, string][] {
  return Object.entries(env).sort(([a], [b]) => a.localeCompare(b));
}

function policyLabel(
  policyId: string,
  policies: OpenShellPolicy[],
): string {
  const match = policies.find((p) => p.id === policyId);
  return match ? match.name : policyId || "none";
}

/** True when the spec attaches no OpenShell providers on sandbox create. */
export function sandboxHasNoProviders(
  profile: { provider_names?: string[] | null } | null | undefined,
): boolean {
  return (profile?.provider_names ?? []).length === 0;
}

const NO_PROVIDERS_WARNING =
  "No providers attached — nothing is injected into the sandbox on create. Attach the providers this run needs (model credentials for the engine, and usually github-app for git/gh) before dispatching.";

export function SandboxesPanelView({
  profiles,
  policies,
  defaultId,
  cockpitId,
  availableProviders,
  availableMcpServers = [],
  selectedId,
  busy,
  error,
  editingId,
  draft,
  onSelect,
  onDraftChange,
  onStartCreate,
  onStartEdit,
  onCancelEdit,
  onSave,
  onDelete,
  onSetDefault,
  onSetCockpit,
}: {
  profiles: SandboxProfile[];
  policies: OpenShellPolicy[];
  defaultId: string | null;
  cockpitId: string | null;
  availableProviders: OpenShellProviderView[];
  availableMcpServers?: McpServerDesired[];
  selectedId: string | null;
  busy?: boolean;
  error?: string | null;
  editingId: string | null;
  draft: ProfileDraft;
  onSelect: (id: string) => void;
  onDraftChange: (next: ProfileDraft) => void;
  onStartCreate: () => void;
  onStartEdit: (p: SandboxProfile) => void;
  onCancelEdit: () => void;
  onSave: () => void;
  onDelete: (id: string) => void;
  onSetDefault: (id: string) => void;
  onSetCockpit: (id: string) => void;
}) {
  const isCreate = editingId === "";
  const isEditing = editingId !== null;
  const selected =
    selectedId != null ? profiles.find((p) => p.id === selectedId) : undefined;
  /** Cockpit uses an explicit override, else the global default. */
  const effectiveCockpitId = cockpitId ?? defaultId;
  const canDeleteSelected = !!selected && !isEditing;
  /** Editing the Cockpit spec: shipped `sandboard` is required (resolve/inject re-add it). */
  const editingCockpitSpec =
    isEditing &&
    !isCreate &&
    !!effectiveCockpitId &&
    (editingId === effectiveCockpitId || draft.id === effectiveCockpitId);

  const toggleProvider = (name: string) => {
    const set = new Set(draft.provider_names);
    if (set.has(name)) set.delete(name);
    else set.add(name);
    onDraftChange({ ...draft, provider_names: [...set] });
  };

  const toggleMcp = (id: string) => {
    if (editingCockpitSpec && id === "sandboard") return;
    const set = new Set(draft.mcp_server_ids);
    if (set.has(id)) set.delete(id);
    else set.add(id);
    onDraftChange({ ...draft, mcp_server_ids: [...set] });
  };

  return (
    <div
      className="openshell-band openshell-profiles"
      data-testid="openshell-profiles"
    >
      <section aria-labelledby="openshell-profiles-title" data-testid="sandboxes-panel">
        <div className="openshell-band-head">
          <h3 id="openshell-profiles-title">Sandbox specs</h3>
          <p className="dim">
            Image, resources, engine, policy, and which providers attach when a
            sandbox is created. The first spec is the global default (Cockpit
            uses it unless you pick another). Policies and Providers are
            configured in the other tabs.
          </p>
        </div>

        {error && <div className="err">{error}</div>}

        <div className="openshell-profiles-layout" data-testid="sandbox-profile-list">
          <div className="openshell-profile-rail">
            {profiles.length === 0 ? (
              <div className="settings-placeholder" data-testid="sandboxes-empty">
                <p>No sandbox specs yet.</p>
                <p className="dim">
                  Create one — the form starts from the minimal policy. It
                  becomes the default (and Cockpit) until you add another.
                </p>
              </div>
            ) : (
              <ul className="openshell-profile-rail-ul">
                {profiles.map((p) => {
                  const isDefault = defaultId === p.id;
                  const isCockpit = effectiveCockpitId === p.id;
                  const active = selectedId === p.id;
                  return (
                    <li key={p.id}>
                      <button
                        type="button"
                        className={
                          active
                            ? "openshell-profile-rail-btn active"
                            : "openshell-profile-rail-btn"
                        }
                        disabled={busy || isEditing}
                        onClick={() => onSelect(p.id)}
                        data-testid={`sandbox-profile-${p.id}`}
                      >
                        <span className="openshell-profile-rail-name">{p.name}</span>
                        <span className="dim openshell-profile-rail-id">{p.id}</span>
                        <span className="openshell-profile-rail-badges">
                          {isDefault && (
                            <span
                              className="sandbox-default-badge"
                              data-testid="sandbox-default-badge"
                            >
                              default
                            </span>
                          )}
                          {isCockpit && (
                            <span
                              className="sandbox-default-badge"
                              data-testid="sandbox-cockpit-badge"
                            >
                              Cockpit
                            </span>
                          )}
                          {sandboxHasNoProviders(p) && (
                            <span
                              className="sandbox-warn-badge"
                              data-testid="sandbox-no-providers-badge"
                              title={NO_PROVIDERS_WARNING}
                            >
                              no providers
                            </span>
                          )}
                        </span>
                      </button>
                    </li>
                  );
                })}
              </ul>
            )}
            {!isEditing && (
              <div className="btns sandbox-profile-toolbar">
                <button
                  type="button"
                  className="primary"
                  disabled={busy}
                  onClick={onStartCreate}
                  data-testid="sandbox-create"
                >
                  New profile
                </button>
              </div>
            )}
          </div>

          <div className="openshell-profile-detail">
            {isEditing ? (
              <form
                className="sandbox-profile-form"
                data-testid="sandbox-profile-form"
                onSubmit={(e) => {
                  e.preventDefault();
                  onSave();
                }}
              >
                <h3>{isCreate ? "Create profile" : `Edit ${editingId}`}</h3>
                {!isCreate && (
                  <label>
                    Id
                    <input
                      className="search-input"
                      value={draft.id}
                      disabled
                      readOnly
                      data-testid="sandbox-field-id"
                    />
                  </label>
                )}
                <label>
                  Name
                  <input
                    className="search-input"
                    value={draft.name}
                    disabled={busy}
                    onChange={(e) =>
                      onDraftChange({ ...draft, name: e.target.value })
                    }
                    required
                    data-testid="sandbox-field-name"
                  />
                </label>
                <label>
                  Image
                  <input
                    className="search-input"
                    value={draft.image}
                    disabled={busy}
                    onChange={(e) =>
                      onDraftChange({ ...draft, image: e.target.value })
                    }
                    required
                    data-testid="sandbox-field-image"
                  />
                </label>
                <label>
                  Agent engine
                  <select
                    className="search-input"
                    value={draft.engine}
                    disabled={busy}
                    onChange={(e) =>
                      onDraftChange({ ...draft, engine: e.target.value })
                    }
                    data-testid="sandbox-field-engine"
                  >
                    <option value="cursor">Cursor Agent (cursor)</option>
                    <option value="agy">Antigravity CLI (agy)</option>
                    <option value="claude">Claude Code (Anthropic)</option>
                    <option value="opencode">OpenCode (opencode)</option>
                    <option value="hermes">Hermes Agent (hermes)</option>
                  </select>
                </label>

                <label>
                  Model
                  <input
                    className="search-input"
                    value={draft.model}
                    disabled={busy}
                    placeholder="optional — card.model overrides"
                    onChange={(e) =>
                      onDraftChange({ ...draft, model: e.target.value })
                    }
                    data-testid="sandbox-field-model"
                  />
                  <span className="dim sandbox-field-hint">
                    Passed to the agent CLI when set (`agy --model`, `agent
                    --model`, `hermes --model`). Unset cards inherit this; card.model overrides on
                    claim. Ignored for `claude` / `opencode` (gateway
                    `inference.local` route).
                  </span>
                </label>

                <label>
                  Policy
                  <select
                    className="search-input"
                    value={draft.policy_id}
                    disabled={busy || policies.length === 0}
                    onChange={(e) =>
                      onDraftChange({ ...draft, policy_id: e.target.value })
                    }
                    required
                    data-testid="sandbox-field-policy"
                  >
                    {policies.length === 0 ? (
                      <option value="">No policies — add under Policies</option>
                    ) : (
                      policies.map((p) => (
                        <option key={p.id} value={p.id}>
                          {p.name} ({p.id})
                        </option>
                      ))
                    )}
                  </select>
                  <span className="dim sandbox-field-hint">
                    OpenShell allow-list from the Policies catalog. Edit YAML
                    under Policies.
                  </span>
                </label>

                <div
                  className="openshell-profile-providers"
                  data-testid="sandbox-field-providers"
                >
                  <div className="openshell-profile-providers-head">
                    <span className="openshell-profile-providers-title">
                      Attach providers
                    </span>
                    <span className="dim sandbox-field-hint">
                      Passed on sandbox create. Credentials live under Providers.
                    </span>
                  </div>
                  {availableProviders.length === 0 ? (
                    <p className="dim" data-testid="sandbox-providers-empty">
                      No providers yet — add them under Providers.
                    </p>
                  ) : (
                    <ul className="openshell-profile-provider-ul">
                      {availableProviders.map((p) => {
                        const typeDiffers =
                          p.type.trim().toLowerCase() !==
                          p.name.trim().toLowerCase();
                        return (
                          <li key={p.name}>
                            <label className="openshell-provider-check">
                              <input
                                type="checkbox"
                                checked={draft.provider_names.includes(p.name)}
                                disabled={busy}
                                onChange={() => toggleProvider(p.name)}
                                data-testid={`sandbox-provider-${p.name}`}
                              />
                              <span className="openshell-provider-check-text">
                                <span className="openshell-provider-check-name">
                                  {p.name}
                                </span>
                                {typeDiffers ? (
                                  <span className="dim openshell-provider-check-type">
                                    {p.type}
                                  </span>
                                ) : null}
                              </span>
                            </label>
                          </li>
                        );
                      })}
                    </ul>
                  )}
                  {sandboxHasNoProviders(draft) && (
                    <p
                      className="info"
                      role="status"
                      data-testid="sandbox-no-providers-warn"
                    >
                      {NO_PROVIDERS_WARNING}
                    </p>
                  )}
                </div>

                <div
                  className="openshell-profile-providers"
                  data-testid="sandbox-field-mcp-servers"
                >
                  <div className="openshell-profile-providers-head">
                    <span className="openshell-profile-providers-title">
                      Attach MCP servers
                    </span>
                    <span className="dim sandbox-field-hint">
                      Config inject + policy/provider merge at create.
                      {editingCockpitSpec
                        ? " Built-in sandboard stays on for Cockpit."
                        : " Cockpit always gets built-in sandboard."}
                    </span>
                  </div>
                  {availableMcpServers.length === 0 ? (
                    <p className="dim" data-testid="sandbox-mcp-servers-empty">
                      No MCP servers yet — add them under Settings → MCP
                      servers.
                    </p>
                  ) : (
                    <ul className="openshell-profile-provider-ul">
                      {availableMcpServers.map((s) => {
                        const lockedSandboard =
                          editingCockpitSpec && s.id === "sandboard";
                        const checked =
                          lockedSandboard || draft.mcp_server_ids.includes(s.id);
                        return (
                          <li key={s.id}>
                            <label className="openshell-provider-check">
                              <input
                                type="checkbox"
                                checked={checked}
                                disabled={busy || lockedSandboard}
                                onChange={() => toggleMcp(s.id)}
                                data-testid={`sandbox-mcp-${s.id}`}
                                title={
                                  lockedSandboard
                                    ? "Required for Cockpit — cannot detach"
                                    : undefined
                                }
                              />
                              <span className="openshell-provider-check-text">
                                <span className="openshell-provider-check-name">
                                  {s.name}
                                  {lockedSandboard ? " (required)" : ""}
                                </span>
                                <span className="dim openshell-provider-check-type">
                                  {s.transport.kind} · {s.audience ?? "cockpit"}
                                </span>
                              </span>
                            </label>
                          </li>
                        );
                      })}
                    </ul>
                  )}
                </div>

                <div
                  className="openshell-profile-providers"
                  data-testid="sandbox-field-env"
                >
                  <div className="openshell-profile-providers-head">
                    <span className="openshell-profile-providers-title">
                      Environment variables
                    </span>
                    <span
                      className="dim sandbox-field-hint"
                      data-testid="sandbox-env-non-secret-hint"
                    >
                      Non-secret values overlaid at sandbox create (profile wins
                      on key clash). Put secrets on Providers — not here.
                    </span>
                  </div>
                  {envEntries(draft.env).length === 0 ? (
                    <p className="dim" data-testid="sandbox-env-empty">
                      No env vars — add keys like API URLs or tool paths.
                    </p>
                  ) : (
                    <ul className="openshell-profile-provider-ul sandbox-env-ul">
                      {envEntries(draft.env).map(([key, value]) => (
                        <li key={key} className="sandbox-env-row">
                          <label className="sandbox-env-key">
                            <span className="dim">Key</span>
                            <input
                              className="search-input"
                              value={key}
                              disabled
                              readOnly
                              data-testid={`sandbox-env-key-${key}`}
                            />
                          </label>
                          <label className="sandbox-env-value">
                            <span className="dim">Value</span>
                            <input
                              className="search-input"
                              value={value}
                              disabled={busy}
                              onChange={(e) =>
                                onDraftChange({
                                  ...draft,
                                  env: { ...draft.env, [key]: e.target.value },
                                })
                              }
                              data-testid={`sandbox-env-value-${key}`}
                            />
                          </label>
                          <button
                            type="button"
                            className="sandbox-env-remove"
                            disabled={busy}
                            onClick={() => {
                              const next = { ...draft.env };
                              delete next[key];
                              onDraftChange({ ...draft, env: next });
                            }}
                            data-testid={`sandbox-env-remove-${key}`}
                          >
                            Remove
                          </button>
                        </li>
                      ))}
                    </ul>
                  )}
                  <div className="btns" style={{ marginTop: 0 }}>
                    <button
                      type="button"
                      disabled={busy}
                      data-testid="sandbox-env-add"
                      onClick={() => {
                        const key = window.prompt("Environment variable name");
                        if (!key?.trim()) return;
                        const trimmed = key.trim();
                        if (trimmed in draft.env) return;
                        onDraftChange({
                          ...draft,
                          env: { ...draft.env, [trimmed]: "" },
                        });
                      }}
                    >
                      Add variable
                    </button>
                  </div>
                </div>

                <label data-testid="sandbox-field-prompt">
                  Sandbox prompt (seat notes)
                  <textarea
                    className="search-input"
                    rows={5}
                    value={draft.prompt}
                    disabled={busy}
                    placeholder="optional — injected into cold and Cockpit briefing"
                    onChange={(e) =>
                      onDraftChange({ ...draft, prompt: e.target.value })
                    }
                    data-testid="sandbox-field-prompt-input"
                  />
                  <span className="dim sandbox-field-hint">
                    Operator notes for agents using this spec. Shown once at
                    briefing start — not re-dumped on resume.
                  </span>
                </label>

                <div className="sandbox-profile-form-row">
                  <label>
                    CPU
                    <input
                      className="search-input"
                      value={draft.cpu}
                      disabled={busy}
                      placeholder="optional"
                      onChange={(e) =>
                        onDraftChange({ ...draft, cpu: e.target.value })
                      }
                      data-testid="sandbox-field-cpu"
                    />
                  </label>
                  <label>
                    Memory
                    <input
                      className="search-input"
                      value={draft.memory}
                      disabled={busy}
                      placeholder="optional"
                      onChange={(e) =>
                        onDraftChange({ ...draft, memory: e.target.value })
                      }
                      data-testid="sandbox-field-memory"
                    />
                  </label>
                </div>
                <div className="btns">
                  <button
                    type="submit"
                    className="primary"
                    disabled={busy}
                    data-testid="sandbox-save"
                  >
                    {isCreate ? "Create" : "Save"}
                  </button>
                  <button type="button" disabled={busy} onClick={onCancelEdit}>
                    Cancel
                  </button>
                </div>
              </form>
            ) : selected ? (
              <div
                className="openshell-profile-readonly"
                data-testid={`sandbox-profile-detail-${selected.id}`}
              >
                <div className="sandbox-profile-title">
                  <strong>{selected.name}</strong>
                  <span className="dim sandbox-profile-id">{selected.id}</span>
                </div>
                <div className="dim sandbox-profile-meta">
                  {selected.engine?.trim() || "engine: default"}
                  {selected.model?.trim() && (
                    <>
                      <span className="sep">·</span>
                      {selected.model.trim()}
                    </>
                  )}
                  <span className="sep">·</span>
                  {selected.image}
                  {(selected.cpu || selected.memory) && (
                    <>
                      <span className="sep">·</span>
                      {[selected.cpu, selected.memory].filter(Boolean).join(" / ")}
                    </>
                  )}
                </div>
                <div
                  className="openshell-profile-attach-summary"
                  data-testid="sandbox-policy-summary"
                >
                  <span className="dim">Policy</span>
                  <strong data-testid="sandbox-policy-name">
                    {policyLabel(selected.policy_id, policies)}
                  </strong>
                  <span className="dim sandbox-profile-id">{selected.policy_id}</span>
                </div>
                <div
                  className="openshell-profile-attach-summary"
                  data-testid="sandbox-attach-summary"
                >
                  <span className="dim">Attach on create</span>
                  <strong>
                    {(selected.provider_names ?? []).length === 0
                      ? "none"
                      : (selected.provider_names ?? []).join(", ")}
                  </strong>
                </div>
                {envEntries(selected.env ?? {}).length > 0 && (
                  <div
                    className="openshell-profile-attach-summary"
                    data-testid="sandbox-env-summary"
                  >
                    <span className="dim">Env</span>
                    <strong>
                      {envEntries(selected.env ?? {})
                        .map(([k, v]) => `${k}=${v}`)
                        .join(", ")}
                    </strong>
                  </div>
                )}
                {selected.prompt?.trim() && (
                  <div
                    className="openshell-profile-attach-summary sandbox-prompt-summary"
                    data-testid="sandbox-prompt-summary"
                  >
                    <span className="dim">Sandbox prompt</span>
                    <span>{selected.prompt.trim()}</span>
                  </div>
                )}
                {sandboxHasNoProviders(selected) && (
                  <p
                    className="info"
                    role="status"
                    data-testid="sandbox-no-providers-warn"
                  >
                    {NO_PROVIDERS_WARNING}
                  </p>
                )}
                <div className="btns sandbox-profile-actions">
                  <button
                    type="button"
                    disabled={busy}
                    onClick={() => onStartEdit(selected)}
                    data-testid={`sandbox-edit-${selected.id}`}
                  >
                    Edit
                  </button>
                  {selected.id !== defaultId && (
                    <button
                      type="button"
                      className="primary"
                      disabled={busy}
                      onClick={() => onSetDefault(selected.id)}
                      data-testid={`sandbox-set-default-${selected.id}`}
                    >
                      Set default
                    </button>
                  )}
                  {selected.id !== effectiveCockpitId && (
                    <button
                      type="button"
                      disabled={busy}
                      onClick={() => onSetCockpit(selected.id)}
                      data-testid={`sandbox-set-cockpit-${selected.id}`}
                    >
                      Use for Cockpit
                    </button>
                  )}
                  {canDeleteSelected && (
                    <button
                      type="button"
                      disabled={busy}
                      onClick={() => onDelete(selected.id)}
                      data-testid={`sandbox-delete-${selected.id}`}
                    >
                      Delete
                    </button>
                  )}
                </div>
              </div>
            ) : (
              <div className="settings-placeholder" data-testid="sandboxes-select-hint">
                <p>Select a profile, or create one.</p>
              </div>
            )}
          </div>
        </div>
      </section>
    </div>
  );
}

export function SandboxesPanel() {
  const [profiles, setProfiles] = useState<SandboxProfile[]>([]);
  const [policies, setPolicies] = useState<OpenShellPolicy[]>([]);
  const [providers, setProviders] = useState<OpenShellProviderView[]>([]);
  const [mcpServers, setMcpServers] = useState<McpServerDesired[]>([]);
  const [defaultId, setDefaultId] = useState<string | null>(null);
  const [cockpitId, setCockpitId] = useState<string | null>(null);
  const [createDefaults, setCreateDefaults] =
    useState<SandboxProfileCreateDefaults | null>(null);
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [editingId, setEditingId] = useState<string | null>(null);
  const [draft, setDraft] = useState<ProfileDraft>(() => emptyDraft());

  const refresh = useCallback(() => {
    setLoading(true);
    return Promise.all([
      api.listSandboxProfiles(),
      api.listOpenShellProviders(),
      api.listOpenShellPolicies(),
      api.listMcpServers(),
    ])
      .then(([out, prov, pol, mcp]) => {
        setProfiles(out.profiles);
        setDefaultId(out.default_sandbox_profile_id);
        setCockpitId(out.cockpit_sandbox_profile_id);
        setCreateDefaults(out.create_defaults);
        setProviders(prov.providers);
        setPolicies(pol.policies);
        setMcpServers(mcp.servers);
        setSelectedId((cur) => {
          if (cur && out.profiles.some((p) => p.id === cur)) return cur;
          return (
            out.default_sandbox_profile_id ??
            out.profiles[0]?.id ??
            null
          );
        });
        setError(null);
      })
      .catch((e) => setError(String(e)))
      .finally(() => setLoading(false));
  }, []);

  useEffect(() => {
    refresh();
  }, [refresh]);

  const run = (p: Promise<unknown>) => {
    setBusy(true);
    setError(null);
    return p
      .then(() => refresh())
      .catch((e) => setError(String(e)))
      .finally(() => setBusy(false));
  };

  if (loading && profiles.length === 0 && !error) {
    return (
      <div
        className="openshell-band openshell-profiles"
        data-testid="openshell-profiles"
      >
        <section aria-labelledby="openshell-profiles-title" data-testid="sandboxes-panel">
          <div className="openshell-band-head">
            <h3 id="openshell-profiles-title">Sandbox specs</h3>
            <p className="dim">loading…</p>
          </div>
        </section>
      </div>
    );
  }

  return (
    <SandboxesPanelView
      profiles={profiles}
      policies={policies}
      defaultId={defaultId}
      cockpitId={cockpitId}
      availableProviders={providers}
      availableMcpServers={mcpServers}
      selectedId={selectedId}
      busy={busy}
      error={error}
      editingId={editingId}
      draft={draft}
      onSelect={setSelectedId}
      onDraftChange={setDraft}
      onStartCreate={() => {
        setEditingId("");
        setDraft(emptyDraft(createDefaults));
      }}
      onStartEdit={(p) => {
        setEditingId(p.id);
        setDraft(draftFrom(p));
      }}
      onCancelEdit={() => {
        setEditingId(null);
        setDraft(emptyDraft(createDefaults));
      }}
      onSave={() => {
        const policy_id = draft.policy_id.trim();
        if (!policy_id) {
          setError("policy is required — add one under Policies");
          return;
        }
        const effectiveCockpit = cockpitId ?? defaultId;
        const savingCockpit =
          !!effectiveCockpit &&
          (draft.id.trim() === effectiveCockpit ||
            editingId === effectiveCockpit);
        let mcp_server_ids = [...draft.mcp_server_ids];
        if (savingCockpit && !mcp_server_ids.includes("sandboard")) {
          mcp_server_ids = ["sandboard", ...mcp_server_ids];
        }
        const body = {
          ...(editingId ? { id: draft.id.trim() } : {}),
          name: draft.name.trim(),
          image: draft.image.trim(),
          policy_id,
          cpu: draft.cpu.trim() || null,
          memory: draft.memory.trim() || null,
          engine: draft.engine.trim() || null,
          model: draft.model.trim() || null,
          provider_names: draft.provider_names,
          mcp_server_ids,
          env: draft.env,
          prompt: draft.prompt.trim() || null,
        };
        run(api.upsertSandboxProfile(body)).then(() => {
          setEditingId(null);
          setDraft(emptyDraft(createDefaults));
        });
      }}
      onDelete={(id) => {
        if (
          !window.confirm(
            `Delete profile “${id}”? Projects using it must be reassigned first.`,
          )
        ) {
          return;
        }
        run(api.deleteSandboxProfile(id)).then(() => {
          setSelectedId((cur) => (cur === id ? null : cur));
        });
      }}
      onSetDefault={(id) => run(api.setDefaultSandboxProfile(id))}
      onSetCockpit={(id) => run(api.setCockpitSandboxProfile(id))}
    />
  );
}

/** Project-level sandbox override picker (unset = global default). */
export function ProjectSandboxPicker({
  projectId,
  value,
  profiles,
  defaultId,
  busy,
  error,
  onChange,
}: {
  projectId: number;
  value: string | null | undefined;
  profiles: SandboxProfile[];
  defaultId: string | null;
  busy?: boolean;
  error?: string | null;
  onChange: (next: string | null) => void;
}) {
  const defaultLabel =
    defaultId != null
      ? profiles.find((p) => p.id === defaultId)?.name ?? defaultId
      : "none configured";

  const effectiveId = value ?? defaultId;
  const effectiveProfile =
    effectiveId != null
      ? profiles.find((p) => p.id === effectiveId)
      : undefined;

  return (
    <div className="project-sandbox-picker" data-testid="project-sandbox-picker">
      <label className="section-title" style={{ display: "block", marginBottom: 2 }}>
        Sandbox spec
      </label>
      <p className="dim" style={{ marginBottom: 4, fontSize: 12 }}>
        Override for this Project. Unset uses the global default ({defaultLabel}
        ).
      </p>
      {error && <div className="err">{error}</div>}
      <select
        className="search-input"
        style={{
          width: "100%",
          background: "var(--panel)",
          color: "var(--ink)",
          padding: "6px",
        }}
        value={value ?? ""}
        disabled={busy}
        data-testid={`project-sandbox-select-${projectId}`}
        onChange={(e) => {
          const v = e.target.value;
          onChange(v === "" ? null : v);
        }}
      >
        <option value="">Use global default</option>
        {profiles.map((p) => (
          <option key={p.id} value={p.id}>
            {p.id === defaultId ? `${p.name} · global default` : p.name}
            {sandboxHasNoProviders(p) ? " · no providers" : ""}
          </option>
        ))}
      </select>
      {sandboxHasNoProviders(effectiveProfile) && (
        <p
          className="info"
          role="status"
          data-testid="project-sandbox-no-providers-warn"
        >
          {NO_PROVIDERS_WARNING}
        </p>
      )}
    </div>
  );
}
