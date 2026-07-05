import {
  createEffect,
  createMemo,
  createResource,
  createSignal,
  For,
  onCleanup,
  Show,
  Suspense,
  type Component,
} from "solid-js";
import type {
  AnchorKind,
  Decision,
  EventItem,
  Phase,
  Person,
  Project,
  SearchHit,
  Severity,
  Task,
  TaskStatus,
  Topic,
  WireEvent,
} from "@hive/shared";
import { TASK_STATUSES } from "@hive/shared";
import { A, useSearchParams } from "@solidjs/router";
import { api, getDoneRetentionHours, setDoneRetentionHours } from "./api.ts";
import { liveRev } from "./live.ts";
import { Icon } from "./icons.tsx";
import { DECISION_GLYPH, highlightSnippet, relTime, SkeletonList, TASK_GLYPH } from "./lib.tsx";
import { Markdown } from "./markdown.tsx";
import { EmptyState } from "./primitives.tsx";
import { KIND } from "./kinds.ts";
import { EntityList, type EntityListConfig } from "./EntityList.tsx";

// ---- due-date helpers ----

/** True when a task is overdue: has a due date, it's in the past, and not done. */
function isOverdue(t: Task): boolean {
  return !!t.due && t.status !== "done" && new Date(t.due).getTime() < Date.now();
}

/** Format an ISO due date as a short locale string. */
function fmtDue(iso: string): string {
  return new Date(iso).toLocaleDateString(undefined, { month: "short", day: "numeric" });
}

/** Renders whichever structured entity an anchor points at. */
export const EntityCard: Component<{
  kind: AnchorKind;
  entity: Task | Decision | EventItem | null;
}> = (props) => {
  return (
    <Show when={props.entity} fallback={<p class="dim">entity not found</p>}>
      <Show when={props.kind === "task"}>
        <TaskBody t={props.entity as Task} />
      </Show>
      <Show when={props.kind === "decision"}>
        <DecisionBody d={props.entity as Decision} />
      </Show>
      <Show when={props.kind === "event"}>
        <EventBody e={props.entity as EventItem} />
      </Show>
    </Show>
  );
};

const Assignees: Component<{ who: string[] }> = (props) => (
  <Show when={props.who.length}>
    <div class="assignees">
      <For each={props.who}>{(a) => <span class="actor-chip sm">{a}</span>}</For>
    </div>
  </Show>
);

const TaskBody: Component<{ t: Task }> = (props) => (
  <div>
    <h3>
      {TASK_GLYPH[props.t.status]} {props.t.title}
    </h3>
    <div class="meta">
      <span class="badge">{props.t.status}</span>
      <span class={`pri pri-${props.t.priority}`}>{props.t.priority}</span>
      <Show when={props.t.due}>
        <span class={isOverdue(props.t) ? "due-badge overdue" : "due-badge"}>
          {isOverdue(props.t) ? "overdue" : fmtDue(props.t.due!)}
        </span>
      </Show>
    </div>
    <Assignees who={props.t.assignees} />
  </div>
);

/** The structured ADR prose (context / decision / consequences) — shared by
 * the drawer body below and the Decisions board rows. */
const DecisionFields: Component<{ item: Decision }> = (props) => (
  <>
    <Show when={props.item.context}>
      <div class="field">
        <strong>Context.</strong>
        <Markdown src={props.item.context} />
      </div>
    </Show>
    <div class="field">
      <strong>Decision.</strong>
      <Markdown src={props.item.decision} />
    </div>
    <Show when={props.item.consequences}>
      <div class="field">
        <strong>Consequences.</strong>
        <Markdown src={props.item.consequences} />
      </div>
    </Show>
  </>
);

const DecisionBody: Component<{ d: Decision }> = (props) => (
  <div>
    <h3>
      {DECISION_GLYPH[props.d.status]} {props.d.title}
    </h3>
    <span class="badge">{props.d.status}</span>
    <DecisionFields item={props.d} />
  </div>
);

