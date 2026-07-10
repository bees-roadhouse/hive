import { createEffect, createResource, createSignal, For, Show, type Component } from "solid-js";
import type { DashboardStats, WireEvent } from "@hive/shared";
import { ACTORS, DECISION_STATUSES, TASK_STATUSES } from "@hive/shared";
import { api } from "./api.ts";
import { liveRev } from "./live.ts";
import { Icon } from "./icons.tsx";
import { relTime, SkeletonList, TASK_GLYPH, DECISION_GLYPH } from "./lib.tsx";

// Known AI actors (matches ACTORS from shared — kind='ai').
const AI_ACTORS = new Set(ACTORS.filter((a) => a.kind === "ai").map((a) => a.name));

// ---- date helpers ----

const todayISO = () => new Date().toISOString().slice(0, 10);

function isoToDate(iso: string): Date {
  // YYYY-MM-DD string — parse as local midnight to avoid timezone drift on comparisons.
  const [y, m, d] = iso.slice(0, 10).split("-").map(Number);
  return new Date(y, m - 1, d);
}

function isOverdue(due: string, status: string): boolean {
  return status !== "done" && isoToDate(due) < isoToDate(todayISO());
}

function isDueToday(due: string): boolean {
  return due.slice(0, 10) === todayISO();
}

/** Generate the 30-day window, filling missing days with count=0. */
function fillDays(raw: { day: string; count: number }[]): { day: string; count: number }[] {
  const map = new Map(raw.map((r) => [r.day, r.count]));
  const days: { day: string; count: number }[] = [];
  for (let i = 29; i >= 0; i--) {
    const d = new Date();
    d.setDate(d.getDate() - i);
    const key = d.toISOString().slice(0, 10);
    days.push({ day: key, count: map.get(key) ?? 0 });
  }
  return days;
}

// ---- SVG sparkline bar chart ----

const SparkBars: Component<{
  data: { day: string; count: number }[];
  height?: number;
}> = (props) => {
  const h = () => props.height ?? 64;
  const max = () => Math.max(...props.data.map((d) => d.count), 1);
  const bw = () => 100 / props.data.length;

  return (
    <svg
      viewBox={`0 0 100 ${h()}`}
      preserveAspectRatio="none"
      class="spark-svg"
      style={{ height: `${h()}px` }}
    >
      <For each={props.data}>
        {(d, i) => {
          const bh = () => Math.max((d.count / max()) * (h() - 4), d.count > 0 ? 4 : 0);
          const x = () => i() * bw() + bw() * 0.1;
          const bwPx = () => bw() * 0.8;
          const y = () => h() - bh();
          return (
            <rect
              x={x()}
              y={y()}
              width={bwPx()}
              height={bh()}
              rx="1.5"
              class="spark-bar"
              classList={{ "spark-bar-zero": d.count === 0 }}
            >
              <title>{d.day}: {d.count}</title>
            </rect>
          );
        }}
      </For>
    </svg>
  );
};

// ---- Horizontal bar row (for author/callout charts) ----

const HBar: Component<{
  label: string;
  value: number;
  max: number;
}> = (props) => {
  const pct = () => (props.max > 0 ? (props.value / props.max) * 100 : 0);
  return (
    <div class="dash-hbar-row">
      <span class="dash-hbar-label">{props.label}</span>
      <div class="dash-hbar-track">
        <div class="dash-hbar-fill" style={{ width: `${pct()}%` }} />
      </div>
      <span class="dash-hbar-val">{props.value}</span>
    </div>
  );
};

// ---- Calendar helpers ----

const MONTH_NAMES = ["Jan","Feb","Mar","Apr","May","Jun","Jul","Aug","Sep","Oct","Nov","Dec"];

function monthName(month: number): string { return MONTH_NAMES[month - 1]; }

function daysInMonth(year: number, month: number): number {
  return new Date(year, month, 0).getDate();
}

function firstWeekday(year: number, month: number): number {
  return new Date(year, month - 1, 1).getDay();
}

function toISO(year: number, month: number, day: number): string {
  return `${year}-${String(month).padStart(2, "0")}-${String(day).padStart(2, "0")}`;
}

// ---- Month calendar grid ----

