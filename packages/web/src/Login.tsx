import { createSignal, Show, type Component } from "solid-js";
import { api, setCurrentUser } from "./api.ts";

// Login screen. Shown when onboarding is complete but no valid session exists.
export const Login: Component<{ instanceName: string | null; onLogin: () => void }> = (props) => {
  const [email, setEmail] = createSignal("");
  const [password, setPassword] = createSignal("");
  const [error, setError] = createSignal<string | null>(null);
  const [busy, setBusy] = createSignal(false);

  const submit = async (e: Event) => {
    e.preventDefault();
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
      </form>
    </div>
  );
};
