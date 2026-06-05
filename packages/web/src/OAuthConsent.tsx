import { createResource, createSignal, For, Show, type Component } from "solid-js";
import { api } from "./api.ts";

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

  const [ctx] = createResource(() => api.oauthContext(clientId));
  const [chosen, setChosen] = createSignal<string>("");
  const [error, setError] = createSignal<string | null>(null);
  const [busy, setBusy] = createSignal(false);

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
        <div class="auth-brand"><span class="logo">🐝</span><span class="brand-name">hive</span></div>
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
            <p class="dim">This issues a long-lived token (1 year) that identifies every MCP action as that AI. You can revoke it anytime from the Account tab.</p>
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