const MonthCalendar: Component<{
  year: number;
  month: number;
  tasksWithDue: DashboardStats["tasksWithDue"];
}> = (props) => {
  const today = todayISO();
  const dim = () => daysInMonth(props.year, props.month);
  const startWd = () => firstWeekday(props.year, props.month);

  const cells = (): number[] => {
    const arr: number[] = Array(startWd()).fill(0);
    for (let d = 1; d <= dim(); d++) arr.push(d);
    while (arr.length % 7 !== 0) arr.push(0);
    return arr;
  };

  const byDay = (): Map<string, DashboardStats["tasksWithDue"]> => {
    const m = new Map<string, DashboardStats["tasksWithDue"]>();
    for (const t of props.tasksWithDue) {
      const key = t.due.slice(0, 10);
      if (!m.has(key)) m.set(key, []);
      m.get(key)!.push(t);
    }
    return m;
  };

  const DAY_LABELS = ["Su", "Mo", "Tu", "We", "Th", "Fr", "Sa"];

  return (
    <div class="cal-grid">
      <For each={DAY_LABELS}>{(d) => <div class="cal-weekday">{d}</div>}</For>
      <For each={cells()}>
        {(day) => {
          if (day === 0) return <div class="cal-cell cal-empty" />;
          const iso = toISO(props.year, props.month, day);
          const dayTasks = () => byDay().get(iso) ?? [];
          const isToday = iso === today;
          return (
            <div classList={{ "cal-cell": true, "cal-today": isToday }}>
              <span class="cal-num">{day}</span>
              <Show when={dayTasks().length > 0}>
                <div class="cal-chips">
                  <For each={dayTasks().slice(0, 3)}>
                    {(t) => (
                      <span
                        class="cal-task-chip"
                        classList={{
                          "cal-chip-overdue": isOverdue(t.due, t.status),
                          "cal-chip-today": isDueToday(t.due) && !isOverdue(t.due, t.status),
                        }}
                        title={t.title}
                      >
                        {t.title.length > 11 ? t.title.slice(0, 11) + "…" : t.title}
                      </span>
                    )}
                  </For>
                  <Show when={dayTasks().length > 3}>
                    <span class="cal-task-chip cal-chip-more">+{dayTasks().length - 3}</span>
                  </Show>
                </div>
              </Show>
            </div>
          );
        }}
      </For>
    </div>
  );
};

// ---- Upcoming / overdue tasks list ----

const UpcomingTasks: Component<{ tasksWithDue: DashboardStats["tasksWithDue"] }> = (props) => {
  const today = todayISO();

  const overdue = () =>
    props.tasksWithDue
      .filter((t) => isOverdue(t.due, t.status))
      .sort((a, b) => a.due.localeCompare(b.due));

  const upcoming = () =>
    props.tasksWithDue
      .filter((t) => !isOverdue(t.due, t.status))
      .sort((a, b) => a.due.localeCompare(b.due))
      .slice(0, 8);

  const fmtDue = (iso: string) => {
    if (iso.slice(0, 10) === today) return "today";
    const [, m, d] = iso.slice(0, 10).split("-").map(Number);
    return `${monthName(m)} ${d}`;
  };

  return (
    <div class="upcoming-tasks">
      <Show when={overdue().length > 0}>
        <div class="upcoming-section">
          <span class="upcoming-group-label overdue-label">overdue</span>
          <For each={overdue()}>
            {(t) => (
              <div class="upcoming-row">
                <span class="cal-task-chip cal-chip-overdue" title={t.title}>
                  {t.title.length > 26 ? t.title.slice(0, 26) + "…" : t.title}
                </span>
                <span class="dim sm">{fmtDue(t.due)}</span>
              </div>
            )}
          </For>
        </div>
      </Show>
      <Show when={upcoming().length > 0}>
        <div class="upcoming-section">
          <span class="upcoming-group-label">upcoming</span>
          <For each={upcoming()}>
            {(t) => (
              <div class="upcoming-row">
                <span
                  class="cal-task-chip"
                  classList={{ "cal-chip-today": isDueToday(t.due) }}
                  title={t.title}
                >
                  {t.title.length > 26 ? t.title.slice(0, 26) + "…" : t.title}
                </span>
                <span class="dim sm">{fmtDue(t.due)}</span>
              </div>
            )}
          </For>
        </div>
      </Show>
      <Show when={props.tasksWithDue.length === 0}>
        <p class="dim sm">no tasks with due dates</p>
      </Show>
    </div>
  );
};

// ---- Agent activity live feed ----

