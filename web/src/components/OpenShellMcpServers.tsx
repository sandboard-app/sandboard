import { useCallback, useEffect, useState } from "react";
import { api } from "../api.js";
import type {
  McpAudience,
  McpServerDesired,
  McpTransport,
  OpenShellProviderView,
} from "../types.js";
import { YamlEditor } from "./YamlEditor.js";

type Kind = "http" | "stdio";

type ServerDraft = {
  id: string;
  name: string;
  kind: Kind;
  url: string;
  auth: "none" | "cockpit_bearer" | "oauth";
  oauthEnv: string;
  oauthProvider: string;
  command: string;
  argsText: string;
  cwd: string;
  policy_fragment_yaml: string;
  provider_names: string[];
  env_text: string;
  audience: McpAudience;
  shipped: boolean;
};

const emptyDraft = (): ServerDraft => ({
  id: "",
  name: "",
  kind: "stdio",
  url: "",
  auth: "none",
  oauthEnv: "",
  oauthProvider: "",
  command: "",
  argsText: "",
  cwd: "",
  policy_fragment_yaml: "",
  provider_names: [],
  env_text: "",
  audience: "both",
  shipped: false,
});

function draftFrom(s: McpServerDesired): ServerDraft {
  const t = s.transport;
  if (t.kind === "http") {
    const authKind = t.auth?.kind ?? "none";
    return {
      id: s.id,
      name: s.name,
      kind: "http",
      url: t.url ?? "",
      auth:
        authKind === "cockpit_bearer"
          ? "cockpit_bearer"
          : authKind === "oauth"
            ? "oauth"
            : "none",
      oauthEnv: t.auth?.kind === "oauth" ? t.auth.env : "",
      oauthProvider: t.auth?.kind === "oauth" ? t.auth.provider : "",
      command: "",
      argsText: "",
      cwd: "",
      policy_fragment_yaml: s.policy_fragment_yaml ?? "",
      provider_names: [...(s.provider_names ?? [])],
      env_text: Object.entries(s.env ?? {})
        .map(([k, v]) => `${k}=${v}`)
        .join("\n"),
      audience: s.audience ?? "cockpit",
      shipped: !!s.shipped,
    };
  }
  return {
    id: s.id,
    name: s.name,
    kind: "stdio",
    url: "",
    auth: "none",
    oauthEnv: "",
    oauthProvider: "",
    command: t.command,
    argsText: (t.args ?? []).join(" "),
    cwd: t.cwd ?? "",
    policy_fragment_yaml: s.policy_fragment_yaml ?? "",
    provider_names: [...(s.provider_names ?? [])],
    env_text: Object.entries(s.env ?? {})
      .map(([k, v]) => `${k}=${v}`)
      .join("\n"),
    audience: s.audience ?? "both",
    shipped: !!s.shipped,
  };
}

function parseEnv(text: string): Record<string, string> {
  const out: Record<string, string> = {};
  for (const line of text.split("\n")) {
    const t = line.trim();
    if (!t || t.startsWith("#")) continue;
    const eq = t.indexOf("=");
    if (eq <= 0) continue;
    out[t.slice(0, eq).trim()] = t.slice(eq + 1);
  }
  return out;
}

function transportFrom(d: ServerDraft): McpTransport {
  if (d.kind === "http") {
    const auth =
      d.auth === "cockpit_bearer"
        ? { kind: "cockpit_bearer" as const }
        : d.auth === "oauth"
          ? {
              kind: "oauth" as const,
              provider: d.oauthProvider.trim(),
              env: d.oauthEnv.trim(),
            }
          : { kind: "none" as const };
    return { kind: "http", url: d.url.trim(), auth };
  }
  const args = d.argsText
    .trim()
    .split(/\s+/)
    .filter(Boolean);
  return {
    kind: "stdio",
    command: d.command.trim(),
    args,
    cwd: d.cwd.trim() || null,
  };
}

function transportLabel(s: McpServerDesired): string {
  if (s.transport.kind === "http") {
    return `http ${s.transport.url || "(cockpit seat)"}`;
  }
  return `stdio ${s.transport.command}`;
}

