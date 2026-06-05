import { createResource, createSignal, For, Show, type Component } from "solid-js";
import type {
  AnchorKind,
  Decision,
  EventItem,
  Phase,
  Person,
  Project,
  SearchHit,
  Task,
  TaskStatus,
  Topic,
  WireEvent,
} from "@hive/shared";
import { TASK_STATUSES } from "@hive/shared";
import { api, getDoneRetentionHours, setDoneRetentionHours } from "./api.ts";
import { liveRev } from "./live.ts";
import { Icon } from "./icons.tsx";
import { DECISION_GLYPH, relTime, TASK_GLYPH } from "./lib.tsx";
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
      <For each={decisions()}>
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
      <For each={events()}>
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
      <For each={hits()}>
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

// ---- Wire ----

export const Wire: Component = () => {
  const [events] = createResource(() => ({ _r: liveRev() }), () => api.wire());
  return (
    <section class="wire">
      <For each={events() as WireEvent[]}>
        {(e) => (
          <div class="wire-row">
            <time>{relTime(e.created_at)}</time>
            <span class="actor-chip sm">{e.actor}</span>
            <code>{e.kind}</code>
          </div>
        )}
      </For>
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
      <Show when={people()} fallback={<p class="dim sm">loading…</p>}>
        <For each={people() as Person[]} fallback={<p class="dim sm">no people yet.</p>}>
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

export const TopicsView: Component = () => {
  const [topics] = createResource(() => ({ _r: liveRev() }), () => api.topics());
  return (
    <section>
      <p class="dim pad">Topics extracted from <code>[topic:…]</code> references in journal entries.</p>
      <Show when={topics()} fallback={<p class="dim sm">loading…</p>}>
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
      <Show when={projects()} fallback={<p class="dim sm">loading…</p>}>
        <For each={projects() as Project[]} fallback={<p class="dim sm">no projects yet — assign a task a project in a journal entry.</p>}>
          {(p) => <ProjectCard p={p} />}
        </For>
      </Show>
    </section>
  );
};