const AgentFeed: Component<{ events: WireEvent[] }> = (props) => {
  const agentEvents = () =>
    props.events.filter((e) => AI_ACTORS.has(e.actor)).slice(0, 18);

  // Flash agent rows that just arrived over the live stream (skip first paint).
  let seen: Set<string> | null = null;
  const isFresh = (e: WireEvent): boolean => seen !== null && !seen.has(e.id);
  createEffect(() => {
    seen = new Set(agentEvents().map((e) => e.id));
  });

  return (
    <div class="agent-feed">
      <Show when={agentEvents().length === 0}>
        <p class="dim sm">no agent activity yet — AI actors will appear here as they write</p>
      </Show>
      <For each={agentEvents()}>
        {(ev) => (
          <div class="agent-row" classList={{ "just-landed": isFresh(ev) }}>
            <span class="agent-actor kind-ai">{ev.actor}</span>
            <code class="agent-kind">{ev.kind}</code>
            <time class="agent-time">{relTime(ev.created_at)}</time>
          </div>
        )}
      </For>
    </div>
  );
};

// ---- KPI tile ----

const Kpi: Component<{ n: number; label: string }> = (props) => (
  <div class="kpi">
    <div class="kpi-n">{props.n}</div>
    <div class="kpi-l">{props.label}</div>
  </div>
);

