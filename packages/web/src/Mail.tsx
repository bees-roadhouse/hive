import { createEffect, createMemo, createResource, createSignal, For, Show, type Component } from "solid-js";
import type { MailAccount, MailMessageSummary, MailThreadMessage } from "@hive/shared";
import { useSearchParams } from "@solidjs/router";
import { api } from "./api.ts";
import { Icon } from "./icons.tsx";
import { liveRev } from "./live.ts";
import { EmptyState } from "./primitives.tsx";

const shortDate = (iso: string): string => {
  const t = Date.parse(iso);
  if (Number.isNaN(t)) return "";
  return new Date(t).toLocaleString(undefined, { month: "short", day: "numeric", hour: "2-digit", minute: "2-digit" });
};

const oneLine = (s: string | null | undefined, max = 110): string => {
  const flat = (s ?? "").replace(/\s+/g, " ").trim();
  return flat.length > max ? `${flat.slice(0, max)}…` : flat;
};

const names = (xs: string[] | undefined): string => (xs && xs.length ? xs.join(", ") : "—");

const fmtBytes = (n: number): string => {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(0)} KB`;
  return `${(n / (1024 * 1024)).toFixed(1)} MB`;
};

const AccountLabel: Component<{ id: string; accounts: MailAccount[] }> = (props) => {
  const acct = () => props.accounts.find((a) => a.id === props.id);
  return <span>{acct()?.label ?? acct()?.address ?? props.id}</span>;
};

const MailBody: Component<{ message: MailThreadMessage }> = (props) => (
  <article class="mail-msg">
    <header class="mail-msg-head">
      <div>
        <strong>{props.message.from}</strong>
        <div class="mail-recipients dim sm">to {names(props.message.to)}</div>
      </div>
      <time class="dim sm">{shortDate(props.message.received_at)}</time>
    </header>
    <pre class="mail-plain">{props.message.body_text || "(no plaintext body)"}</pre>
    <Show when={props.message.attachments?.length}>
      <div class="mail-atts">
        <For each={props.message.attachments}>
          {(a) =>
            a.stored ? (
              <a
                class="mail-label mail-att"
                href={`/api/mail/attachments/${encodeURIComponent(a.id)}`}
                target="_blank"
                rel="noopener"
                title={`${a.mime} · ${fmtBytes(a.size)}`}
              >
                {a.filename || "(unnamed)"} <span class="mail-att-size">{fmtBytes(a.size)}</span>
              </a>
            ) : (
              <span
                class="mail-label mail-att mail-att-missing"
                title={`${a.mime} · ${fmtBytes(a.size)} — bytes not stored (oversize or missing on the server)`}
              >
                {a.filename || "(unnamed)"} <span class="mail-att-size">{fmtBytes(a.size)}</span>
              </span>
            )
          }
        </For>
      </div>
    </Show>
  </article>
);

export const Mail: Component = () => {
  const [params, setParams] = useSearchParams();
  const [draft, setDraft] = createSignal(typeof params.q === "string" ? params.q : "");
  const selectedThread = () => (typeof params.thread === "string" ? params.thread : null);
  const targetMessage = () => (typeof params.message === "string" ? params.message : null);
  const accountId = () => (typeof params.account_id === "string" ? params.account_id : "");

  const [accounts] = createResource(() => liveRev(), () => api.mailAccounts());
  const [messages] = createResource(
    () => ({ q: typeof params.q === "string" ? params.q : "", account_id: accountId(), _r: liveRev() }),
    (k) => api.mailMessages({ query: k.q, account_id: k.account_id || undefined }),
  );
  const [thread] = createResource(
    () => selectedThread(),
    (id) => (id ? api.mailThread(id) : Promise.resolve(null)),
  );

  const rows = () => messages.latest ?? [];
  const accts = () => accounts.latest ?? [];
  const current = createMemo(() => rows().find((m) => m.thread_id === selectedThread()) ?? null);

  createEffect(() => {
    const msg = targetMessage();
    if (!msg) return;
    const hit = rows().find((m) => m.id === msg);
    if (hit && selectedThread() !== hit.thread_id) setParams({ thread: hit.thread_id, message: msg });
  });

  createEffect(() => {
    if (selectedThread() || !rows().length) return;
    setParams({ thread: rows()[0]!.thread_id });
  });

  const select = (m: MailMessageSummary) => setParams({ thread: m.thread_id, message: undefined });
  const applySearch = () => setParams({ q: draft().trim() || undefined, thread: undefined, message: undefined });

  return (
    <div class="mail">
      <aside class="mail-rail">
        <div class="mail-tools">
          <div class="mail-search">
            <input
              placeholder="search mail…"
              value={draft()}
              onInput={(e) => setDraft(e.currentTarget.value)}
              onKeyDown={(e) => { if (e.key === "Enter") applySearch(); }}
            />
            <button class="ghost" onClick={applySearch}>search</button>
          </div>
          <select
            value={accountId()}
            onChange={(e) => setParams({ account_id: e.currentTarget.value || undefined, thread: undefined, message: undefined })}
            aria-label="mail account"
          >
            <option value="">all accounts</option>
            <For each={accts()}>
              {(a) => <option value={a.id}>{a.label || a.address}</option>}
            </For>
          </select>
        </div>
        <div class="mail-rows">
          <For
            each={rows()}
            fallback={<EmptyState icon="mail" title="No mail found." hint="Synced plaintext messages will appear here." />}
          >
            {(m) => (
              <button class="mail-row" classList={{ selected: selectedThread() === m.thread_id }} onClick={() => select(m)}>
                <span class="mail-row-top">
                  <strong>{m.subject || "(no subject)"}</strong>
                  <time>{shortDate(m.received_at)}</time>
                </span>
                <span class="mail-row-from">{m.from}</span>
                <span class="mail-row-snippet">{oneLine(m.snippet)}</span>
                <span class="mail-row-labels">
                  <For each={m.labels?.filter((label) => label !== "seen").slice(0, 4) ?? []}>
                    {(label) => <span class="mail-label">{label}</span>}
                  </For>
                  <Show when={m.has_attachments}><span class="mail-label">paperclip</span></Show>
                </span>
                <span class="mail-row-meta">
                  <AccountLabel id={m.account_id} accounts={accts()} />
                </span>
              </button>
            )}
          </For>
        </div>
      </aside>

      <main class="mail-main">
        <Show
          when={thread()}
          fallback={
            <div class="mail-empty">
              <Icon name="mail" size={30} />
              <h3>Select a message</h3>
              <p class="dim">Read-only archive. Plaintext only; raw HTML is never rendered.</p>
            </div>
          }
        >
          {(t) => (
            <>
              <header class="mail-head">
                <div>
                  <h3>{t()?.subject || current()?.subject || "(no subject)"}</h3>
                  <p class="dim sm">thread {t()?.thread_id}</p>
                </div>
                <span class="kind-chip mail-chip"><Icon name="mail" size={12} /> mail</span>
              </header>
              <div class="mail-thread">
                <For each={t()?.messages ?? []} fallback={<p class="dim sm">No plaintext messages in this thread.</p>}>
                  {(m) => <MailBody message={m} />}
                </For>
              </div>
            </>
          )}
        </Show>
      </main>
    </div>
  );
};
