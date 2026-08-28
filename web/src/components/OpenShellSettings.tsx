import { useCallback, useEffect, useState, type ReactNode } from "react";
import { api } from "../api.js";
import type {
  OpenShellAuthMode,
  OpenShellOidcConfig,
  OpenShellSettings,
  OpenShellStatus,
} from "../types.js";
import { OpenShellPoliciesPanel } from "./OpenShellPolicies.js";
import { OpenShellProvidersPanel } from "./OpenShellProviders.js";
import { OpenShellProviderTypesPanel } from "./OpenShellProviderTypes.js";
import { SandboxesPanel } from "./OpenShellProfiles.js";

export type OpenShellTab =
  | "connectivity"
  | "providers"
  | "provider-types"
  | "policies"
  | "profiles";

const TABS: { id: OpenShellTab; label: string }[] = [
  { id: "connectivity", label: "Connectivity" },
  { id: "providers", label: "Providers" },
  { id: "provider-types", label: "Provider types" },
  { id: "policies", label: "Policies" },
  { id: "profiles", label: "Sandbox specs" },
];

const EMPTY_OIDC: OpenShellOidcConfig = {
  issuer: "",
  client_id: "",
  audience: "",
};

export function OpenShellPanelView({
  status,
  gatewayEndpoint,
  authMode,
  oidc,
  oidcStatus,
  caPem,
  clientCertPem,
  clientKeyPem,
  mtls,
  busy,
  error,
  savedHint,
  activeTab: activeTabProp,
  onTabChange,
  onGatewayEndpointChange,
  onAuthModeChange,
  onOidcChange,
  onCaPemChange,
  onClientCertPemChange,
  onClientKeyPemChange,
  onRefresh,
  onSave,
  onClearMtls,
  onOidcLogin,
  onOidcLogout,
  oidcAwaitingPaste = false,
  oidcPaste = "",
  oidcAuthorizeUrl = "",
  oidcRedirectUri = "",
  onOidcPasteChange,
  onOidcCompletePaste,
  providers,
  providerTypes,
  policies,
  profiles,
}: {
  status: OpenShellStatus | null;
  gatewayEndpoint: string;
  authMode: OpenShellAuthMode | "";
  oidc: OpenShellOidcConfig;
  oidcStatus?: OpenShellSettings["oidc_status"];
  caPem: string;
  clientCertPem: string;
  clientKeyPem: string;
  mtls?: OpenShellSettings["mtls"];
  busy?: boolean;
  error?: string | null;
  savedHint?: string | null;
  /** Controlled tab (tests). Uncontrolled when omitted. */
  activeTab?: OpenShellTab;
  onTabChange?: (tab: OpenShellTab) => void;
  onGatewayEndpointChange: (next: string) => void;
  onAuthModeChange: (next: OpenShellAuthMode | "") => void;
  onOidcChange: (next: OpenShellOidcConfig) => void;
  onCaPemChange: (next: string) => void;
  onClientCertPemChange: (next: string) => void;
  onClientKeyPemChange: (next: string) => void;
  onRefresh: () => void;
  onSave: () => void;
  onClearMtls: () => void;
  onOidcLogin: () => void;
  onOidcLogout: () => void;
  oidcAwaitingPaste?: boolean;
  oidcPaste?: string;
  oidcAuthorizeUrl?: string;
  oidcRedirectUri?: string;
  onOidcPasteChange?: (next: string) => void;
  onOidcCompletePaste?: () => void;
  providers?: ReactNode;
  providerTypes?: ReactNode;
  policies?: ReactNode;
  profiles?: ReactNode;
}) {
  const [internalTab, setInternalTab] = useState<OpenShellTab>("connectivity");
  const tab = activeTabProp ?? internalTab;
  const setTab = (next: OpenShellTab) => {
    onTabChange?.(next);
    if (activeTabProp === undefined) setInternalTab(next);
  };

  const healthLabel = !status
    ? "…"
    : status.healthy
      ? "Healthy"
      : "Unhealthy";
  const healthClass = !status
    ? "dim"
    : status.healthy
      ? "openshell-health-ok"
      : "openshell-health-bad";
  const authLabel =
    authMode === "mtls"
      ? mtls?.complete
        ? "mTLS configured"
        : "mTLS incomplete"
      : authMode === "oidc"
        ? oidcStatus?.logged_in
          ? "OIDC logged in"
          : "OIDC not logged in"
        : "Pick auth mode";

  return (
    <section aria-labelledby="openshell-title" data-testid="openshell-panel">
      <header className="openshell-hero">
        <h2 id="openshell-title">OpenShell</h2>
        <p className="dim openshell-hero-lead">
          Connect sandboard to your OpenShell gateway, then configure providers,
          policies, and sandbox specs. Each spec picks which providers, policy,
          and MCP servers a run gets.
        </p>
        <div
          className="openshell-status-chip"
          data-testid="openshell-health"
          data-healthy={status?.healthy ? "true" : "false"}
        >
          <span className="dim">Gateway</span>
          <strong className={healthClass} data-testid="openshell-health-label">
            {healthLabel}
          </strong>
          <span className="dim">·</span>
          <span className="dim">Auth</span>
          <strong data-testid="openshell-auth-label">{authLabel}</strong>
          <button
            type="button"
            disabled={busy}
            onClick={onRefresh}
            data-testid="openshell-refresh"
          >
            Refresh
          </button>
        </div>
      </header>

      {error && <div className="err">{error}</div>}
      {savedHint && (
        <p className="dim" data-testid="openshell-saved-hint">
          {savedHint}
        </p>
      )}

      <nav
        className="openshell-subnav"
        aria-label="OpenShell sections"
        data-testid="openshell-subnav"
      >
        {TABS.map((t) => (
          <button
            key={t.id}
            type="button"
            className={
              tab === t.id ? "openshell-subnav-btn active" : "openshell-subnav-btn"
            }
            aria-current={tab === t.id ? "page" : undefined}
            onClick={() => setTab(t.id)}
            data-testid={`openshell-tab-${t.id}`}
          >
            {t.label}
          </button>
        ))}
      </nav>

      {tab === "connectivity" && (
        <div
          className="openshell-pane openshell-connectivity"
          data-testid="openshell-connectivity"
          aria-labelledby="openshell-connectivity-title"
        >
          <div className="openshell-band-head">
            <h3 id="openshell-connectivity-title">Connectivity</h3>
            <p className="dim">
              HTTPS gateway URL and an explicit auth mode (mTLS or OIDC). The
              endpoint must match that mode — a local mTLS gateway URL will not
              work with OIDC. Secrets are stored encrypted and are not returned
              by the API.
            </p>
          </div>

          {status?.summary && (
            <pre
              className="openshell-health-summary"
              data-testid="openshell-health-summary"
            >
              {status.summary}
            </pre>
          )}

          <form
            className="sandbox-profile-form workspace-form"
            data-testid="openshell-gateway-form"
            onSubmit={(e) => {
              e.preventDefault();
              onSave();
            }}
          >
            <label>
              Gateway endpoint
              <input
                className="search-input"
                value={gatewayEndpoint}
                disabled={busy}
                placeholder="https://gateway.example.com"
                onChange={(e) => onGatewayEndpointChange(e.target.value)}
                data-testid="openshell-field-endpoint"
              />
            </label>

            <div className="openshell-auth-mode" data-testid="openshell-auth-mode">
              <span className="openshell-auth-mode-label">Auth mode</span>
              <div
                className="openshell-auth-mode-options"
                role="radiogroup"
                aria-label="Auth mode"
              >
                <button
                  type="button"
                  role="radio"
                  aria-checked={authMode === "mtls"}
                  className={
                    authMode === "mtls"
                      ? "openshell-auth-mode-btn active"
                      : "openshell-auth-mode-btn"
                  }
                  disabled={busy}
                  onClick={() => onAuthModeChange("mtls")}
                  data-testid="openshell-auth-mtls"
                >
                  mTLS
                </button>
                <button
                  type="button"
                  role="radio"
                  aria-checked={authMode === "oidc"}
                  className={
                    authMode === "oidc"
                      ? "openshell-auth-mode-btn active"
                      : "openshell-auth-mode-btn"
                  }
                  disabled={busy}
                  onClick={() => onAuthModeChange("oidc")}
                  data-testid="openshell-auth-oidc"
                >
                  OIDC
                </button>
              </div>
            </div>

            {authMode === "mtls" && (
              <>
                <label>
                  CA certificate (PEM)
                  <textarea
                    className="search-input"
                    rows={4}
                    value={caPem}
                    disabled={busy}
                    placeholder={
                      mtls?.ca
                        ? "Configured — paste to replace"
                        : "-----BEGIN CERTIFICATE-----"
                    }
                    onChange={(e) => onCaPemChange(e.target.value)}
                    data-testid="openshell-field-ca"
                  />
                </label>
                <label>
                  Client certificate (PEM)
                  <textarea
                    className="search-input"
                    rows={4}
                    value={clientCertPem}
                    disabled={busy}
                    placeholder={
                      mtls?.client_cert
                        ? "Configured — paste to replace"
                        : "-----BEGIN CERTIFICATE-----"
                    }
                    onChange={(e) => onClientCertPemChange(e.target.value)}
                    data-testid="openshell-field-client-cert"
                  />
                </label>
                <label>
                  Client private key (PEM)
                  <textarea
                    className="search-input"
                    rows={4}
                    value={clientKeyPem}
                    disabled={busy}
                    placeholder={
                      mtls?.client_key
                        ? "Configured — paste to replace"
                        : "-----BEGIN PRIVATE KEY-----"
                    }
                    onChange={(e) => onClientKeyPemChange(e.target.value)}
                    data-testid="openshell-field-client-key"
                  />
                </label>
              </>
            )}

            {authMode === "oidc" && (
              <>
                <label>
                  OIDC issuer
                  <input
                    className="search-input"
                    value={oidc.issuer}
                    disabled={busy}
                    placeholder="https://idp.example.com/realms/openshell"
                    onChange={(e) =>
                      onOidcChange({ ...oidc, issuer: e.target.value })
                    }
                    data-testid="openshell-field-oidc-issuer"
                  />
                </label>
                <label>
                  Client ID
                  <input
                    className="search-input"
                    value={oidc.client_id}
                    disabled={busy}
                    placeholder="openshell-cli"
                    onChange={(e) =>
                      onOidcChange({ ...oidc, client_id: e.target.value })
                    }
                    data-testid="openshell-field-oidc-client-id"
                  />
                </label>
                <label>
                  Audience
                  <input
                    className="search-input"
                    value={oidc.audience}
                    disabled={busy}
                    placeholder="openshell-cli"
                    onChange={(e) =>
                      onOidcChange({ ...oidc, audience: e.target.value })
                    }
                    data-testid="openshell-field-oidc-audience"
                  />
                </label>
                <p className="dim" data-testid="openshell-oidc-login-status">
                  {oidcStatus?.logged_in
                    ? "Logged in (tokens encrypted in board DB)."
                    : "Not logged in — Save settings, then Log in."}
                </p>
                {oidcAwaitingPaste ? (
                  <div data-testid="openshell-oidc-paste">
                    <p className="dim" data-testid="openshell-oidc-redirect-hint">
                      IdP login opened in a new tab. The browser will land on a
                      loopback URL that fails to load (
                      <code>
                        {oidcRedirectUri || "http://127.0.0.1:…/callback"}
                      </code>
                      ). Paste that address bar URL (or the{" "}
                      <code>?code=…&state=…</code> portion) here.
                    </p>
                    {oidcAuthorizeUrl && (
                      <p className="dim">
                        If no tab opened, open{" "}
                        <a
                          href={oidcAuthorizeUrl}
                          target="_blank"
                          rel="noopener noreferrer"
                          data-testid="openshell-oidc-authorize-link"
                        >
                          the authorize URL
                        </a>{" "}
                        yourself.
                      </p>
                    )}
                    {onOidcPasteChange && (
                      <label>
                        Redirect URL
                        <textarea
                          className="search-input"
                          rows={3}
                          value={oidcPaste}
                          disabled={busy}
                          placeholder="http://127.0.0.1:…/callback?code=…&state=…"
                          onChange={(e) => onOidcPasteChange(e.target.value)}
                          data-testid="openshell-oidc-paste-url"
                        />
                      </label>
                    )}
                    {onOidcCompletePaste && (
                      <button
                        type="button"
                        className="primary"
                        disabled={busy || !oidcPaste.trim()}
                        onClick={onOidcCompletePaste}
                        data-testid="openshell-oidc-paste-complete"
                      >
                        Complete login
                      </button>
                    )}
                  </div>
                ) : (
                  <p className="dim" data-testid="openshell-oidc-redirect-hint">
                    Log in uses a loopback callback (same as the OpenShell CLI).
                    After Keycloak, paste the{" "}
                    <code>http://127.0.0.1:…/callback</code> URL that fails to
                    load.
                  </p>
                )}
              </>
            )}

            <div className="btns">
              <button
                type="submit"
                className="primary"
                disabled={busy || !authMode}
                data-testid="openshell-save"
              >
                Save
              </button>
              {authMode === "mtls" && (
                <button
                  type="button"
                  disabled={busy || !mtls?.complete}
                  onClick={onClearMtls}
                  data-testid="openshell-clear-mtls"
                >
                  Clear mTLS
                </button>
              )}
              {authMode === "oidc" && (
                <>
                  <button
                    type="button"
                    className="primary"
                    disabled={busy || !oidc.issuer.trim() || !oidc.client_id.trim()}
                    onClick={onOidcLogin}
                    data-testid="openshell-oidc-login"
                  >
                    Log in
                  </button>
                  <button
                    type="button"
                    disabled={busy || !oidcStatus?.logged_in}
                    onClick={onOidcLogout}
                    data-testid="openshell-oidc-logout"
                  >
                    Log out
                  </button>
                </>
              )}
            </div>
          </form>
        </div>
      )}

      {tab === "providers" && (
        <div className="openshell-pane" data-testid="openshell-providers-host">
          {providers}
        </div>
      )}

      {tab === "provider-types" && (
        <div
          className="openshell-pane"
          data-testid="openshell-provider-types-host"
        >
          {providerTypes}
        </div>
      )}

      {tab === "policies" && (
        <div className="openshell-pane" data-testid="openshell-policies-host">
          {policies}
        </div>
      )}

      {tab === "profiles" && (
        <div className="openshell-pane" data-testid="openshell-profiles-host">
          {profiles}
        </div>
      )}
    </section>
  );
}

