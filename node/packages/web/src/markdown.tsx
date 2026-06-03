import { createEffect, type JSX } from "solid-js";
import { marked } from "marked";
import DOMPurify from "dompurify";
import type { ResolvedAnchor } from "@hive/shared";
import { ANCHOR_GLYPH } from "./lib.tsx";

// GFM (task-list checkboxes, tables) + soft line breaks so a single newline in
// journal prose renders as a line break rather than being swallowed.
marked.setOptions({ gfm: true, breaks: true });

/** Markdown source → sanitized HTML string. */
export function renderMarkdown(src: string): string {
  return DOMPurify.sanitize(marked.parse(src) as string);
}

/** Render markdown safely (no anchor overlay) — for entity/note/decision bodies. */
export function Markdown(props: { src: string; class?: string }): JSX.Element {
  return <div class={`md ${props.class ?? ""}`} innerHTML={renderMarkdown(props.src)} />;
}

const MENTION = /@[a-z][a-z0-9_-]*/gi;

/**
 * Render a journal entry as markdown, then overlay clickable anchor highlights
 * and @mention chips. Anchors are matched by their text (markdown rendering
 * moots the raw char offsets); an anchor whose text is split across markdown
 * formatting simply isn't highlighted inline — it's still reachable from the
 * boards and the drawer.
 */
export function JournalBody(props: {
  body: string;
  anchors: ResolvedAnchor[];
  onAnchor?: (a: ResolvedAnchor) => void;
}): JSX.Element {
  let el!: HTMLDivElement;
  createEffect(() => {
    el.innerHTML = renderMarkdown(props.body);
    // Longest first so a short anchor can't pre-empt a longer overlapping one.
    for (const a of [...props.anchors].sort((x, y) => y.text.length - x.text.length)) {
      wrapFirst(el, a.text, (span) => {
        span.className = `anchor anchor-${a.kind}`;
        span.title = `${a.kind} — click to open`;
        span.addEventListener("click", () => props.onAnchor?.(a));
        const sup = document.createElement("sup");
        sup.textContent = ANCHOR_GLYPH[a.kind];
        span.appendChild(sup);
      });
    }
    chipMentions(el);
  });
  return <div ref={el} class="md prose" />;
}

/** Wrap the first plain occurrence of `needle` in a <span>, then `decorate` it.
 * Skips text already inside an anchor/mention/code so passes don't nest. */
function wrapFirst(root: HTMLElement, needle: string, decorate: (span: HTMLSpanElement) => void): void {
  if (!needle) return;
  const walker = document.createTreeWalker(root, NodeFilter.SHOW_TEXT);
  let node = walker.nextNode() as Text | null;
  while (node) {
    if (!node.parentElement?.closest(".anchor, .mention, code, pre")) {
      const i = node.nodeValue!.indexOf(needle);
      if (i !== -1) {
        const match = node.splitText(i);
        match.splitText(needle.length);
        const span = document.createElement("span");
        span.textContent = match.nodeValue;
        match.replaceWith(span);
        decorate(span);
        return;
      }
    }
    node = walker.nextNode() as Text | null;
  }
}

/** Wrap every @mention in a chip across the rendered text. */
function chipMentions(root: HTMLElement): void {
  const walker = document.createTreeWalker(root, NodeFilter.SHOW_TEXT);
  const targets: Text[] = [];
  let node = walker.nextNode() as Text | null;
  while (node) {
    if (!node.parentElement?.closest(".anchor, .mention, code, pre")) {
      MENTION.lastIndex = 0;
      if (MENTION.test(node.nodeValue!)) targets.push(node);
    }
    node = walker.nextNode() as Text | null;
  }
  for (const t of targets) {
    const s = t.nodeValue!;
    const frag = document.createDocumentFragment();
    let last = 0;
    MENTION.lastIndex = 0;
    for (let m = MENTION.exec(s); m; m = MENTION.exec(s)) {
      if (m.index > last) frag.appendChild(document.createTextNode(s.slice(last, m.index)));
      const span = document.createElement("span");
      span.className = "mention";
      span.textContent = m[0];
      frag.appendChild(span);
      last = m.index + m[0].length;
    }
    if (last < s.length) frag.appendChild(document.createTextNode(s.slice(last)));
    t.replaceWith(frag);
  }
}