const fmtBytes = (n: number): string => {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(0)} KB`;
  if (n < 1024 * 1024 * 1024) return `${(n / (1024 * 1024)).toFixed(1)} MB`;
  return `${(n / (1024 * 1024 * 1024)).toFixed(2)} GB`;
};

// ---- Main dashboard export ----

export const Dashboard: Component = () => {
  const [stats] = createResource(() => ({ _r: liveRev() }), () => api.dashboard());
  const [wireEvents] = createResource(() => ({ _r: liveRev() }), () => api.wire());
  // Mail ships dark until HIVE_MAIL_ENABLED — hide the archive card with it.
  const [authCfg] = createResource(() => api.authConfig());

  const now = new Date();
  const [viewYear, setViewYear] = createSignal(now.getFullYear());
  const [viewMonth, setViewMonth] = createSignal(now.getMonth() + 1);

  const prevMonth = () => {
    if (viewMonth() === 1) { setViewYear((y) => y - 1); setViewMonth(12); }
    else setViewMonth((m) => m - 1);
  };
  const nextMonth = () => {
    if (viewMonth() === 12) { setViewYear((y) => y + 1); setViewMonth(1); }
    else setViewMonth((m) => m + 1);
  };

  return (
    <Show when={stats()} fallback={<SkeletonList rows={3} />}>
      {(s) => {
        const filledDays = () => fillDays(s().entriesByDay);
        const maxDayCount = () => Math.max(...filledDays().map((d) => d.count), 1);
        const maxAuthorCount = () => Math.max(...s().entriesByAuthor.map((a) => a.count), 1);
        const maxCallouts = () => Math.max(...(s().calloutsByPerson.length ? s().calloutsByPerson.map((c) => c.count) : [1]), 1);

        return (
          <section class="dash">

            {/* ---- KPI tiles ---- */}
            <div class="kpis">
              <Kpi n={s().entries} label="journal entries" />
              <Kpi n={s().tasks.todo + s().tasks.doing + s().tasks.blocked} label="open tasks" />
              <Kpi n={s().decisions.total} label="decisions" />
              <Kpi n={s().calloutsByPerson.length} label="people referenced" />
            </div>

            {/* ---- Calendar + Upcoming ---- */}
            <div class="dash-cal-row">
              <div class="panel dash-cal-panel">
                <div class="cal-header">
                  <button class="ghost" onClick={prevMonth} title="previous month" aria-label="previous month">
                    <Icon name="chev-l" size={14} />
                  </button>
                  <h3>{monthName(viewMonth())} {viewYear()}</h3>
                  <button class="ghost" onClick={nextMonth} title="next month" aria-label="next month">
                    <Icon name="chev-r" size={14} />
                  </button>
                </div>
                <MonthCalendar
                  year={viewYear()}
                  month={viewMonth()}
                  tasksWithDue={s().tasksWithDue}
                />
                <div class="cal-legend">
                  <span class="cal-leg"><span class="cal-swatch cal-swatch-overdue" /> overdue</span>
                  <span class="cal-leg"><span class="cal-swatch cal-swatch-today" /> today</span>
                  <span class="cal-leg"><span class="cal-swatch cal-swatch-normal" /> upcoming</span>
                </div>
              </div>

              <div class="panel dash-upcoming-panel">
                <h3>Due & overdue</h3>
                <UpcomingTasks tasksWithDue={s().tasksWithDue} />
              </div>
            </div>

            {/* ---- Charts row ---- */}
            <div class="dash-charts-row">
              <div class="panel">
                <h3>Activity · last 30 days</h3>
                <SparkBars data={filledDays()} height={68} />
                <div class="spark-axis">
                  <span class="dim sm">{filledDays()[0]?.day?.slice(5)}</span>
                  <span class="dim sm">today</span>
                </div>
                <p class="dash-chart-sub">
                  <span class="dim sm">peak {maxDayCount()} · 30d total {filledDays().reduce((a, d) => a + d.count, 0)}</span>
                </p>
              </div>

              <div class="panel">
                <h3>Entries by author</h3>
                <For each={s().entriesByAuthor}>
                  {(a) => <HBar label={a.author} value={a.count} max={maxAuthorCount()} />}
                </For>
                <Show when={s().entriesByAuthor.length === 0}>
                  <p class="dim sm">no entries yet</p>
                </Show>
              </div>

              <div class="panel">
                <h3>Person callouts</h3>
                <For each={s().calloutsByPerson}>
                  {(c) => <HBar label={c.name} value={c.count} max={maxCallouts()} />}
                </For>
                <Show when={s().calloutsByPerson.length === 0}>
                  <p class="dim sm">no person references yet</p>
                </Show>
              </div>
            </div>

            {/* ---- Status panels + Agent feed ---- */}
            <div class="dash-grid">
              <div class="panel">
                <h3>Tasks by status</h3>
                <For each={TASK_STATUSES}>
                  {(st) => (
                    <div class="bar-row">
                      <span class="bar-label">{TASK_GLYPH[st]} {st}</span>
                      <div class="bar-track">
                        <div
                          class="bar-fill"
                          style={{ width: `${s().tasks.total ? (s().tasks[st] / s().tasks.total) * 100 : 0}%` }}
                        />
                      </div>
                      <span class="bar-val">{s().tasks[st]}</span>
                    </div>
                  )}
                </For>
              </div>

              <div class="panel">
                <h3>Decisions by status</h3>
                <For each={DECISION_STATUSES}>
                  {(st) => (
                    <div class="bar-row">
                      <span class="bar-label">{DECISION_GLYPH[st]} {st}</span>
                      <div class="bar-track">
                        <div
                          class="bar-fill"
                          style={{ width: `${s().decisions.total ? (s().decisions[st] / s().decisions.total) * 100 : 0}%` }}
                        />
                      </div>
                      <span class="bar-val">{s().decisions[st]}</span>
                    </div>
                  )}
                </For>
              </div>

              <div class="panel">
                <h3>Inboxes</h3>
                <table class="mini">
                  <For each={s().inbox}>
                    {(i) => (
                      <tr>
                        <td>
                          <span class="actor-chip sm">{i.recipient}</span>
                          <span class="dim"> {i.kind}</span>
                        </td>
                        <td class="num">
                          <Show when={i.unread} fallback={<span class="dim">0</span>}>
                            <span class="unread-pill">{i.unread}</span>
                          </Show>
                          <span class="dim"> / {i.total}</span>
                        </td>
                      </tr>
                    )}
                  </For>
                </table>
              </div>

              <Show when={authCfg()?.mailEnabled === true && s().mail}>
                {(m) => (
                  <div class="panel">
                    <h3>Mail archive</h3>
                    <table class="mini">
                      <tbody>
                        <tr>
                          <td>messages</td>
                          <td class="num">{m().messages}</td>
                        </tr>
                        <tr>
                          <td>accounts</td>
                          <td class="num">{m().accounts}</td>
                        </tr>
                        <tr>
                          <td>searchable</td>
                          <td class="num">{m().search}</td>
                        </tr>
                        <tr>
                          <td>attachment storage</td>
                          <td class="num">{fmtBytes(m().blobBytes)}</td>
                        </tr>
                      </tbody>
                    </table>
                  </div>
                )}
              </Show>

              {/* Agent live feed — full width */}
              <div class="panel wide">
                <h3 class="agent-feed-title">
                  Agents · live
                  <span class="live-dot" />
                </h3>
                <Show when={wireEvents()} fallback={<p class="dim sm">connecting…</p>}>
                  {(evs) => <AgentFeed events={evs()} />}
                </Show>
              </div>
            </div>

          </section>
        );
      }}
    </Show>
  );
};