const EventBody: Component<{ e: EventItem }> = (props) => (
  <div>
    <h3>◷ {props.e.title}</h3>
    <Show when={props.e.at}>
      <span class="badge">{props.e.at}</span>
    </Show>
    <Assignees who={props.e.assignees} />
  </div>
);

// ---- Tasks board ----

// Retention options: label → hours (Infinity = never hide)
const RETENTION_OPTIONS: { label: string; hours: number }[] = [
  { label: "1h", hours: 1 },
  { label: "8h", hours: 8 },
  { label: "24h", hours: 24 },
  { label: "7d", hours: 168 },
  { label: "always", hours: Infinity },
];

type GroupBy = "status" | "assignee";

// Sentinel column key for tasks with no assignee, in assignee grouping.
const UNASSIGNED = "__unassigned__";

export const Tasks: Component = () => {
  const [tasks, { refetch, mutate }] = createResource(() => ({ _r: liveRev() }), () => api.tasks());
  const [projects] = createResource(() => ({ _r: liveRev() }), () => api.projects());
  const [showDone, setShowDone] = createSignal(false);
  const [retentionHours, setRetentionHoursState] = createSignal(getDoneRetentionHours());
  const [groupBy, setGroupBy] = createSignal<GroupBy>("status");
  const [projectFilter, setProjectFilter] = createSignal<string>(""); // "" = all
  const [dragId, setDragId] = createSignal<string | null>(null);
  const [overCol, setOverCol] = createSignal<string | null>(null);

  const cycle = async (t: Task) => {
    const next = TASK_STATUSES[(TASK_STATUSES.indexOf(t.status) + 1) % TASK_STATUSES.length];
    await api.patchTask(t.id, { status: next });
    refetch();
  };

  const changeRetention = (hours: number) => {
    setDoneRetentionHours(hours);
    setRetentionHoursState(hours);
  };

  /** Map a project id → its display name, for the cards' project chip. */
  const projectName = (id: string | null): string | undefined =>
    id ? (projects() ?? []).find((p) => p.id === id)?.name ?? id : undefined;

  /** Tasks after the project filter + the done-retention rule, before grouping. */
  const filtered = createMemo<Task[]>(() => {
    const all = tasks() ?? [];
    const proj = projectFilter();
    const retention = retentionHours();
    const cutoff = Date.now() - retention * 3_600_000;
    return all.filter((t) => {
      if (proj && t.project !== proj) return false;
      if (t.status !== "done") return true;
      if (showDone()) return true; // override: show all done
      if (!Number.isFinite(retention)) return true; // "always" setting
      return new Date(t.updated_at).getTime() >= cutoff;
    });
  });

  // Columns depend on the grouping. Status grouping is the fixed four buckets;
  // assignee grouping derives its columns from whoever appears in the filtered
  // set (plus an "unassigned" bucket when any task has no assignee).
  const columns = createMemo<{ key: string; label: string; glyph?: string }[]>(() => {
    if (groupBy() === "status") {
      return TASK_STATUSES.map((s) => ({ key: s, label: s, glyph: TASK_GLYPH[s] }));
    }
    const who = new Set<string>();
    let anyUnassigned = false;
    for (const t of filtered()) {
      if (t.assignees.length === 0) anyUnassigned = true;
      else for (const a of t.assignees) who.add(a);
    }
    const cols = [...who].sort().map((a) => ({ key: a, label: a }));
    if (anyUnassigned) cols.push({ key: UNASSIGNED, label: "unassigned" });
    return cols.length ? cols : [{ key: UNASSIGNED, label: "unassigned" }];
  });

  /** Tasks that belong in a given column under the active grouping. */
  const tasksFor = (key: string): Task[] => {
    if (groupBy() === "status") return filtered().filter((t) => t.status === key);
    if (key === UNASSIGNED) return filtered().filter((t) => t.assignees.length === 0);
    return filtered().filter((t) => t.assignees.includes(key));
  };

  // Dropping a card on a column re-statuses it (status grouping) or reassigns it
  // (assignee grouping). Optimistic: patch the resource locally first, then
  // persist; on failure, refetch to snap back to server truth.
  const drop = async (key: string) => {
    const id = dragId();
    setDragId(null);
    setOverCol(null);
    if (!id) return;
    const t = (tasks() ?? []).find((x) => x.id === id);
    if (!t) return;

    if (groupBy() === "status") {
      if (t.status === key) return;
      const status = key as TaskStatus;
      mutate((prev) => (prev ?? []).map((x) => (x.id === id ? { ...x, status } : x)));
      try {
        await api.patchTask(id, { status });
      } finally {
        refetch();
      }
    } else {
      // Reassign: a single-owner column means the dropped task becomes owned by
      // that one person (UNASSIGNED clears assignees). No-op if already there.
      const next = key === UNASSIGNED ? [] : [key];
      const same =
        t.assignees.length === next.length && t.assignees.every((a, i) => a === next[i]);
      if (same) return;
      mutate((prev) => (prev ?? []).map((x) => (x.id === id ? { ...x, assignees: next } : x)));
      try {
        await api.patchTask(id, { assignees: next });
      } finally {
        refetch();
      }
    }
  };

  return (
    <section>
      <div class="tasks-header">
        <p class="dim pad">
          Tasks emerge from journal entries. Click a card to advance its status, or drag it between
          columns.
        </p>
        <div class="tasks-controls">
          <div class="seg" role="group" aria-label="group tasks by">
            <button classList={{ active: groupBy() === "status" }} onClick={() => setGroupBy("status")}>
              by status
            </button>
            <button classList={{ active: groupBy() === "assignee" }} onClick={() => setGroupBy("assignee")}>
              by assignee
            </button>
          </div>
          <label class="task-project-filter">
            <span class="dim sm">project</span>
            <select value={projectFilter()} onChange={(e) => setProjectFilter(e.currentTarget.value)}>
              <option value="">all projects</option>
              <For each={projects() ?? []}>
                {(p) => <option value={p.id}>{p.name}</option>}
              </For>
            </select>
          </label>
          <label class="show-done-toggle">
            <input
              type="checkbox"
              checked={showDone()}
              onChange={(e) => setShowDone(e.currentTarget.checked)}
            />
            {" show done"}
          </label>
          <span class="dim sm">hide done after:</span>
          <div class="seg">
            <For each={RETENTION_OPTIONS}>
              {(opt) => (
                <button
                  classList={{ active: retentionHours() === opt.hours }}
                  onClick={() => changeRetention(opt.hours)}
                  title={`Hide done tasks after ${opt.label}`}
                >
                  {opt.label}
                </button>
              )}
            </For>
          </div>
        </div>
      </div>
      <div class="board" classList={{ "board-assignee": groupBy() === "assignee" }}>
        <For each={columns()}>
          {(col) => (
            <div
              class="col"
              classList={{ "col-drop": overCol() === col.key }}
              onDragOver={(e) => {
                e.preventDefault();
                if (overCol() !== col.key) setOverCol(col.key);
              }}
              onDragLeave={(e) => {
                // Only clear when actually leaving the column, not crossing a child.
                if (!e.currentTarget.contains(e.relatedTarget as Node)) setOverCol(null);
              }}
              onDrop={(e) => {
                e.preventDefault();
                void drop(col.key);
              }}
            >
              <h3>
                {col.glyph ? `${col.glyph} ` : ""}
                {col.label}
              </h3>
              <For each={tasksFor(col.key)}>
                {(t) => (
                  <div
                    class="card"
                    classList={{ dragging: dragId() === t.id }}
                    draggable={true}
                    onDragStart={(e) => {
                      setDragId(t.id);
                      e.dataTransfer?.setData("text/plain", t.id);
                      if (e.dataTransfer) e.dataTransfer.effectAllowed = "move";
                    }}
                    onDragEnd={() => {
                      setDragId(null);
                      setOverCol(null);
                    }}
                    onClick={() => cycle(t)}
                  >
                    <div class="card-meta-row">
                      <span class={`pri pri-${t.priority}`}>{t.priority}</span>
                      <Show when={t.due}>
                        <span class={isOverdue(t) ? "due-badge overdue" : "due-badge dim"}>
                          {isOverdue(t) ? "overdue" : fmtDue(t.due!)}
                        </span>
                      </Show>
                    </div>
                    <div class="card-title">{t.title}</div>
                    {/* In assignee grouping the column already says who owns it,
                        so surface status on the card instead; in status grouping
                        show the project for context. */}
                    <Show
                      when={groupBy() === "assignee"}
                      fallback={
                        <Show when={projectName(t.project)}>
                          <span class="badge task-project-badge">{projectName(t.project)}</span>
                        </Show>
                      }
                    >
                      <span class="badge">{TASK_GLYPH[t.status]} {t.status}</span>
                    </Show>
                    <Assignees who={t.assignees} />
                  </div>
                )}
              </For>
              <Show when={tasksFor(col.key).length === 0}>
                <p class="dim sm col-empty">drop here</p>
              </Show>
            </div>
          )}
        </For>
      </div>
    </section>
  );
};

