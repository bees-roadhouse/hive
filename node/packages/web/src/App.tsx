import { createResource, createSignal, For, type Component, Show } from "solid-js";
import type {
  Decision,
  DecisionStatus,
  JournalEntry,
  Note,
  SearchHit,
  Task,
  TaskStatus,
  WireEvent,
} from "@hive/shared";
import { DECISION_STATUSES, PRIORITIES, TASK_STATUSES } from "@hive/shared";
import { api, getActor, setActor } from "./api.ts";

const TABS = ["tasks", "decisions", "notes", "journal", "search", "wire"] as const;
type Tab = (typeof TABS)[number];

const ACTORS = ["nate", "maggie", "pia", "apis", "cera"];

export const App: Component = () => {
  const [tab, setTab] = createSignal<Tab>("tasks");
  const [actor, setActorState] = createSignal(getActor());

  const onActor = (a: string) => {
    setActor(a);
    setActorState(a);
  };

  return (
    <div class="app">
      <header>
        <h1>🐝 hive</h1>
        <nav>
          <For each={TABS}>
            {(t) => (
              <button classList={{ active: tab() === t }} onClick={() => setTab(t)}>
                {t}
              </button>
            )}
          </For>
        </nav>
        <label class="actor">
          acting as
          <select value={actor()} onChange={(e) => onActor(e.currentTarget.value)}>
            <For each={ACTORS}>{(a) => <option value={a}>{a}</option>}</For>
          </select>
        </label>
      </header>

      <main>
        <Show when={tab() === "tasks"}>
          <Tasks />
        </Show>
        <Show when={tab() === "decisions"}>
          <Decisions />
        </Show>
        <Show when={tab() === "notes"}>
          <Notes />
        </Show>
        <Show when={tab() === "journal"}>
          <Journal />
        </Show>
        <Show when={tab() === "search"}>
          <SearchPane />
        </Show>
        <Show when={tab() === "wire"}>
          <Wire />
        </Show>
      </main>

      <footer>
        Node + Solid rewrite of <code>hive</code> · the shared brain for Bee's Roadhouse AIs
      </footer>
    </div>
  );
};

// ---- Tasks ----

