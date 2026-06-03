import { createResource, For, Show, type Component } from "solid-js";
import { DECISION_STATUSES, TASK_STATUSES } from "@hive/shared";
import { api } from "./api.ts";
import { liveRev } from "./live.ts";
import { DECISION_GLYPH, relTime, TASK_GLYPH } from "./lib.tsx";

/** Cross-board view with simple drill-down bars. */
export const Dashboard: Component = () => {
  const [stats] = createResource(() => ({ _r: liveRev() }), () => api.dashboard());

  return (
    <Show when={stats()} fallback={<p class="dim pad">loading…</p>}>
      {(s) => (
        <section class="dash">
          <div class="kpis">
            <Kpi n={s().entries} label="journal entries" />
            <Kpi n={s().tasks.total} label="tasks" />
            <Kpi n={s().decisions.total} label="decisions" />
            <Kpi n={s().events} label="events" />
          </div>

          <div class="dash-grid">
            <div class="panel">
              <h3>Tasks by status</h3>
              <For each={TASK_STATUSES}>
                {(st) => (
                  <Bar
                    label={`${TASK_GLYPH[st]} ${st}`}
                    value={s().tasks[st]}
                    total={s().tasks.total}
                  />
                )}
              </For>
            </div>

            <div class="panel">
              <h3>Decisions by status</h3>
              <For each={DECISION_STATUSES}>
                {(st) => (
                  <Bar
                    label={`${DECISION_GLYPH[st]} ${st}`}
                    value={s().decisions[st]}
                    total={s().decisions.total}
                  />
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

            <div class="panel">
              <h3>Entries by author</h3>
              <For each={s().byAuthor}>
                {(a) => {
                  const max = Math.max(...s().byAuthor.map((x) => x.entries), 1);
                  return <Bar label={a.author} value={a.entries} total={max} />;
                }}
              </For>
            </div>

            <div class="panel wide">
              <h3>Recent activity</h3>
              <For each={s().recent}>
                {(ev) => (
                  <div class="wire-row">
                    <time>{relTime(ev.created_at)}</time>
                    <span class="actor-chip sm">{ev.actor}</span>
                    <code>{ev.kind}</code>
                  </div>
                )}
              </For>
            </div>
          </div>
        </section>
      )}
    </Show>
  );
};

const Kpi: Component<{ n: number; label: string }> = (props) => (
  <div class="kpi">
    <div class="kpi-n">{props.n}</div>
    <div class="kpi-l">{props.label}</div>
  </div>
);

const Bar: Component<{ label: string; value: number; total: number }> = (props) => (
  <div class="bar-row">
    <span class="bar-label">{props.label}</span>
    <div class="bar-track">
      <div class="bar-fill" style={{ width: `${props.total ? (props.value / props.total) * 100 : 0}%` }} />
    </div>
    <span class="bar-val">{props.value}</span>
  </div>
);