// ---- Decisions ----

const decisionsConfig: EntityListConfig<Decision> = {
  kind: KIND.decision,
  fetch: () => api.decisions(),
  row: {
    title: (d) => d.title,
    // Status rides a badge (accepted glows honey, the rest stay dim) instead
    // of the old per-status card classes.
    badges: (d) => [{ label: d.status, tone: d.status === "accepted" ? "honey" : "dim" }],
    body: DecisionFields,
    originQuote: (d) => d.anchor_text ?? null,
  },
};

export const Decisions: Component = () => <EntityList config={decisionsConfig} />;

// ---- Events ----

const eventsConfig: EntityListConfig<EventItem> = {
  kind: KIND.event,
  fetch: () => api.events(),
  row: {
    title: (e) => e.title,
    badges: (e) => (e.at ? [{ label: e.at }] : []),
    metas: (e) => e.assignees,
    originQuote: (e) => e.anchor_text ?? null,
  },
};

export const Events: Component = () => <EntityList config={eventsConfig} />;

// ---- Search ----

// The results list is its own component reading its own resource, wrapped in a
// local Suspense below. That keeps a refetch's pending state contained here —
// reading a suspending resource at the page level would otherwise bubble to the
// app-shell Suspense and flash the whole UI on every query change.
const SearchResults: Component<{ query: string; mode: "keyword" | "semantic" }> = (props) => {
  const [hits] = createResource(
    () => ({ q: props.query, mode: props.mode }),
    (k) => (k.q.trim() ? api.search(k.q, k.mode) : Promise.resolve([] as SearchHit[])),
  );
  return (
    <For
      each={hits()}
      fallback={
        <Show when={props.query.trim()}>
          <EmptyState icon="search" title="No matches." hint="Try different words, or switch modes." />
        </Show>
      }
    >
      {(h) => (
        <div class="hit">
          <ResultKindChip hit={h} />
          <Show when={h.kind === "mail"} fallback={<strong>{h.title}</strong>}>
            <A class="hit-title" href={`/mail?message=${encodeURIComponent(h.id)}`}><strong>{h.title}</strong></A>
          </Show>
          <Show when={h.snippet} fallback={<span class="snippet">score {h.score}</span>}>
            {/* escape-then-mark: body text is untrusted, only our <mark>s render */}
            <span class="snippet" innerHTML={highlightSnippet(h.snippet)} />
          </Show>
        </div>
      )}
    </For>
  );
};

