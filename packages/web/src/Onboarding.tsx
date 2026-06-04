import { createSignal, Show, type Component } from "solid-js";
import { api, setCurrentUser } from "./api.ts";

// First-run setup (v0.1.1). Shown only when /onboarding/status reports the
// instance hasn't been set up. Creates the first admin and names the instance,
// then logs the admin straight in.
export const Onboarding: Component<{ onDone: () => void }> = (props) => {
  const [instanceName, setInstanceName] = createSignal("Bee's Roadhouse hive");
  const [adminName, setAdminName] = createSignal("");
  const [adminEmail, setAdminEmail] = createSignal("");
  const [password, setPassword] = createSignal("");
  const [confirm, setConfirm] = createSignal("");
  const [error, setError] = createSignal<string | null>(null);
  const [busy, setBusy] = createSignal(false);

  const submit = async (e: Event) => {
    e.preventDefault();
    setError(null);
    if (password().length < 8) return setError("Password must be at least 8 characters.");
    if (password() !== confirm()) return setError("Passwords don't match.");
    setBusy(true);
    try {
      const { user } = await api.onboard({
        instanceName: instanceName().trim(),
        adminName: adminName().trim(),
        adminEmail: adminEmail().trim(),
        password: password(),
      });
      setCurrentUser(user);
      props.onDone();
    } catch (err) {
      setError(String(err instanceof Error ? err.message : err));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div class="auth-screen">
      <form class="auth-card" onSubmit={submit}>
        <div class="auth-brand">
          <span class="logo">🐝</span>
          <span class="brand-name">hive</span>
        </div>
        <h1>Welcome — let's set up hive</h1>
        <p class="dim">Create the first admin account and name this instance.</p>

        <label>
          Instance name
          <input value={instanceName()} onInput={(e) => setInstanceName(e.currentTarget.value)} required />
        </label>
        <label>
          Your name
          <input value={adminName()} onInput={(e) => setAdminName(e.currentTarget.value)} placeholder="Nate Smith" required />
        </label>
        <label>
          Email
          <input type="email" value={adminEmail()} onInput={(e) => setAdminEmail(e.currentTarget.value)} required />
        </label>
        <label>
          Password
          <input type="password" value={password()} onInput={(e) => setPassword(e.currentTarget.value)} required />
        </label>
        <label>
          Confirm password
          <input type="password" value={confirm()} onInput={(e) => setConfirm(e.currentTarget.value)} required />
        </label>

        <Show when={error()}>
          <p class="auth-error">{error()}</p>
        </Show>
        <button type="submit" disabled={busy()}>
          {busy() ? "Setting up…" : "Create admin & continue"}
        </button>
      </form>
    </div>
  );
};
