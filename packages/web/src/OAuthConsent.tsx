import { createResource, createSignal, For, Show, type Component } from "solid-js";
import { api } from "./api.ts";
import { Icon } from "./icons.tsx";

// OAuth consent screen (the "sign in as the AI" step). Reached at /consent after
// /authorize validated the request. The human is already signed in (App gates
// this behind auth); they pick which AI identity to grant, and we hand an auth
// code back to the requesting MCP client.
export const OAuthConsent: Component = () => {
  const params = new URLSearchParams(window.location.search);
  const clientId = params.get("client_id") ?? "";
  const redirectUri = params.get("redirect_uri") ?? "";
  const codeChallenge = params.get("code_challenge") ?? "";
  const state = params.get("state") ?? "";
  const scope = params.get("scope") ?? "mcp";

  // Token-lifetime presets (seconds). 0 is the server's "never expires" sentinel.
  const BASE_LIFETIMES = [
    { label: "7 days", secs: 7 * 24 * 60 * 60 },
    { label: "30 days", secs: 30 * 24 * 60 * 60 },
    { label: "90 days", secs: 90 * 24 * 60 * 60 },
    { label: "1 year", secs: 365 * 24 * 60 * 60 },
  ];

  const [ctx] = createResource(() => api.oauthContext(clientId));
  const [chosen, setChosen] = createSignal<string>("");
  const [ttl, setTtl] = createSignal<number>(365 * 24 * 60 * 60);
  const [error, setError] = createSignal<string | null>(null);
  const [busy, setBusy] = createSignal(false);

  const lifetimes = () =>
    ctx()?.allow_never_expires ? [...BASE_LIFETIMES, { label: "Never", secs: 0 }] : BASE_LIFETIMES;
  const ttlLabel = () => lifetimes().find((l) => l.secs === ttl())?.label ?? "1 year";

  const approve = async () => {
    const ident = chosen() || ctx()?.identities[0]?.slug;
    if (!ident) return setError("No AI identity to grant.");
    setBusy(true);
    try {
      const { redirect } = await api.oauthGrant({
        client_id: clientId,
        redirect_uri: redirectUri,
        code_challenge: codeChallenge,
        state,
        scope,
        ai_actor: ident,
        csrf: ctx()!.csrf,
        token_ttl_secs: ttl(),
      });
      window.location.href = redirect;
    } catch (err) {
      setError(String(err instanceof Error ? err.message : err));
      setBusy(false);
    }
  };

  const deny = () => {
    const sep = redirectUri.includes("?") ? "&" : "?";
    window.location.href = `${redirectUri}${sep}error=access_denied${state ? `&state=${encodeURIComponent(state)}` : ""}`;
  };

  return (
    <div class="auth-screen">
      <div class="auth-card">
        <div class="auth-brand"><span class="brand-logo"><Icon name="hex" size={28} /></span><span class="brand-name">hive</span></div>
        <Show when={ctx()} fallback={<p class="dim">Loading…</p>}>
          <h1>Authorize access</h1>
          <p>
            <strong>{ctx()!.client_name}</strong> wants to connect to hive as one of your AI identities.
          </p>
          <Show
            when={ctx()!.identities.length > 0}
            fallback={<p class="auth-error">You don't own any AI identities to grant. Ask an admin to assign one to you.</p>}
          >
            <label>
              Connect as
              <select value={chosen()} onChange={(e) => setChosen(e.currentTarget.value)}>
                <For each={ctx()!.identities}>{(i) => <option value={i.slug}>{i.name} ({i.slug})</option>}</For>
              </select>
            </label>
            <label>
              Access lasts
              <select value={ttl()} onChange={(e) => setTtl(Number(e.currentTarget.value))}>
                <For each={lifetimes()}>{(l) => <option value={l.secs}>{l.label}</option>}</For>
              </select>
            </label>
            <p class="dim">This issues a {ttl() === 0 ? "non-expiring token" : `token that lasts ${ttlLabel()}`} and identifies every MCP action as that AI. You can revoke it anytime from the Account tab.</p>
            <Show when={error()}><p class="auth-error">{error()}</p></Show>
            <div class="consent-actions">
              <button class="logout" onClick={deny} disabled={busy()}>Deny</button>
              <button type="button" onClick={approve} disabled={busy()}>{busy() ? "Authorizing…" : "Approve"}</button>
            </div>
          </Show>
        </Show>
      </div>
    </div>
  );
};