const ResultKindChip: Component<{ hit: SearchHit }> = (props) => {
  const k = () => KIND[props.hit.kind];
  return (
    <Show when={k()} fallback={<span class="badge">{props.hit.kind}</span>}>
      {(kind) => (
        <span class="kind-chip" classList={{ "mail-chip": props.hit.kind === "mail" }} title={kind().label}>
          <Icon name={kind().icon} size={12} /> {kind().label}
        </span>
      )}
    </Show>
  );
};

export const SearchPane: Component = () => {
  const [params] = useSearchParams();
  // Seed from ?q= so the command palette's free-text passthrough (and any
  // deep link) lands with the query already running.
  const initial = typeof params.q === "string" ? params.q : "";
  const [q, setQ] = createSignal(initial); // live input value
  const [query, setQuery] = createSignal(initial); // debounced value that drives the search
  const [mode, setMode] = createSignal<"keyword" | "semantic">("keyword");

  // Palette navigations while the pane is already mounted update ?q= in place.
  createEffect(() => {
    const next = typeof params.q === "string" ? params.q : "";
    if (next && next !== q()) {
      setQ(next);
      setQuery(next);
    }
  });

  // Debounce the live input into `query` so a search fires at most ~250ms after
  // typing stops, not once per keystroke. The input stays fully responsive
  // because it's bound to `q`, which the resource never reads.
  let timer: ReturnType<typeof setTimeout> | undefined;
  const onType = (value: string) => {
    setQ(value);
    clearTimeout(timer);
    timer = setTimeout(() => setQuery(value), 250);
  };
  onCleanup(() => clearTimeout(timer));

  return (
    <section>
      <div class="row">
        <input
          placeholder="search journal, tasks, decisions, events…"
          value={q()}
          onInput={(e) => onType(e.currentTarget.value)}
        />
        <div class="seg">
          <button classList={{ active: mode() === "keyword" }} onClick={() => setMode("keyword")}>
            keyword
          </button>
          <button classList={{ active: mode() === "semantic" }} onClick={() => setMode("semantic")}>
            semantic
          </button>
        </div>
      </div>
      <p class="dim sm pad">
        {mode() === "semantic" ? "vector similarity via the local embedder" : "full-text keyword match"}
      </p>
      <Suspense fallback={<p class="dim sm pad">searching…</p>}>
        <SearchResults query={query()} mode={mode()} />
      </Suspense>
    </section>
  );
};

