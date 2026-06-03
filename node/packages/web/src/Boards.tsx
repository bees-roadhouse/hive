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
import { api } from "./api.ts";
import { Icon } from "./icons.tsx";
import { DECISION_GLYPH, relTime, TASK_GLYPH } from "./lib.tsx";

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
      <p><strong>Context.</strong> {props.d.context}</p>
    </Show>
    <p><strong>Decision.</strong> {props.d.decision}</p>
    <Show when={props.d.consequences}>
      <p><strong>Consequences.</strong> {props.d.consequences}</p>
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

export const Tasks: Component = () => {
  const [tasks, { refetch }] = createResource(() => api.tasks());
  const cycle = async (t: Task) => {
    const next = TASK_STATUSES[(TASK_STATUSES.indexOf(t.status) + 1) % TASK_STATUSES.length];
    await api.patchTask(t.id, { status: next });
    refetch();
  };
  return (
    <section>
      <p class="dim pad">Tasks emerge from journal entries. Click a card to advance its status.</p>
      <div class="board">
        <For each={TASK_STATUSES}>
          {(status) => (
            <div class="col">
              <h3>
                {TASK_GLYPH[status]} {status}
              </h3>
              <For each={tasks()?.filter((t) => t.status === status)}>
                {(t) => (
                  <div class="card" onClick={() => cycle(t)}>
                    <span class={`pri pri-${t.priority}`}>{t.priority}</span>
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
  const [decisions] = createResource(() => api.decisions());
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
              <p><strong>Context.</strong> {d.context}</p>
            </Show>
            <p><strong>Decision.</strong> {d.decision}</p>
            <Show when={d.consequences}>
              <p><strong>Consequences.</strong> {d.consequences}</p>
            </Show>
            <Show when={d.anchor_text}>
              <blockquote class="origin">from journal: “{d.anchor_text}”</blockquote>
            </Show>
          </article>
        )}
      </For>
    </section>
  );
};

// ---- Events ----

export const Events: Component = () => {
  const [events] = createResource(() => api.events());
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
              <blockquote class="origin">from journal: “{e.anchor_text}”</blockquote>
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
  const [events] = createResource(() => api.wire());
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
  const [people] = createResource(() => api.people());
  return (
    <section>
      <p class="dim pad">People known to hive. Created automatically when referenced in journal entries, or added from Admin.</p>
      <Show when={people()} fallback={<p class="dim sm">loading…</p>}>
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
  const [topics] = createResource(() => api.topics());
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
  const [projects] = createResource(() => api.projects());
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
