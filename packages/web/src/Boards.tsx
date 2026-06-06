import { createEffect, createResource, createSignal, For, Show, type Component } from "solid-js";
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
import { api, getDoneRetentionHours, setDoneRetentionHours } from "./api.ts";
import { liveRev } from "./live.ts";
import { Icon } from "./icons.tsx";
import { DECISION_GLYPH, relTime, SkeletonList, TASK_GLYPH } from "./lib.tsx";
import { Markdown } from "./markdown.tsx";

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

const DecisionBody: Component<{ d: Decision }> = (props) => (
  <div>
    <h3>
      {DECISION_GLYPH[props.d.status]} {props.d.title}
    </h3>
    <span class="badge">{props.d.status}</span>
    <Show when={props.d.context}>
      <div class="field">
        <strong>Context.</strong>
        <Markdown src={props.d.context} />
      </div>
    </Show>
    <div class="field">
      <strong>Decision.</strong>
      <Markdown src={props.d.decision} />
    </div>
    <Show when={props.d.consequences}>
      <div class="field">
        <strong>Consequences.</strong>
        <Markdown src={props.d.consequences} />
      </div>
    </Show>
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

export const Tasks: Component = () => {
  const [tasks, { refetch }] = createResource(() => ({ _r: liveRev() }), () => api.tasks());
  const [showDone, setShowDone] = createSignal(false);
  const [retentionHours, setRetentionHoursState] = createSignal(getDoneRetentionHours());

  const cycle = async (t: Task) => {
    const next = TASK_STATUSES[(TASK_STATUSES.indexOf(t.status) + 1) % TASK_STATUSES.length];
    await api.patchTask(t.id, { status: next });
    refetch();
  };

  const changeRetention = (hours: number) => {
    setDoneRetentionHours(hours);
    setRetentionHoursState(hours);
  };

  /** Filter tasks for a given status column, applying done-retention for the "done" column. */
  const visibleTasks = (status: TaskStatus): Task[] => {
    const all = tasks() ?? [];
    if (status !== "done") return all.filter((t) => t.status === status);
    const retention = retentionHours();
    return all.filter((t) => {
      if (t.status !== "done") return false;
      if (showDone()) return true; // override: show all
      if (!Number.isFinite(retention)) return true; // "always" setting
      const cutoff = Date.now() - retention * 3_600_000;
      return new Date(t.updated_at).getTime() >= cutoff;
    });
  };

  return (
    <section>
      <div class="tasks-header">
        <p class="dim pad">Tasks emerge from journal entries. Click a card to advance its status.</p>
        <div class="tasks-controls">
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
      <div class="board">
        <For each={TASK_STATUSES}>
          {(status) => (
            <div class="col">
              <h3>
                {TASK_GLYPH[status]} {status}
              </h3>
              <For each={visibleTasks(status)}>
                {(t) => (
                  <div class="card" onClick={() => cycle(t)}>
                    <div class="card-meta-row">
                      <span class={`pri pri-${t.priority}`}>{t.priority}</span>
                      <Show when={t.due}>
                        <span class={isOverdue(t) ? "due-badge overdue" : "due-badge dim"}>
                          {isOverdue(t) ? "overdue" : fmtDue(t.due!)}
                        </span>
                      </Show>
                    </div>
                    <div class="card-title">{t.title}</div>
                    <Assignees who={t.assignees} />
                  </div>
                )}
              </For>
            </div>
          )}
        </For>
      </div>
    </section>
  );
};

// ---- Decisions ----

export const Decisions: Component = () => {
  const [decisions] = createResource(() => ({ _r: liveRev() }), () => api.decisions());
  return (
    <section>
      <For each={decisions()} fallback={<p class="dim sm pad">no decisions yet — a decision emerges when you anchor one in a journal entry.</p>}>
        {(d) => (
          <article class={`decision status-${d.status}`}>
            <header>
              <span class="glyph">{DECISION_GLYPH[d.status]}</span>
              <h3>{d.title}</h3>
              <span class="badge">{d.status}</span>
            </header>
            <Show when={d.context}>
              <div class="field">
                <strong>Context.</strong>
                <Markdown src={d.context} />
              </div>
            </Show>
            <div class="field">
              <strong>Decision.</strong>
              <Markdown src={d.decision} />
            </div>
            <Show when={d.consequences}>
              <div class="field">
                <strong>Consequences.</strong>
                <Markdown src={d.consequences} />
              </div>
            </Show>
            <Show when={d.anchor_text}>
              <blockquote class="origin">from journal: "{d.anchor_text}"</blockquote>
            </Show>
          </article>
        )}
      </For>
    </section>
  );
};

// ---- Events ----

export const Events: Component = () => {
  const [events] = createResource(() => ({ _r: liveRev() }), () => api.events());
  return (
    <section>
      <For each={events()} fallback={<p class="dim sm pad">no events yet — an event emerges when you anchor one in a journal entry.</p>}>
        {(e) => (
          <article class="entry">
            <h3>◷ {e.title}</h3>
            <Show when={e.at}>
              <span class="badge">{e.at}</span>
            </Show>
            <Assignees who={e.assignees} />
            <Show when={e.anchor_text}>
              <blockquote class="origin">from journal: "{e.anchor_text}"</blockquote>
            </Show>
          </article>
        )}
      </For>
    </section>
  );
};

// ---- Search ----

export const SearchPane: Component = () => {
  const [q, setQ] = createSignal("");
  const [mode, setMode] = createSignal<"keyword" | "semantic">("keyword");
  const [hits] = createResource(
    () => ({ q: q(), mode: mode() }),
    (k) => (k.q.trim() ? api.search(k.q, k.mode) : Promise.resolve([] as SearchHit[])),
  );
  return (
    <section>
      <div class="row">
        <input
          placeholder="search journal, tasks, decisions, events…"
          value={q()}
          onInput={(e) => setQ(e.currentTarget.value)}
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
        {mode() === "semantic" ? "vector similarity via the local embedder" : "FTS5 keyword match"}
      </p>
      <For each={hits()} fallback={<Show when={q().trim()}><p class="dim sm pad">no matches.</p></Show>}>
        {(h) => (
          <div class="hit">
            <span class="badge">{h.kind}</span>
            <strong>{h.title}</strong>
            <Show when={h.snippet} fallback={<span class="snippet">score {h.score}</span>}>
              <span class="snippet" innerHTML={h.snippet} />
            </Show>
          </div>
        )}
      </For>
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
        <For each={shown()} fallback={<p class="dim sm pad">no news yet — add an RSS or scrape source in Settings; items land here as the worker polls them.</p>}>
          {(e) => <NewsCard e={e} fresh={isFresh(e)} />}
        </For>
      </Show>
      <Show when={filter() !== "news"}>
        <For each={shown()} fallback={<p class="dim sm pad">no activity yet — the wire shows live events as you and the agents work.</p>}>
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
  const [people] = createResource(() => ({ _r: liveRev() }), () => api.people());
  return (
    <section>
      <p class="dim pad">People known to hive. Created automatically when referenced in journal entries, or added from Admin.</p>
      <Show when={people()} fallback={<SkeletonList rows={6} />}>
        <For each={people() as Person[]} fallback={<p class="dim sm">no people yet — reference someone in a journal entry.</p>}>
          {(p) => (
            <div class="entity-row">
              <span class="entity-icon"><Icon name="person" size={16} /></span>
              <span class="entity-name">{p.name}</span>
              <span class={`badge kind-badge-${p.kind}`}>{p.kind}</span>
              <span class="dim sm entity-slug">{p.slug}</span>
            </div>
          )}
        </For>
      </Show>
    </section>
  );
};

// ---- Topics view ----

export const TopicsView: Component = () => {
  const [topics] = createResource(() => ({ _r: liveRev() }), () => api.topics());
  return (
    <section>
      <p class="dim pad">Topics extracted from <code>[topic:…]</code> references in journal entries.</p>
      <Show when={topics()} fallback={<SkeletonList rows={6} />}>
        <For each={topics() as Topic[]} fallback={<p class="dim sm">no topics yet — reference a topic in a journal entry.</p>}>
          {(t) => (
            <div class="entity-row">
              <span class="entity-icon"><Icon name="topic" size={16} /></span>
              <span class="entity-name">{t.name}</span>
              <span class="dim sm entity-slug">{t.slug}</span>
            </div>
          )}
        </For>
      </Show>
    </section>
  );
};

// ---- Projects view ----

const ProjectCard: Component<{ p: Project }> = (props) => {
  const [detail] = createResource(() => api.projectById(props.p.id));
  return (
    <article class="project-card">
      <header class="project-header">
        <span class="entity-icon"><Icon name="project" size={16} /></span>
        <h3 class="project-name">{props.p.name}</h3>
        <span class="dim sm project-slug">{props.p.slug}</span>
      </header>

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
    </article>
  );
};

export const ProjectsView: Component = () => {
  const [projects] = createResource(() => ({ _r: liveRev() }), () => api.projects());
  return (
    <section>
      <p class="dim pad">Projects with their phases and task counts. Projects are created automatically when a task references one.</p>
      <Show when={projects()} fallback={<SkeletonList rows={4} />}>
        <For each={projects() as Project[]} fallback={<p class="dim sm">no projects yet — assign a task a project in a journal entry.</p>}>
          {(p) => <ProjectCard p={p} />}
        </For>
      </Show>
    </section>
  );
};