// ---- Wire (news + info feed) ----

// feed.item / scrape.item events carry a rich payload the worker ingests from
// RSS/scrape sources. Everything else on the wire is internal activity.
type NewsPayload = {
  title?: string;
  url?: string | null;
  body?: string;
  source?: string;
  category?: string | null;
  severity?: Severity;
};
const NEWS_KINDS = new Set(["feed.item", "scrape.item"]);
const isNews = (e: WireEvent): boolean => NEWS_KINDS.has(e.kind);
const newsPayload = (e: WireEvent): NewsPayload => (e.payload ?? {}) as NewsPayload;

type WireFilter = "news" | "activity" | "all";

const NewsCard: Component<{ e: WireEvent; fresh: boolean }> = (props) => {
  const p = () => newsPayload(props.e);
  const sev = () => p().severity ?? "info";
  return (
    <article class="news-card" classList={{ "just-landed": props.fresh }}>
      <span class={`sev-dot sev-${sev()}`} title={sev()} />
      <div class="news-body">
        <div class="news-head">
          <Show
            when={p().url}
            fallback={<span class="news-title">{p().title ?? props.e.kind}</span>}
          >
            <a class="news-title" href={p().url!} target="_blank" rel="noopener noreferrer">
              {p().title ?? p().url}
            </a>
          </Show>
        </div>
        <Show when={p().body}>
          <p class="news-snippet">{(p().body ?? "").slice(0, 240)}</p>
        </Show>
        <div class="news-meta">
          <span class="actor-chip sm">{p().source ?? props.e.actor}</span>
          <Show when={p().category}>
            <span class="badge news-cat">{p().category}</span>
          </Show>
          <Show when={sev() !== "info"}>
            <span class={`badge sev-badge sev-${sev()}`}>{sev()}</span>
          </Show>
          <time class="dim sm">{relTime(props.e.created_at)}</time>
        </div>
      </div>
    </article>
  );
};

