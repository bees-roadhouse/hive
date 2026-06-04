import { createResource, createSignal, For, Show, type Component } from "solid-js";
import { ACTORS, type UserRole } from "@hive/shared";
import { api, getCurrentUser } from "./api.ts";

// Admin panel (v0.1.1): manage login users and programmatic API tokens.
// Only rendered for admins (App gates the tab). Non-admins never see it.
export const Account: Component = () => {
  const me = getCurrentUser();
  const [users, { refetch: refetchUsers }] = createResource(() => api.users());
  const [tokens, { refetch: refetchTokens }] = createResource(() => api.apiTokens());

  // new user form
  const [uName, setUName] = createSignal("");
  const [uEmail, setUEmail] = createSignal("");
  const [uPass, setUPass] = createSignal("");
  const [uRole, setURole] = createSignal<UserRole>("member");
  const [uErr, setUErr] = createSignal<string | null>(null);

  const addUser = async (e: Event) => {
    e.preventDefault();
    setUErr(null);
    try {
      await api.addUser({ name: uName().trim(), email: uEmail().trim(), password: uPass(), role: uRole() });
      setUName("");
      setUEmail("");
      setUPass("");
      refetchUsers();
    } catch (err) {
      setUErr(String(err instanceof Error ? err.message : err));
    }
  };

  // new token form — actor defaults to the AI actors (tokens are mostly for them)
  const [tActor, setTActor] = createSignal("pia");
  const [tLabel, setTLabel] = createSignal("");
  const [freshToken, setFreshToken] = createSignal<string | null>(null);

  const mintToken = async (e: Event) => {
    e.preventDefault();
    const { token } = await api.createToken(tActor().trim(), tLabel().trim() || `${tActor()} token`);
    setFreshToken(token); // shown once
    setTLabel("");
    refetchTokens();
  };

  const revoke = async (id: string) => {
    await api.deleteToken(id);
    refetchTokens();
  };

  return (
    <div class="account">
      <section>
        <h3>Users</h3>
        <table class="data-table">
          <thead>
            <tr><th>Name</th><th>Email</th><th>Actor</th><th>Role</th></tr>
          </thead>
          <tbody>
            <For each={users() ?? []}>
              {(u) => (
                <tr>
                  <td>{u.name}{u.id === me?.id ? " (you)" : ""}</td>
                  <td>{u.email}</td>
                  <td>{u.actor}</td>
                  <td>{u.role}</td>
                </tr>
              )}
            </For>
          </tbody>
        </table>
        <form class="inline-form" onSubmit={addUser}>
          <input placeholder="name" value={uName()} onInput={(e) => setUName(e.currentTarget.value)} required />
          <input type="email" placeholder="email" value={uEmail()} onInput={(e) => setUEmail(e.currentTarget.value)} required />
          <input type="password" placeholder="password (8+)" value={uPass()} onInput={(e) => setUPass(e.currentTarget.value)} required />
          <select value={uRole()} onChange={(e) => setURole(e.currentTarget.value as UserRole)}>
            <option value="member">member</option>
            <option value="admin">admin</option>
          </select>
          <button type="submit">Add user</button>
        </form>
        <Show when={uErr()}><p class="auth-error">{uErr()}</p></Show>
      </section>

      <section>
        <h3>API tokens</h3>
        <p class="dim">Bearer tokens for programmatic clients (CLI, MCP, AI agents). Set <code>HIVE_API_TOKEN</code> to one.</p>
        <Show when={freshToken()}>
          <div class="token-reveal">
            <strong>Copy this token now — it won't be shown again:</strong>
            <code>{freshToken()}</code>
          </div>
        </Show>
        <table class="data-table">
          <thead>
            <tr><th>Actor</th><th>Label</th><th>Created</th><th>Last used</th><th></th></tr>
          </thead>
          <tbody>
            <For each={tokens() ?? []}>
              {(t) => (
                <tr>
                  <td>{t.actor}</td>
                  <td>{t.label}</td>
                  <td>{t.created_at.slice(0, 10)}</td>
                  <td>{t.last_used_at ? t.last_used_at.slice(0, 10) : "—"}</td>
                  <td><button class="danger" onClick={() => revoke(t.id)}>revoke</button></td>
                </tr>
              )}
            </For>
          </tbody>
        </table>
        <form class="inline-form" onSubmit={mintToken}>
          <select value={tActor()} onChange={(e) => setTActor(e.currentTarget.value)}>
            <For each={ACTORS}>{(a) => <option value={a.name}>{a.name} ({a.kind})</option>}</For>
          </select>
          <input placeholder="label (e.g. pia laptop)" value={tLabel()} onInput={(e) => setTLabel(e.currentTarget.value)} />
          <button type="submit">Mint token</button>
        </form>
      </section>
    </div>
  );
};
