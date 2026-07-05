import { For, type JSX } from "solid-js";
import type { AnchorKind, DecisionStatus, ResolvedAnchor, TaskStatus } from "@hive/shared";

export const ANCHOR_GLYPH: Record<AnchorKind, string> = {
  task: "◻",
  decision: "◆",
  event: "◷",
};

export const TASK_GLYPH: Record<TaskStatus, string> = {
  todo: "○",
  doing: "◐",
  blocked: "✖",
  done: "●",
};

export const DECISION_GLYPH: Record<DecisionStatus, string> = {
  proposed: "◇",
  accepted: "◆",
  rejected: "✖",
  superseded: "⊘",
};

/** Shimmer placeholder rows shown while a list resource is still loading. */
export function SkeletonList(props: { rows?: number }): JSX.Element {
  const rows = () => Array.from({ length: props.rows ?? 5 });
  return (
    <div class="skeleton-list" aria-hidden="true">
      <For each={rows()}>{() => <div class="skeleton skeleton-row" />}</For>
    </div>
  );
}

/**
 * Search snippets come from Postgres ts_headline with StartSel=[ / StopSel=]
 * (plain-text markers, see api semantic.rs). The body text itself is untrusted
 * (journal prose today, ingested content tomorrow), so escape it BEFORE turning
 * the markers into <mark> — this is the only place snippet HTML is built, and
 * it must never pass raw body text to innerHTML.
 */
export function highlightSnippet(s: string): string {
  const esc = s
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;");
  return esc.replace(/\[([^\[\]]{1,120}?)\]/g, "<mark>$1</mark>");
}

export const relTime = (iso: string) => {
  const s = (Date.now() - new Date(iso).getTime()) / 1000;
  if (s < 60) return "just now";
  if (s < 3600) return `${Math.floor(s / 60)}m ago`;
  if (s < 86400) return `${Math.floor(s / 3600)}h ago`;
  return new Date(iso).toLocaleDateString();
};

/**
 * Render journal prose with its anchored spans highlighted (click → onAnchor)
 * and @mentions chipped. `anchors` are server-resolved with start/end offsets.
 */
export function Prose(props: {
  body: string;
  anchors: ResolvedAnchor[];
  onAnchor?: (a: ResolvedAnchor) => void;
}): JSX.Element {
  const sorted = () => [...props.anchors].sort((a, b) => a.start - b.start);

  // Build a flat list of segments: anchored spans + the plain text between them.
  const segments = () => {
    const segs: ({ type: "plain"; text: string } | { type: "anchor"; a: ResolvedAnchor })[] = [];
    let cursor = 0;
    for (const a of sorted()) {
      if (a.start > cursor) segs.push({ type: "plain", text: props.body.slice(cursor, a.start) });
      segs.push({ type: "anchor", a });
      cursor = Math.max(cursor, a.end);
    }
    if (cursor < props.body.length) segs.push({ type: "plain", text: props.body.slice(cursor) });
    return segs;
  };

  return (
    <p class="prose">
      <For each={segments()}>
        {(seg) =>
          seg.type === "plain" ? (
            <Mentions text={seg.text} />
          ) : (
            <span
              class={`anchor anchor-${seg.a.kind}`}
              title={`${seg.a.kind} — click to open`}
              onClick={() => props.onAnchor?.(seg.a)}
            >
              {seg.a.text}
              <sup>{ANCHOR_GLYPH[seg.a.kind]}</sup>
            </span>
          )
        }
      </For>
    </p>
  );
}

/** Wrap @mentions in chips. */
export function Mentions(props: { text: string }): JSX.Element {
  const parts = () => props.text.split(/(@[a-z][a-z0-9_-]*)/gi);
  return (
    <For each={parts()}>
      {(p) => (p.startsWith("@") ? <span class="mention">{p}</span> : <>{p}</>)}
    </For>
  );
}