export const Wire: Component = () => {
  const [events, { refetch }] = createResource(() => ({ _r: liveRev() }), () => api.wire());
  const [filter, setFilter] = createSignal<WireFilter>("news");
  const [refreshing, setRefreshing] = createSignal(false);
  const [lastRefresh, setLastRefresh] = createSignal<string>(new Date().toISOString());

  // Track which event ids we've already shown so newly-arrived ones (pushed over
  // the live SSE stream, or pulled by a manual refresh) get a brief honey
  // "just-landed" wash. The first load isn't flagged — only events that appear
  // after we've seen a baseline.
  let seen: Set<string> | null = null;
  const isFresh = (e: WireEvent): boolean => seen !== null && !seen.has(e.id);
  createEffect(() => {
    const list = (events() as WireEvent[]) ?? [];
    seen = new Set(list.map((e) => e.id));
  });

  const shown = (): WireEvent[] => {
    const all = (events() as WireEvent[]) ?? [];
    const f = filter();
    if (f === "all") return all;
    if (f === "news") return all.filter(isNews);
    return all.filter((e) => !isNews(e));
  };

  // "Refresh now": always refetch the wire immediately (real today). Also ask
  // the backend to poll sources for genuinely new items — that endpoint may
  // not exist yet, so swallow its failure and keep the instant refetch.
  const refreshNow = async () => {
    if (refreshing()) return;
    setRefreshing(true);
    try {
      await api.pollSources().catch(() => undefined);
      await refetch();
      setLastRefresh(new Date().toISOString());
    } finally {
      setRefreshing(false);
    }
  };

  return (
    <section class="wire">
      <div class="wire-bar">
        <div class="wire-live">
          <span class="live-dot" />
          <span class="dim sm">live · updated {relTime(lastRefresh())}</span>
        </div>
        <div class="wire-controls">
          <div class="seg wire-filter">
            <button classList={{ active: filter() === "news" }} onClick={() => setFilter("news")}>news</button>
            <button classList={{ active: filter() === "activity" }} onClick={() => setFilter("activity")}>activity</button>
            <button classList={{ active: filter() === "all" }} onClick={() => setFilter("all")}>all</button>
          </div>
          <button class="primary wire-refresh" onClick={refreshNow} disabled={refreshing()}>
            <Show when={refreshing()} fallback={<>↻ refresh now</>}>
              <span class="spinner" /> refreshing…
            </Show>
          </button>
        </div>
      </div>

      <Show when={filter() === "news"}>
        <For
          each={shown()}
          fallback={
            <EmptyState
              icon="wire"
              title="No news yet."
              hint="Add an RSS or scrape source in Settings; items land here as the worker polls."
            />
          }
        >
          {(e) => <NewsCard e={e} fresh={isFresh(e)} />}
        </For>
      </Show>
      <Show when={filter() !== "news"}>
        <For
          each={shown()}
          fallback={
            <EmptyState
              icon="wire"
              title="No activity yet."
              hint="Live events appear here as you and the agents work."
            />
          }
        >
          {(e) => (
            <div class="wire-row" classList={{ "just-landed": isFresh(e) }}>
              <time>{relTime(e.created_at)}</time>
              <span class="actor-chip sm">{e.actor}</span>
              <code>{e.kind}</code>
            </div>
          )}
        </For>
      </Show>
    </section>
  );
};

// ---- People view ----