export function OpenShellPanel({
  activeTab,
  onTabChange,
}: {
  activeTab?: OpenShellTab;
  onTabChange?: (tab: OpenShellTab) => void;
} = {}) {
  const [status, setStatus] = useState<OpenShellStatus | null>(null);
  const [gatewayEndpoint, setGatewayEndpoint] = useState("");
  const [authMode, setAuthMode] = useState<OpenShellAuthMode | "">("");
  const [oidc, setOidc] = useState<OpenShellOidcConfig>(EMPTY_OIDC);
  const [oidcStatus, setOidcStatus] =
    useState<OpenShellSettings["oidc_status"]>();
  const [caPem, setCaPem] = useState("");
  const [clientCertPem, setClientCertPem] = useState("");
  const [clientKeyPem, setClientKeyPem] = useState("");
  const [mtls, setMtls] = useState<OpenShellSettings["mtls"]>();
  const [loading, setLoading] = useState(true);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [savedHint, setSavedHint] = useState<string | null>(null);
  const [tab, setTab] = useState<OpenShellTab>("connectivity");
  const [oidcAwaitingPaste, setOidcAwaitingPaste] = useState(false);
  const [oidcPaste, setOidcPaste] = useState("");
  const [oidcAuthorizeUrl, setOidcAuthorizeUrl] = useState("");
  const [oidcRedirectUri, setOidcRedirectUri] = useState("");

  const applySaved = useCallback((cfg: OpenShellSettings, st?: OpenShellStatus) => {
    setGatewayEndpoint(cfg.gateway_endpoint ?? st?.gateway_endpoint ?? "");
    setAuthMode(cfg.auth_mode ?? st?.auth_mode ?? "");
    setOidc(cfg.oidc ?? EMPTY_OIDC);
    setOidcStatus(cfg.oidc_status ?? st?.oidc_status);
    setMtls(cfg.mtls ?? st?.mtls);
    setCaPem("");
    setClientCertPem("");
    setClientKeyPem("");
  }, []);

  const refresh = useCallback(() => {
    setBusy(true);
    return Promise.all([api.getOpenShellStatus(), api.getOpenShell()])
      .then(([st, cfg]: [OpenShellStatus, OpenShellSettings]) => {
        setStatus(st);
        applySaved(cfg, st);
        setError(null);
      })
      .catch((e) => setError(String(e)))
      .finally(() => {
        setBusy(false);
        setLoading(false);
      });
  }, [applySaved]);

  useEffect(() => {
    refresh();
  }, [refresh]);

  // Return from /oauth/openshell/callback after IdP login.
  useEffect(() => {
    const params = new URLSearchParams(window.location.search);
    const status = params.get("openshell_oidc");
    if (!status) return;
    const message = params.get("message");
    const clean = () => {
      const url = new URL(window.location.href);
      url.searchParams.delete("openshell_oidc");
      url.searchParams.delete("message");
      window.history.replaceState({}, "", url.pathname + url.search + url.hash);
    };
    if (status === "ok") {
      setSavedHint("OIDC login complete.");
      refresh();
    } else if (status === "error") {
      setError(message ? decodeURIComponent(message) : "OIDC login failed");
    }
    clean();
  }, [refresh]);

  const put = (body: OpenShellSettings, hint: string) => {
    setBusy(true);
    setError(null);
    setSavedHint(null);
    api
      .putOpenShell(body)
      .then((saved) => {
        applySaved(saved);
        setSavedHint(hint);
        return api.getOpenShellStatus();
      })
      .then((st) => setStatus(st))
      .catch((e) => setError(String(e)))
      .finally(() => setBusy(false));
  };

  return (
    <OpenShellPanelView
      status={status}
      gatewayEndpoint={gatewayEndpoint}
      authMode={authMode}
      oidc={oidc}
      oidcStatus={oidcStatus}
      caPem={caPem}
      clientCertPem={clientCertPem}
      clientKeyPem={clientKeyPem}
      mtls={mtls}
      busy={busy || loading}
      error={error}
      savedHint={savedHint}
      activeTab={activeTab ?? tab}
      onTabChange={(next) => {
        onTabChange?.(next);
        if (activeTab === undefined) setTab(next);
      }}
      onGatewayEndpointChange={(next) => {
        setSavedHint(null);
        setGatewayEndpoint(next);
      }}
      onAuthModeChange={(next) => {
        setSavedHint(null);
        setAuthMode(next);
      }}
      onOidcChange={(next) => {
        setSavedHint(null);
        setOidc(next);
      }}
      onCaPemChange={(next) => {
        setSavedHint(null);
        setCaPem(next);
      }}
      onClientCertPemChange={(next) => {
        setSavedHint(null);
        setClientCertPem(next);
      }}
      onClientKeyPemChange={(next) => {
        setSavedHint(null);
        setClientKeyPem(next);
      }}
      onRefresh={() => {
        setSavedHint(null);
        refresh();
      }}
      onSave={() => {
        if (!authMode) {
          setError("Pick an auth mode (mTLS or OIDC).");
          return;
        }
        const body: OpenShellSettings = {
          gateway_endpoint: gatewayEndpoint.trim() || null,
          auth_mode: authMode,
        };
        if (authMode === "mtls") {
          if (caPem.trim()) body.ca_pem = caPem;
          if (clientCertPem.trim()) body.client_cert_pem = clientCertPem;
          if (clientKeyPem.trim()) body.client_key_pem = clientKeyPem;
        }
        if (authMode === "oidc") {
          body.oidc = {
            issuer: oidc.issuer.trim(),
            client_id: oidc.client_id.trim(),
            audience: oidc.audience.trim(),
          };
        }
        put(
          body,
          authMode === "mtls"
            ? "Saved. mTLS PEMs are sealed in the board database."
            : "Saved. Log in to store OIDC tokens.",
        );
      }}
      onClearMtls={() => {
        put(
          {
            gateway_endpoint: gatewayEndpoint.trim() || null,
            auth_mode: "mtls",
            clear_mtls: true,
          },
          "Cleared sealed mTLS material.",
        );
      }}
      oidcAwaitingPaste={oidcAwaitingPaste}
      oidcPaste={oidcPaste}
      oidcAuthorizeUrl={oidcAuthorizeUrl}
      oidcRedirectUri={oidcRedirectUri}
      onOidcPasteChange={setOidcPaste}
      onOidcCompletePaste={() => {
        const redirect = oidcPaste.trim();
        if (!redirect) {
          setError("paste the loopback redirect URL from the address bar");
          return;
        }
        setBusy(true);
        setError(null);
        api
          .openshellOidcComplete({ redirect })
          .then(() => {
            setOidcAwaitingPaste(false);
            setOidcPaste("");
            setOidcAuthorizeUrl("");
            setOidcRedirectUri("");
            setSavedHint("OIDC login complete.");
            return refresh();
          })
          .catch((e) => setError(String(e)))
          .finally(() => setBusy(false));
      }}
      onOidcLogin={() => {
        setBusy(true);
        setError(null);
        setSavedHint(null);
        setOidcPaste("");
        const body: OpenShellSettings = {
          gateway_endpoint: gatewayEndpoint.trim() || null,
          auth_mode: "oidc",
          oidc: {
            issuer: oidc.issuer.trim(),
            client_id: oidc.client_id.trim(),
            audience: oidc.audience.trim(),
          },
        };
        api
          .putOpenShell(body)
          .then(() => api.openshellOidcLogin())
          .then((out) => {
            window.open(out.authorize_url, "_blank", "noopener,noreferrer");
            setOidcAuthorizeUrl(out.authorize_url);
            setOidcRedirectUri(out.redirect_uri);
            setOidcAwaitingPaste(true);
            setSavedHint(
              "IdP login opened in a new tab. Paste the loopback redirect URL here when the page fails to load.",
            );
          })
          .catch((e) => setError(String(e)))
          .finally(() => setBusy(false));
      }}
      onOidcLogout={() => {
        setBusy(true);
        setError(null);
        api
          .openshellOidcLogout()
          .then(() => {
            setOidcAwaitingPaste(false);
            setOidcPaste("");
            setOidcAuthorizeUrl("");
            setOidcRedirectUri("");
            setSavedHint("Logged out of OpenShell OIDC.");
            return refresh();
          })
          .catch((e) => setError(String(e)))
          .finally(() => setBusy(false));
      }}
      providers={<OpenShellProvidersPanel gatewayHealthy={!!status?.healthy} />}
      providerTypes={<OpenShellProviderTypesPanel />}
      policies={<OpenShellPoliciesPanel />}
      profiles={<SandboxesPanel />}
    />
  );
}
