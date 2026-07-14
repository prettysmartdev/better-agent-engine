import { useState, type FormEvent } from "react";
import { ApiError, login } from "../api/client";
import { Wordmark } from "./ui";

/** The login shell: posts the MAX password once to obtain a session cookie. */
export default function Login({ onSuccess }: { onSuccess: () => void }) {
  const [password, setPassword] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  async function submit(e: FormEvent) {
    e.preventDefault();
    setBusy(true);
    setError(null);
    try {
      await login(password);
      onSuccess();
    } catch (err) {
      // The server returns an identical error for any wrong password.
      if (err instanceof ApiError && err.status === 401) {
        setError("Incorrect password.");
      } else {
        setError(err instanceof Error ? err.message : "Login failed.");
      }
      setBusy(false);
    }
  }

  return (
    <div className="login-screen">
      <form className="login-card" onSubmit={submit}>
        <Wordmark className="login-wordmark" />
        <p className="login-sub">Sign in to administer this BAE instance.</p>
        <label className="field">
          <span className="field-label">MAX password</span>
          <input
            type="password"
            name="password"
            autoComplete="current-password"
            autoFocus
            value={password}
            onChange={(e) => setPassword(e.target.value)}
            disabled={busy}
          />
        </label>
        {error && (
          <div className="banner banner-error" role="alert">
            {error}
          </div>
        )}
        <button
          type="submit"
          className="btn btn-primary"
          disabled={busy || !password}
        >
          {busy ? "Signing in…" : "Sign in"}
        </button>
      </form>
    </div>
  );
}