export const PeopleView: Component = () => {
  const [people, { refetch }] = createResource(() => ({ _r: liveRev() }), () => api.people());
  const [editing, setEditing] = createSignal<string | null>(null);
  const [bio, setBio] = createSignal("");
  const [role, setRole] = createSignal("");
  const open = (p: Person) => {
    setEditing(p.slug);
    setBio(p.bio ?? "");
    setRole(p.role ?? "");
  };
  const save = async (slug: string) => {
    await api.patchPerson(slug, { bio: bio(), role: role() });
    setEditing(null);
    refetch();
  };
  return (
    <section>
      <p class="dim pad">Identities known to hive — humans and AIs. Click one to edit its profile (bio + role). AIs can also keep their own profile updated via MCP.</p>
      <Show when={people()} fallback={<SkeletonList rows={6} />}>
        <For
          each={people() as Person[]}
          fallback={
            <EmptyState
              icon="people"
              title="No people yet."
              hint="Mention someone in a journal entry to introduce them."
            />
          }
        >
          {(p) => (
            <div class="person-card">
              <div class="entity-row person-head" onClick={() => (editing() === p.slug ? setEditing(null) : open(p))}>
                <span class="entity-icon"><Icon name="person" size={16} /></span>
                <span class="entity-name">{p.name}</span>
                <Show when={p.role}><span class="badge">{p.role}</span></Show>
                <span class={`badge kind-badge-${p.kind}`}>{p.kind}</span>
                <span class="dim sm entity-slug">{p.slug}</span>
              </div>
              <Show when={p.bio && editing() !== p.slug}>
                <p class="dim sm person-bio">{p.bio}</p>
              </Show>
              <Show when={editing() === p.slug}>
                <div class="person-edit">
                  <input placeholder="role (e.g. VP of Technology)" value={role()} onInput={(e) => setRole(e.currentTarget.value)} />
                  <textarea placeholder="bio — who they are / what they do" rows="3" value={bio()} onInput={(e) => setBio(e.currentTarget.value)} />
                  <div class="consent-actions">
                    <button class="logout" onClick={() => setEditing(null)}>Cancel</button>
                    <button type="button" onClick={() => save(p.slug)}>Save</button>
                  </div>
                </div>
              </Show>
            </div>
          )}
        </For>
      </Show>
    </section>
  );
};

// ---- Topics view ----

const topicsConfig: EntityListConfig<Topic> = {
  kind: KIND.topic,
  fetch: () => api.topics(),
  intro: "Topics extracted from [topic:…] references in journal entries.",
  row: {
    title: (t) => t.name,
    metas: (t) => [t.slug],
  },
};

export const TopicsView: Component = () => <EntityList config={topicsConfig} />;

// ---- Projects view ----

/** Phases + task-count summary for one project row. Each row keeps its own
 * projectById sub-resource, exactly as the old ProjectCard did. */
const ProjectBody: Component<{ item: Project }> = (props) => {
  const [detail] = createResource(() => props.item.id, (id) => api.projectById(id));
  return (
    <Show when={detail()}>
      {(d) => (
        <>
          {/* Phases */}
          <Show when={d().phases.length > 0}>
            <div class="phases">
              <For each={d().phases}>
                {(ph: Phase) => (
                  <span class="phase-chip">
                    <Icon name="phase" size={13} />
                    {ph.name}
                  </span>
                )}
              </For>
            </div>
          </Show>

          {/* Task summary */}
          <Show when={d().tasks.length > 0}>
            <div class="project-tasks dim sm">
              {d().tasks.filter((t: Task) => t.status !== "done").length} open ·{" "}
              {d().tasks.filter((t: Task) => t.status === "done").length} done
              <span class="dim"> ({d().tasks.length} total)</span>
            </div>
          </Show>
        </>
      )}
    </Show>
  );
};

const projectsConfig: EntityListConfig<Project> = {
  kind: KIND.project,
  fetch: () => api.projects(),
  intro: "Projects with their phases and task counts. Projects are created automatically when a task references one.",
  row: {
    title: (p) => p.name,
    metas: (p) => [p.slug],
    body: ProjectBody,
  },
};

export const ProjectsView: Component = () => <EntityList config={projectsConfig} />;
