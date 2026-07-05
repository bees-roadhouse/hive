import { Show, type JSX } from "solid-js";
import { Icon } from "./icons.tsx";

// Shared presentation primitives — the pieces every page reaches for so the
// shell speaks with one voice. Backing CSS lives in styles.css under
// "shared primitives".

/**
 * EmptyState — the one voice for "nothing here yet".
 * Voice rule: sentence case; the first clause states the state, the second
 * points the way forward; period; no emoji, no exclamation marks.
 */
export function EmptyState(props: {
  icon: string;
  title: string;
  hint?: string;
  action?: JSX.Element;
}): JSX.Element {
  return (
    <div class="empty">
      <span class="empty-icon">
        <Icon name={props.icon} size={28} />
      </span>
      <p class="empty-title">{props.title}</p>
      <Show when={props.hint}>
        <p class="empty-hint">{props.hint}</p>
      </Show>
      <Show when={props.action}>
        <div class="empty-action">{props.action}</div>
      </Show>
    </div>
  );
}

/**
 * SectionHead — h3-level section header: optional leading icon, the title,
 * an optional count badge, and a right slot for controls (children).
 */
export function SectionHead(props: {
  title: string;
  icon?: string;
  count?: number;
  children?: JSX.Element;
}): JSX.Element {
  return (
    <div class="sec-head">
      <Show when={props.icon}>
        <span class="sec-head-icon">
          <Icon name={props.icon!} size={16} />
        </span>
      </Show>
      <h3>{props.title}</h3>
      <Show when={props.count !== undefined}>
        <span class="badge">{props.count}</span>
      </Show>
      {props.children}
    </div>
  );
}

/**
 * StatusDot — one dot family for every status light in the shell:
 * honey while something is live (pulse for active work), accent while it
 * waits on you, danger only for genuine failure, dim at rest.
 */
export function StatusDot(props: {
  tone?: "rest" | "live" | "waiting" | "danger";
  pulse?: boolean;
  title?: string;
}): JSX.Element {
  return (
    <span
      class={`dot dot-${props.tone ?? "rest"}`}
      classList={{ "dot-pulse": props.pulse }}
      title={props.title}
    />
  );
}
