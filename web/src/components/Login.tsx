import { useMemo, useState, type FormEvent } from "react";
import { api } from "../api.js";
import type { AuthStatus } from "../types.js";

export function Login({
  status,
  onAuthed,
}: {
  status: AuthStatus;
  onAuthed: (next: AuthStatus) => void;
}) {
  const [username, setUsername] = useState("");
  const [password, setPassword] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const authError = useMemo(() => {
    const q = new URLSearchParams(window.location.search);
    const e = q.get("auth_error");
    if (e === "not_allowlisted") {
      return "Your GitHub account is not on the allowlist.";
    }
    return null;
  }, []);

  /** Safe same-origin return for MCP OAuth authorize after board login. */
  const returnNext = useMemo(() => {
    const raw = new URLSearchParams(window.location.search).get("next");
    if (!raw || !raw.startsWith("/oauth/authorize")) return null;
    if (raw.includes("//") || raw.includes("\\")) return null;
    return raw;
  }, []);

  const bootstrap = status.bootstrap;
  const submitLabel = bootstrap ? "Create admin & continue" : "Sign in";

  const finishAuthed = (next: AuthStatus) => {
    if (returnNext) {
      window.location.assign(returnNext);
      return;
    }
    if (window.location.search) {
      window.history.replaceState({}, "", window.location.pathname);
    }
    onAuthed(next);
  };

  const submit = (e: FormEvent) => {
    e.preventDefault();
    setBusy(true);
    setError(null);
    const body = { username: username.trim(), password };
    const req = bootstrap ? api.bootstrap(body) : api.login(body);
    req
      .then(finishAuthed)
      .catch((err) => setError(String(err)))
      .finally(() => setBusy(false));
  };

  return (
    <div className="login-shell" data-testid="login">
      <div className="login-atmosphere" aria-hidden="true" />
      <section className="login-panel" aria-labelledby="login-title">
        <header className="login-brand">
          <img
            src="/sandboard-logo.png"
            alt=""
            className="login-brand-mark"
            width={72}
            height={72}
          />
          <h1 id="login-title" className="login-wordmark">
            sandboard
          </h1>
          <p className="login-tagline">A board for agent work.</p>
        </header>

        {bootstrap && (
          <p className="login-bootstrap" data-testid="login-bootstrap">
            First run — set a local admin password. The board stays locked until
            this is done.
          </p>
        )}

        {(error || authError) && (
          <div className="err login-error" data-testid="login-error">
            {error || authError}
          </div>
        )}

        <form className="login-form" onSubmit={submit} data-testid="login-form">
          <label>
            Username
            <input
              className="login-input"
              autoComplete="username"
              value={username}
              disabled={busy}
              onChange={(e) => setUsername(e.target.value)}
              data-testid="login-username"
            />
          </label>
          <label>
            Password
            <input
              className="login-input"
              type="password"
              autoComplete={bootstrap ? "new-password" : "current-password"}
              value={password}
              disabled={busy}
              onChange={(e) => setPassword(e.target.value)}
              data-testid="login-password"
            />
          </label>
          {!bootstrap && password.length > 0 && password.length < 8 && (
            <p className="login-hint">Password must be at least 8 characters.</p>
          )}
          <button
            type="submit"
            className="primary login-submit"
            disabled={busy || !username.trim() || password.length < 8}
            data-testid="login-submit"
          >
            {submitLabel}
          </button>
        </form>

        {!bootstrap && status.github_login_enabled && (
          <div className="login-alt" data-testid="login-github">
            <div className="login-divider" aria-hidden="true">
              <span>or</span>
            </div>
            <a
              className="login-github-btn"
              href={`/auth/github?return_origin=${encodeURIComponent(window.location.origin)}${
                returnNext
                  ? `&next=${encodeURIComponent(returnNext)}`
                  : ""
              }`}
            >
              Sign in with GitHub
            </a>
          </div>
        )}

        {!bootstrap && !status.github_login_enabled && (
          <p className="login-hint" data-testid="login-github-disabled">
            GitHub login needs Client ID + Client secret on the shipped
            github-app provider (Settings → OpenShell → Providers; after you
            sign in as local admin).
          </p>
        )}
      </section>
    </div>
  );
}