export function OpenShellMcpServersPanelView({
  servers,
  availableProviders,
  busy,
  error,
  hint,
  draft,
  editingId,
  oauthSupported,
  oauthDiscovering,
  onDraftChange,
  onSave,
  onCancelEdit,
  onEdit,
  onDelete,
  onStartCreate,
  onOAuthLogin,
  onOAuthDisconnect,
}: {
  servers: McpServerDesired[];
  availableProviders: OpenShellProviderView[];
  busy?: boolean;
  error?: string | null;
  hint?: string | null;
  draft: ServerDraft | null;
  editingId: string | null;
  /** null = not probed / no URL yet */
  oauthSupported: boolean | null;
  oauthDiscovering?: boolean;
  onDraftChange: (next: ServerDraft | null) => void;
  onSave: () => void;
  onCancelEdit: () => void;
  onEdit: (s: McpServerDesired) => void;
  onDelete: (id: string) => void;
  onStartCreate: () => void;
  onOAuthLogin: () => void;
  onOAuthDisconnect: () => void;
}) {
  const isCreate = editingId === "";
  const isEditing = editingId !== null && draft != null;

  const toggleProvider = (name: string) => {
    if (!draft) return;
    const set = new Set(draft.provider_names);
    if (set.has(name)) set.delete(name);
    else set.add(name);
    onDraftChange({ ...draft, provider_names: [...set] });
  };

  return (
    <section
      className="openshell-band openshell-mcp-servers"
      data-testid="mcp-servers-panel"
      aria-labelledby="mcp-servers-title"
    >
      <div className="openshell-band-head">
        <h2 id="mcp-servers-title">MCP servers</h2>
        <p className="dim">
          HTTP or stdio servers injected into sandboxes. Attach them on an
          OpenShell <strong>Sandbox spec</strong>. Policy fragments and
          providers merge at create; engines get Cursor/Claude/agy/OpenCode
          config without pasting JSON.
        </p>
      </div>

      {error && <div className="err">{error}</div>}
      {hint && (
        <p className="dim" data-testid="openshell-mcp-servers-hint">
          {hint}
        </p>
      )}

      {!isEditing && (
        <div className="btns" style={{ marginBottom: 12 }}>
          <button
            type="button"
            className="primary"
            disabled={busy}
            onClick={onStartCreate}
            data-testid="openshell-mcp-servers-add"
          >
            Add MCP server
          </button>
        </div>
      )}

      {servers.length === 0 && !isEditing ? (
        <p className="dim" data-testid="openshell-mcp-servers-empty">
          No MCP servers yet.
        </p>
      ) : (
        <ul
          className="openshell-provider-list"
          data-testid="openshell-mcp-server-list"
        >
          {servers.map((s) => (
            <li
              key={s.id}
              className="openshell-provider-row"
              data-testid={`openshell-mcp-server-${s.id}`}
            >
              <div className="openshell-provider-main">
                <strong>{s.name}</strong>
                <span className="dim">
                  {s.id}
                  {s.shipped ? " · built-in" : ""}
                </span>
              </div>
              <div className="openshell-provider-meta dim">
                {transportLabel(s)} · {s.audience ?? "cockpit"}
              </div>
              <div className="btns">
                <button
                  type="button"
                  disabled={busy || isEditing}
                  onClick={() => onEdit(s)}
                  data-testid={`openshell-mcp-server-edit-${s.id}`}
                >
                  Edit
                </button>
                <button
                  type="button"
                  disabled={busy || isEditing || !!s.shipped}
                  onClick={() => onDelete(s.id)}
                  data-testid={`openshell-mcp-server-delete-${s.id}`}
                >
                  Delete
                </button>
              </div>
            </li>
          ))}
        </ul>
      )}

      {isEditing && draft && (
        <form
          className="sandbox-profile-form workspace-form openshell-provider-form"
          data-testid="openshell-mcp-server-form"
          onSubmit={(e) => {
            e.preventDefault();
            onSave();
          }}
        >
          <h3>{isCreate ? "Create MCP server" : `Edit ${editingId}`}</h3>
          {!isCreate && (
            <label>
              Id
              <input
                className="search-input"
                value={draft.id}
                disabled
                readOnly
                data-testid="openshell-mcp-field-id"
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
              data-testid="openshell-mcp-field-name"
            />
          </label>
          <label>
            Audience
            <select
              className="search-input"
              value={draft.audience}
              disabled={busy || draft.shipped}
              onChange={(e) =>
                onDraftChange({
                  ...draft,
                  audience: e.target.value as McpAudience,
                })
              }
              data-testid="openshell-mcp-field-audience"
            >
              <option value="cockpit">Cockpit only</option>
              <option value="worker">Workers only</option>
              <option value="both">Cockpit + workers</option>
            </select>
          </label>
          <label>
            Transport
            <select
              className="search-input"
              value={draft.kind}
              disabled={busy || draft.shipped}
              onChange={(e) =>
                onDraftChange({
                  ...draft,
                  kind: e.target.value as Kind,
                })
              }
              data-testid="openshell-mcp-field-kind"
            >
              <option value="stdio">Stdio (command + args)</option>
              <option value="http">HTTP (Streamable HTTP URL)</option>
            </select>
          </label>

          {draft.kind === "http" ? (
            draft.auth === "cockpit_bearer" ? (
              <p className="dim" data-testid="openshell-mcp-seat-auth">
                Cockpit seat auth is automatic (host-minted Bearer for{" "}
                <code>/mcp</code>).
              </p>
            ) : (
              <>
                <label>
                  URL
                  <input
                    className="search-input"
                    value={draft.url}
                    disabled={busy}
                    onChange={(e) =>
                      onDraftChange({ ...draft, url: e.target.value })
                    }
                    placeholder="https://…"
                    data-testid="openshell-mcp-field-url"
                  />
                </label>
                {draft.auth === "oauth" && (
                  <p className="dim" data-testid="openshell-mcp-oauth-status">
                    Connected via provider{" "}
                    <code>{draft.oauthProvider || "(pending)"}</code>
                    {draft.oauthEnv ? (
                      <>
                        {" "}
                        (<code>{draft.oauthEnv}</code>)
                      </>
                    ) : null}
                  </p>
                )}
                {(draft.auth === "oauth" ||
                  oauthSupported === true ||
                  oauthDiscovering) && (
                  <div className="sandbox-profile-actions">
                    {(oauthSupported === true || draft.auth === "oauth") && (
                      <button
                        type="button"
                        className="btn"
                        disabled={busy || !draft.url.trim()}
                        onClick={onOAuthLogin}
                        data-testid="openshell-mcp-oauth-login"
                      >
                        {draft.auth === "oauth"
                          ? "Re-login with OAuth"
                          : "Log in with OAuth"}
                      </button>
                    )}
                    {oauthDiscovering && oauthSupported !== true && (
                      <span className="dim" data-testid="openshell-mcp-oauth-probing">
                        Checking OAuth support…
                      </span>
                    )}
                    {draft.auth === "oauth" && draft.id && (
                      <button
                        type="button"
                        className="btn btn-danger"
                        disabled={busy}
                        onClick={onOAuthDisconnect}
                        data-testid="openshell-mcp-oauth-disconnect"
                      >
                        Disconnect OAuth
                      </button>
                    )}
                  </div>
                )}
              </>
            )
          ) : (
            <>
              <label>
                Command
                <input
                  className="search-input"
                  value={draft.command}
                  disabled={busy}
                  onChange={(e) =>
                    onDraftChange({ ...draft, command: e.target.value })
                  }
                  required
                  data-testid="openshell-mcp-field-command"
                />
              </label>
              <label>
                Args (whitespace-separated)
                <input
                  className="search-input"
                  value={draft.argsText}
                  disabled={busy}
                  onChange={(e) =>
                    onDraftChange({ ...draft, argsText: e.target.value })
                  }
                  data-testid="openshell-mcp-field-args"
                />
              </label>
              <label>
                cwd (optional)
                <input
                  className="search-input"
                  value={draft.cwd}
                  disabled={busy}
                  onChange={(e) =>
                    onDraftChange({ ...draft, cwd: e.target.value })
                  }
                  data-testid="openshell-mcp-field-cwd"
                />
              </label>
            </>
          )}

          <div
            className="openshell-profile-providers"
            data-testid="openshell-mcp-field-providers"
          >
            <div className="openshell-profile-providers-head">
              <span className="openshell-profile-providers-title">
                Required providers
              </span>
              <span className="dim sandbox-field-hint">
                Attached when a sandbox using this MCP server is created.
                Credentials live under OpenShell → Providers.
              </span>
            </div>
            {availableProviders.length === 0 ? (
              <p className="dim" data-testid="openshell-mcp-providers-empty">
                No providers yet — add them under OpenShell → Providers.
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
                          data-testid={`openshell-mcp-provider-${p.name}`}
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
          </div>
          <label>
            Env (KEY=value per line)
            <textarea
              className="search-input"
              rows={4}
              value={draft.env_text}
              disabled={busy}
              onChange={(e) =>
                onDraftChange({ ...draft, env_text: e.target.value })
              }
              data-testid="openshell-mcp-field-env"
            />
          </label>
          <label>
            Policy fragment YAML (optional)
            <YamlEditor
              className="sandbox-policy-textarea"
              value={draft.policy_fragment_yaml}
              disabled={busy}
              onChange={(policy_fragment_yaml) =>
                onDraftChange({ ...draft, policy_fragment_yaml })
              }
              rows={10}
              placeholder={
                "network_policies:\n  pypi:\n    name: pypi\n    endpoints:\n      - { host: pypi.org, port: 443, access: full, tls: skip }\n"
              }
              data-testid="openshell-mcp-field-fragment"
            />
          </label>
          <div className="btns">
            <button
              type="submit"
              className="primary"
              disabled={busy}
              data-testid="openshell-mcp-server-save"
            >
              {isCreate ? "Create" : "Save"}
            </button>
            <button type="button" disabled={busy} onClick={onCancelEdit}>
              Cancel
            </button>
          </div>
        </form>
      )}
    </section>
  );
}

export function OpenShellMcpServersPanel() {
  const [servers, setServers] = useState<McpServerDesired[]>([]);
  const [providers, setProviders] = useState<OpenShellProviderView[]>([]);
  const [draft, setDraft] = useState<ServerDraft | null>(null);
  const [editingId, setEditingId] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [hint, setHint] = useState<string | null>(null);
  const [oauthSupported, setOauthSupported] = useState<boolean | null>(null);
  const [oauthDiscovering, setOauthDiscovering] = useState(false);

  const refresh = useCallback(() => {
    return Promise.all([
      api.listMcpServers(),
      api.listOpenShellProviders().catch(() => ({
        providers: [] as OpenShellProviderView[],
        gateway_reachable: false,
      })),
    ])
      .then(([mcp, prov]) => {
        setServers(mcp.servers);
        setProviders(prov.providers);
        setError(null);
      })
      .catch((e) => setError(String(e)));
  }, []);

  useEffect(() => {
    refresh();
  }, [refresh]);

  // Probe RFC 9728 / AS metadata when the HTTP URL settles.
  useEffect(() => {
    if (!draft || draft.kind !== "http" || draft.auth === "cockpit_bearer") {
      setOauthSupported(null);
      setOauthDiscovering(false);
      return;
    }
    const url = draft.url.trim();
    if (!url || !/^https?:\/\//i.test(url)) {
      setOauthSupported(null);
      setOauthDiscovering(false);
      return;
    }
    let cancelled = false;
    setOauthDiscovering(true);
    const t = window.setTimeout(() => {
      api
        .discoverMcpOAuth({ url })
        .then((out) => {
          if (!cancelled) setOauthSupported(!!out.supported);
        })
        .catch(() => {
          if (!cancelled) setOauthSupported(false);
        })
        .finally(() => {
          if (!cancelled) setOauthDiscovering(false);
        });
    }, 400);
    return () => {
      cancelled = true;
      window.clearTimeout(t);
    };
  }, [draft?.kind, draft?.url, draft?.auth]);

  // Return from /oauth/mcp-client/callback → open the connected server.
  useEffect(() => {
    const params = new URLSearchParams(window.location.search);
    const status = params.get("mcp_oauth");
    if (!status) return;
    const id = params.get("id");
    const message = params.get("message");
    const clean = () => {
      const url = new URL(window.location.href);
      url.searchParams.delete("mcp_oauth");
      url.searchParams.delete("id");
      url.searchParams.delete("message");
      window.history.replaceState({}, "", url.pathname + url.search + url.hash);
    };
    if (status === "ok" && id) {
      setHint(`OAuth connected for ${id}.`);
      refresh().then(() =>
        api.getMcpServer(id).then((s) => {
          setEditingId(s.id);
          setDraft(draftFrom(s));
        }),
      );
    } else if (status === "error") {
      setError(message ? decodeURIComponent(message) : "OAuth failed");
    }
    clean();
  }, [refresh]);

  return (
    <OpenShellMcpServersPanelView
      servers={servers}
      availableProviders={providers}
      busy={busy}
      error={error}
      hint={hint}
      draft={draft}
      editingId={editingId}
      oauthSupported={oauthSupported}
      oauthDiscovering={oauthDiscovering}
      onDraftChange={setDraft}
      onCancelEdit={() => {
        setDraft(null);
        setEditingId(null);
      }}
      onStartCreate={() => {
        setEditingId("");
        setDraft({ ...emptyDraft(), kind: "http" });
        setHint(null);
        setError(null);
      }}
      onEdit={(s) => {
        setEditingId(s.id);
        setDraft(draftFrom(s));
        setHint(null);
        setError(null);
      }}
      onSave={() => {
        if (!draft) return;
        const name = draft.name.trim();
        if (!name) {
          setError("name is required");
          return;
        }
        if (draft.auth === "oauth" && (!draft.oauthProvider || !draft.oauthEnv)) {
          setError("complete OAuth login before saving");
          return;
        }
        setBusy(true);
        setError(null);
        setHint(null);
        const body = {
          ...(draft.id.trim() ? { id: draft.id.trim() } : {}),
          name,
          transport: transportFrom(draft),
          policy_fragment_yaml: draft.policy_fragment_yaml.trim() || null,
          provider_names: draft.provider_names,
          env: parseEnv(draft.env_text),
          audience: draft.audience,
        };
        api
          .upsertMcpServer(body)
          .then(() => {
            setDraft(null);
            setEditingId(null);
            setHint("MCP server saved.");
            return refresh();
          })
          .catch((e) => setError(String(e)))
          .finally(() => setBusy(false));
      }}
      onDelete={(id) => {
        if (!window.confirm(`Delete MCP server ${id}?`)) return;
        setBusy(true);
        setError(null);
        setHint(null);
        api
          .deleteMcpServer(id)
          .then(() => {
            if (editingId === id) {
              setDraft(null);
              setEditingId(null);
            }
            setHint(`Deleted ${id}.`);
            return refresh();
          })
          .catch((e) => setError(String(e)))
          .finally(() => setBusy(false));
      }}
      onOAuthLogin={() => {
        if (!draft) return;
        const url = draft.url.trim();
        if (!url) {
          setError("URL is required for OAuth");
          return;
        }
        setBusy(true);
        setError(null);
        setHint(null);
        api
          .startMcpOAuth({
            url,
            server_id: draft.id.trim() || undefined,
            name: draft.name.trim() || undefined,
            return_path: "/settings/openshell/mcp-servers",
          })
          .then((out) => {
            setDraft({ ...draft, id: out.server_id });
            if (editingId === "") setEditingId(out.server_id);
            window.location.href = out.authorize_url;
          })
          .catch((e) => {
            setError(String(e));
            setBusy(false);
          });
      }}
      onOAuthDisconnect={() => {
        if (!draft?.id) return;
        if (!window.confirm(`Disconnect OAuth for ${draft.id}?`)) return;
        setBusy(true);
        setError(null);
        setHint(null);
        api
          .disconnectMcpOAuth(draft.id)
          .then(() => refresh())
          .then(() => api.getMcpServer(draft.id))
          .then((s) => {
            setDraft(draftFrom(s));
            setHint("OAuth disconnected.");
          })
          .catch((e) => setError(String(e)))
          .finally(() => setBusy(false));
      }}
    />
  );
}
