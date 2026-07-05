import { createResource, createSignal, For, Show, type Component } from "solid-js";
import { ACTORS, API_TOKEN_DEFAULT_EXPIRY_DAYS, type ImportResult, type UserRole } from "@hive/shared";
import { api, getCurrentUser } from "./api.ts";

// Token expiry presets (days). 0 asks the server for a non-expiring token.
const EXPIRY_OPTIONS = [
  { label: "30 days", days: 30 },
  { label: "90 days", days: 90 },
  { label: "180 days", days: 180 },
  { label: "1 year", days: 365 },
  { label: "Never", days: 0 },
];

// Admin panel (v0.1.1): manage login users and programmatic API tokens.
// Only rendered for admins (App gates the tab). Non-admins never see it.
export const Account: Component = () => {
  const me = getCurrentUser();
  const [users, { refetch: refetchUsers }] = createResource(() => api.users());
  const [tokens, { refetch: refetchTokens }] = createResource(() => api.apiTokens());
  const [apps, { refetch: refetchApps }] = createResource(() => api.oauthClients());

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
  const [tExpiry, setTExpiry] = createSignal(API_TOKEN_DEFAULT_EXPIRY_DAYS);
  const [freshToken, setFreshToken] = createSignal<string | null>(null);

  const mintToken = async (e: Event) => {
    e.preventDefault();
    const neverExpires = tExpiry() === 0;
    const { token } = await api.createToken(
      tActor().trim(),
      tLabel().trim() || `${tActor()} token`,
      neverExpires ? undefined : tExpiry(),
      neverExpires,
    );
    setFreshToken(token); // shown once
    setTLabel("");
    refetchTokens();
  };

  const revoke = async (id: string) => {
    await api.deleteToken(id);
    refetchTokens();
  };

  // connected apps: revoke every token a client holds (disconnects it).
  const revokeApp = async (id: string, name: string) => {
    if (!confirm(`Disconnect "${name}"? This revokes all of its tokens.`)) return;
    await api.revokeOAuthClient(id);
    refetchApps();
    refetchTokens();
  };

  // memory-namespace bulk reassignment (admin-only, bulk → confirm first)
  const [rUnscoped, setRUnscoped] = createSignal(false);
  const [rFromUser, setRFromUser] = createSignal("");
  const [rAuthor, setRAuthor] = createSignal("");
  const [rTo, setRTo] = createSignal("");
  const [rResult, setRResult] = createSignal<string | null>(null);
  const [rErr, setRErr] = createSignal<string | null>(null);

  const reassignScope = async (e: Event) => {
    e.preventDefault();
    setRResult(null);
    setRErr(null);
    const from = rFromUser().trim();
    const author = rAuthor().trim();
    const to = rTo().trim();
    const target = to ? `user "${to}"` : "global (no owner)";
    if (!confirm(`Bulk-reassign matching journal entries to ${target}? This can't be undone in one click.`))
      return;
    try {
      const { changed } = await api.reassignJournalScope({
        match_unscoped: rUnscoped(),
        from_user: from || undefined,
        author: author || undefined,
        to: to ? to : null,
      });
      setRResult(`${changed} entries reassigned.`);
    } catch (err) {
      setRErr(String(err instanceof Error ? err.message : err));
    }
  };

  // legacy import (upload an old hive.db)
  const [importFile, setImportFile] = createSignal<File | null>(null);
  const [importing, setImporting] = createSignal(false);
  const [importResult, setImportResult] = createSignal<(ImportResult & { warnings: string[] }) | null>(null);
  const [importErr, setImportErr] = createSignal<string | null>(null);

  const runImport = async (e: Event) => {
    e.preventDefault();
    const file = importFile();
    if (!file) return;
    setImporting(true);
    setImportErr(null);
    setImportResult(null);
    try {
      setImportResult(await api.importSqlite(file));
      refetchTokens();
    } catch (err) {
      setImportErr(String(err instanceof Error ? err.message : err));
    } finally {
      setImporting(false);
    }
  };

  const isExpired = (iso: string | null): boolean => !!iso && Date.parse(iso) < Date.now();

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
            <tr><th>Actor</th><th>Label</th><th>Created</th><th>Expires</th><th>Last used</th><th></th></tr>
          </thead>
          <tbody>
            <For each={tokens() ?? []}>
              {(t) => (
                <tr classList={{ "row-expired": isExpired(t.expires_at) }}>
                  <td>{t.actor}</td>
                  <td>{t.label}</td>
                  <td>{t.created_at.slice(0, 10)}</td>
                  <td>
                    {t.expires_at
                      ? isExpired(t.expires_at)
                        ? `expired ${t.expires_at.slice(0, 10)}`
                        : t.expires_at.slice(0, 10)
                      : "never"}
                  </td>
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
          <select value={tExpiry()} onChange={(e) => setTExpiry(Number(e.currentTarget.value))} title="Token expiry">
            <For each={EXPIRY_OPTIONS}>{(o) => <option value={o.days}>{o.label}</option>}</For>
          </select>
          <button type="submit">Mint token</button>
        </form>
      </section>

      <section>
        <h3>Connected apps</h3>
        <p class="dim">OAuth clients that have been granted access via the consent flow. Revoking disconnects the app by deleting all of its tokens.</p>
        <table class="data-table">
          <thead>
            <tr><th>App</th><th>Connected</th><th>Active tokens</th><th>Last used</th><th></th></tr>
          </thead>
          <tbody>
            <For each={apps() ?? []}>
              {(c) => (
                <tr>
                  <td>{c.client_name}</td>
                  <td>{c.created_at.slice(0, 10)}</td>
                  <td>{c.active_tokens}</td>
                  <td>{c.last_used_at ? c.last_used_at.slice(0, 10) : "—"}</td>
                  <td><button class="danger" onClick={() => revokeApp(c.client_id, c.client_name)}>Revoke app</button></td>
                </tr>
              )}
            </For>
          </tbody>
        </table>
        <Show when={(apps() ?? []).length === 0}><p class="dim">No apps connected.</p></Show>
      </section>

      <section>
        <h3>Memory namespaces</h3>
        <p class="dim">
          Admin tool: bulk-reassign journal ownership across per-user memory namespaces. Filters are
          ANDed; an empty "to user" makes matched entries global (visible to everyone). This is a bulk
          mutation — you'll be asked to confirm.
        </p>
        <form class="inline-form" onSubmit={reassignScope}>
          <label class="checkbox-label">
            <input type="checkbox" checked={rUnscoped()} onChange={(e) => setRUnscoped(e.currentTarget.checked)} />
            match unscoped (global) entries
          </label>
          <input placeholder="from user (optional)" value={rFromUser()} onInput={(e) => setRFromUser(e.currentTarget.value)} />
          <input placeholder="author (optional)" value={rAuthor()} onInput={(e) => setRAuthor(e.currentTarget.value)} />
          <input placeholder="to user (blank = global)" value={rTo()} onInput={(e) => setRTo(e.currentTarget.value)} />
          <button type="submit" class="danger">Reassign</button>
        </form>
        <Show when={rResult()}><p class="dim">{rResult()}</p></Show>
        <Show when={rErr()}><p class="auth-error">{rErr()}</p></Show>
      </section>

      <section>
        <h3>Import legacy data</h3>
        <p class="dim">
          Upload an old <code>hive.db</code> (SQLite). Journal, tasks, projects, links, and cross-AI
          messages are imported with their original ids and timestamps — re-running is safe (existing
          rows are skipped).
        </p>
        <form class="inline-form" onSubmit={runImport}>
          <input
            type="file"
            accept=".db,.sqlite,.sqlite3,application/x-sqlite3,application/octet-stream"
            onChange={(e) => setImportFile(e.currentTarget.files?.[0] ?? null)}
          />
          <button type="submit" disabled={!importFile() || importing()}>
            {importing() ? "Importing…" : "Import"}
          </button>
        </form>
        <Show when={importResult()}>
          {(r) => (
            <div class="import-result">
              <p>
                Imported — journal {r().journal.inserted} new / {r().journal.skipped} skipped ·
                tasks {r().tasks.inserted}/{r().tasks.skipped} · projects {r().projects.inserted}/
                {r().projects.skipped} · links {r().links.inserted}/{r().links.skipped}
              </p>
              <Show when={r().warnings.length}>
                <ul class="dim sm">
                  <For each={r().warnings}>{(w) => <li>warning: {w}</li>}</For>
                </ul>
              </Show>
            </div>
          )}
        </Show>
        <Show when={importErr()}><p class="auth-error">{importErr()}</p></Show>
      </section>
    </div>
  );
};
