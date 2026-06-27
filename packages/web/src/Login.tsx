import { createResource, createSignal, Show, type Component } from "solid-js";
import { api, setCurrentUser } from "./api.ts";

// Login screen. Shown when onboarding is complete but no valid session exists.
export const Login: Component<{ instanceName: string | null; onLogin: () => void }> = (props) => {
  const [email, setEmail] = createSignal("");
  const [password, setPassword] = createSignal("");
  const [error, setError] = createSignal<string | null>(null);
  const [busy, setBusy] = createSignal(false);
  const [cfg] = createResource(() => api.authConfig());
  const localAuth = () => cfg()?.localAuth ?? true;

  const ssoLogin = () => {
    const returnTo = window.location.pathname + window.location.search;
    window.location.href = `/api/auth/oidc/start?return_to=${encodeURIComponent(returnTo)}`;
  };

  const submit = async (e: Event) => {
    e.preventDefault();
    if (!localAuth()) return;
    setError(null);
    setBusy(true);
    try {
      const { user } = await api.login(email().trim(), password());
      setCurrentUser(user);
      props.onLogin();
    } catch {
      setError("Invalid email or password.");
    } finally {
      setBusy(false);
    }
  };

  return (
    <div class="auth-screen">
      <form class="auth-card" onSubmit={submit}>
        <div class="auth-brand">
          <span class="logo">🐝</span>
          <span class="brand-name">{props.instanceName ?? "hive"}</span>
        </div>
        <h1>Sign in</h1>
        <Show when={localAuth()}>
          <>
          <label>
            Email
            <input type="email" value={email()} onInput={(e) => setEmail(e.currentTarget.value)} required />
          </label>
          <label>
            Password
            <input type="password" value={password()} onInput={(e) => setPassword(e.currentTarget.value)} required />
          </label>
          <Show when={error()}>
            <p class="auth-error">{error()}</p>
          </Show>
          <button type="submit" disabled={busy()}>
            {busy() ? "Signing in…" : "Sign in"}
          </button>
          </>
        </Show>
        <Show when={cfg()?.oidc}>
          <button type="button" class="logout" onClick={ssoLogin}>Sign in with SSO</button>
        </Show>
        <Show when={cfg() && !localAuth() && !cfg()?.oidc}>
          <p class="auth-error">No sign-in method is enabled.</p>
        </Show>
      </form>
    </div>
  );
};
