import { createResource, For, Show, Suspense, type Component, type JSX } from "solid-js";
import { Dynamic } from "solid-js/web";
import { liveRev } from "./live.ts";
import { SkeletonList } from "./lib.tsx";
import { EmptyState } from "./primitives.tsx";
import { colorVar, type ColorToken, type KindPresentation } from "./kinds.ts";

// EntityList — the presentational engine behind every flat entity board.
// A board is a CONFIG, not a component: kind presentation (glyph / icon /
// color / empty voice) comes from the kind registry, data comes from one
// fetch, and the row shape is a handful of accessors. Built-in boards and
// user-defined entity types render through the same seam.

export interface EntityListConfig<T> {
  kind: KindPresentation;
  /** Wrapped in a createResource keyed on liveRev(), so SSE refreshes it. */
  fetch: () => Promise<T[]>;
  /** Optional lead-in line above the list. */
  intro?: string;
  row: {
    title: (t: T) => string;
    badges?: (t: T) => { label: string; tone?: ColorToken }[];
    metas?: (t: T) => string[];
    /** Optional rich content rendered under the head row. */
    body?: Component<{ item: T }>;
    /** Optional "from journal: …" provenance quote. */
    originQuote?: (t: T) => string | null;
    onClick?: (t: T) => void;
  };
}

export function EntityList<T>(props: { config: EntityListConfig<T> }): JSX.Element {
  const [items] = createResource(
    () => ({ _r: liveRev() }),
    () => props.config.fetch(),
  );
  const kind = () => props.config.kind;
  const row = () => props.config.row;

  return (
    <section>
      <Show when={props.config.intro}>
        <p class="dim sm">{props.config.intro}</p>
      </Show>
      <Suspense fallback={<SkeletonList rows={5} />}>
        {/* .latest keeps the previous list on screen through liveRev refetches
            instead of flashing back to the skeleton (Workspaces pattern). */}
        <For
          each={items.latest ?? []}
          fallback={
            <EmptyState icon={kind().icon} title={kind().empty.title} hint={kind().empty.hint} />
          }
        >
          {(t) => (
            <article
              class="list-card"
              classList={{ clickable: !!row().onClick }}
              onClick={row().onClick ? () => row().onClick!(t) : undefined}
            >
              <div class="list-card-head">
                <span class="list-glyph" style={{ color: colorVar(kind().color) }} aria-hidden="true">
                  {kind().glyph}
                </span>
                <strong class="list-title">{row().title(t)}</strong>
                <For each={row().badges?.(t) ?? []}>
                  {(b) => (
                    <span class="badge" style={b.tone ? { color: colorVar(b.tone) } : undefined}>
                      {b.label}
                    </span>
                  )}
                </For>
                <For each={row().metas?.(t) ?? []}>{(m) => <span class="dim sm">{m}</span>}</For>
              </div>
              <Show when={row().body} keyed>
                {(Body) => <Dynamic component={Body} item={t} />}
              </Show>
              <Show when={row().originQuote?.(t)} keyed>
                {(q) => <blockquote class="origin">from journal: "{q}"</blockquote>}
              </Show>
            </article>
          )}
        </For>
      </Suspense>
    </section>
  );
}