const Tasks: Component = () => {
  const [tasks, { refetch }] = createResource(() => api.tasks());
  const [title, setTitle] = createSignal("");
  const [priority, setPriority] = createSignal("normal");

  const add = async (e: Event) => {
    e.preventDefault();
    if (!title().trim()) return;
    await api.addTask({ title: title(), priority: priority() as any, project: "hive" });
    setTitle("");
    refetch();
  };

  const cycle = async (t: Task) => {
    const order: TaskStatus[] = TASK_STATUSES;
    const next = order[(order.indexOf(t.status) + 1) % order.length];
    await api.patchTask(t.id, { status: next });
    refetch();
  };

  return (
    <section>
      <form class="row" onSubmit={add}>
        <input placeholder="new task…" value={title()} onInput={(e) => setTitle(e.currentTarget.value)} />
        <select value={priority()} onChange={(e) => setPriority(e.currentTarget.value)}>
          <For each={PRIORITIES}>{(p) => <option value={p}>{p}</option>}</For>
        </select>
        <button type="submit">add</button>
      </form>

      <div class="board">
        <For each={TASK_STATUSES}>
          {(status) => (
            <div class="col">
              <h3>{status}</h3>
              <For each={tasks()?.filter((t) => t.status === status)}>
                {(t) => (
                  <div class="card" onClick={() => cycle(t)} title="click to advance status">
                    <span class={`pri pri-${t.priority}`}>{t.priority}</span>
                    <div class="card-title">{t.title}</div>
                    <Show when={t.tags.length}>
                      <div class="tags">{t.tags.map((x) => `#${x}`).join(" ")}</div>
                    </Show>
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

const STATUS_GLYPH: Record<DecisionStatus, string> = {
  proposed: "◇",
  accepted: "◆",
  rejected: "✖",
  superseded: "⊘",
};

const Decisions: Component = () => {
  const [decisions, { refetch }] = createResource(() => api.decisions());
  const [open, setOpen] = createSignal(false);
  const [form, setForm] = createSignal({ title: "", context: "", decision: "", consequences: "" });

  const add = async (e: Event) => {
    e.preventDefault();
    const f = form();
    if (!f.title.trim() || !f.decision.trim()) return;
    await api.addDecision({ ...f, project: "hive", status: "proposed" });
    setForm({ title: "", context: "", decision: "", consequences: "" });
    setOpen(false);
    refetch();
  };

  const setStatus = async (d: Decision, status: DecisionStatus) => {
    await api.patchDecision(d.id, { status });
    refetch();
  };

  return (
    <section>
      <div class="row">
        <button onClick={() => setOpen(!open())}>{open() ? "cancel" : "+ new decision"}</button>
      </div>

      <Show when={open()}>
        <form class="decision-form" onSubmit={add}>
          <input
            placeholder="title — the choice in a sentence"
            value={form().title}
            onInput={(e) => setForm({ ...form(), title: e.currentTarget.value })}
          />
          <textarea
            placeholder="context — what forces are at play?"
            value={form().context}
            onInput={(e) => setForm({ ...form(), context: e.currentTarget.value })}
          />
          <textarea
            placeholder="decision — what did we decide?"
            value={form().decision}
            onInput={(e) => setForm({ ...form(), decision: e.currentTarget.value })}
          />
          <textarea
            placeholder="consequences — what does this commit us to?"
            value={form().consequences}
            onInput={(e) => setForm({ ...form(), consequences: e.currentTarget.value })}
          />
          <button type="submit">record decision</button>
        </form>
      </Show>

      <For each={decisions()}>
        {(d) => (
          <article class={`decision status-${d.status}`}>
            <header>
              <span class="glyph">{STATUS_GLYPH[d.status]}</span>
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
            <footer class="row">
              <For each={DECISION_STATUSES}>
                {(s) => (
                  <button
                    classList={{ active: d.status === s }}
                    disabled={d.status === s}
                    onClick={() => setStatus(d, s)}
                  >
                    {s}
                  </button>
                )}
              </For>
            </footer>
          </article>
        )}
      </For>
    </section>
  );
};

// ---- Notes ----

const Notes: Component = () => {
  const [notes, { refetch }] = createResource(() => api.notes());
  const [title, setTitle] = createSignal("");
  const [body, setBody] = createSignal("");

  const add = async (e: Event) => {
    e.preventDefault();
    if (!title().trim()) return;
    await api.addNote({ title: title(), body: body() });
    setTitle("");
    setBody("");
    refetch();
  };

  return (
    <section>
      <form class="row" onSubmit={add}>
        <input placeholder="note title…" value={title()} onInput={(e) => setTitle(e.currentTarget.value)} />
        <input placeholder="body…" value={body()} onInput={(e) => setBody(e.currentTarget.value)} />
        <button type="submit">add</button>
      </form>
      <For each={notes()}>
        {(n: Note) => (
          <article class="note">
            <h3>{n.title}</h3>
            <p>{n.body}</p>
            <Show when={n.tags.length}>
              <div class="tags">{n.tags.map((x) => `#${x}`).join(" ")}</div>
            </Show>
          </article>
        )}
      </For>
    </section>
  );
};

// ---- Journal ----

const Journal: Component = () => {
  const [entries, { refetch }] = createResource(() => api.journal());
  const [body, setBody] = createSignal("");

  const add = async (e: Event) => {
    e.preventDefault();
    if (!body().trim()) return;
    await api.addJournal({ body: body(), project: "hive" });
    setBody("");
    refetch();
  };

  return (
    <section>
      <form class="row" onSubmit={add}>
        <input placeholder="what happened?" value={body()} onInput={(e) => setBody(e.currentTarget.value)} />
        <button type="submit">log</button>
      </form>
      <For each={entries()}>
        {(e: JournalEntry) => (
          <article class="entry">
            <time>{new Date(e.created_at).toLocaleString()}</time>
            <p>{e.body}</p>
          </article>
        )}
      </For>
    </section>
  );
};

// ---- Search ----

const SearchPane: Component = () => {
  const [q, setQ] = createSignal("");
  const [hits] = createResource(q, (query) => (query.trim() ? api.search(query) : Promise.resolve([])));

  return (
    <section>
      <div class="row">
        <input
          placeholder="search tasks, notes, journal, decisions…"
          value={q()}
          onInput={(e) => setQ(e.currentTarget.value)}
        />
      </div>
      <For each={hits() as SearchHit[]}>
        {(h) => (
          <div class="hit">
            <span class="badge">{h.kind}</span>
            <strong>{h.title}</strong>
            <span class="snippet" innerHTML={h.snippet} />
          </div>
        )}
      </For>
    </section>
  );
};

// ---- Wire ----

const Wire: Component = () => {
  const [events] = createResource(() => api.wire());
  return (
    <section class="wire">
      <For each={events() as WireEvent[]}>
        {(e) => (
          <div class="wire-row">
            <time>{new Date(e.created_at).toLocaleTimeString()}</time>
            <span class="actor-chip">{e.actor}</span>
            <code>{e.kind}</code>
          </div>
        )}
      </For>
    </section>
  );
};
