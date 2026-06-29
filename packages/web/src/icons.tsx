import type { JSX } from "solid-js";

// One cohesive line-icon family (currentColor, consistent stroke) so the whole
// shell reads as a single system instead of a mix of emoji and glyphs. The nav
// tasks/decisions/events marks deliberately echo their inline anchor glyphs
// (square / diamond / clock). The bee 🐝 stays as the brand mark.

// Fresh nodes per call (a DOM node can't live in two places) — so this is a
// function, not a lookup table of pre-built JSX.
function paths(name: string): JSX.Element {
  switch (name) {
    case "journal": // open book — the comb you write into
      return (
        <>
          <path d="M2 4h6a3 3 0 0 1 3 3v13a2.5 2.5 0 0 0-2.5-2.5H2z" />
          <path d="M22 4h-6a3 3 0 0 0-3 3v13a2.5 2.5 0 0 1 2.5-2.5H22z" />
        </>
      );
    case "inbox":
      return (
        <>
          <path d="M22 12h-6l-2 3h-4l-2-3H2" />
          <path d="M5.5 5.1 2 12v6a2 2 0 0 0 2 2h16a2 2 0 0 0 2-2v-6l-3.5-6.9A2 2 0 0 0 16.8 4H7.2a2 2 0 0 0-1.7 1.1z" />
        </>
      );
    case "dashboard":
      return (
        <>
          <rect x="3" y="3" width="7" height="7" rx="1.2" />
          <rect x="14" y="3" width="7" height="7" rx="1.2" />
          <rect x="14" y="14" width="7" height="7" rx="1.2" />
          <rect x="3" y="14" width="7" height="7" rx="1.2" />
        </>
      );
    case "tasks": // check-square — echoes the ◻ anchor glyph
      return (
        <>
          <path d="M9 11l3 3L22 4" />
          <path d="M21 12v7a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h11" />
        </>
      );
    case "decisions": // balance scale — a decision weighed
      return (
        <>
          <path d="M12 3v17M7 20h10" />
          <path d="M5 7h14M5 7l-2.5 5h5zM19 7l-2.5 5h5z" />
        </>
      );
    case "events": // clock — echoes the ◷ anchor glyph
      return (
        <>
          <circle cx="12" cy="12" r="9" />
          <path d="M12 7.5V12l3 2" />
        </>
      );
    case "graph": // linked nodes
      return (
        <>
          <circle cx="6" cy="12" r="2.6" />
          <circle cx="18" cy="6" r="2.6" />
          <circle cx="18" cy="18" r="2.6" />
          <path d="M8.3 10.8l7.4-3.6M8.3 13.2l7.4 3.6" />
        </>
      );
    case "search":
      return (
        <>
          <circle cx="11" cy="11" r="7" />
          <path d="M21 21l-4.3-4.3" />
        </>
      );
    case "wire": // broadcast / rss — waves radiating from a node
      return (
        <>
          <circle cx="5" cy="19" r="1.5" />
          <path d="M5 12a7 7 0 0 1 7 7M5 5a14 14 0 0 1 14 14" />
        </>
      );
    case "admin": // shield — guarded surface, with a check
      return (
        <>
          <path d="M12 2.5l8 3v6c0 4.6-3.3 7.9-8 10-4.7-2.1-8-5.4-8-10v-6z" />
          <path d="M9 11.8l2 2 4-4.5" />
        </>
      );
    case "settings": // cog
      return (
        <>
          <circle cx="12" cy="12" r="3" />
          <path d="M12 2v3M12 19v3M4.2 4.2l2.1 2.1M17.7 17.7l2.1 2.1M2 12h3M19 12h3M4.2 19.8l2.1-2.1M17.7 6.3l2.1-2.1" />
        </>
      );
    case "workspaces": // terminal window — a hosted Claude Code session
      return (
        <>
          <rect x="3" y="4" width="18" height="16" rx="2" />
          <path d="M7 9l3 3-3 3M13 15h4" />
        </>
      );
    case "hex": // honeycomb cell — the day-page marker
      return <path d="M12 2.5l8.2 4.75v9.5L12 21.5l-8.2-4.75v-9.5z" />;
    case "person": // user outline — head + shoulders
    case "account": // the signed-in user — same single-figure glyph
      return (
        <>
          <circle cx="12" cy="8" r="3.5" />
          <path d="M4.5 21c0-4.1 3.4-7.5 7.5-7.5s7.5 3.4 7.5 7.5" />
        </>
      );
    case "people": // two figures — the identities directory (distinct from a single person)
      return (
        <>
          <circle cx="9" cy="8" r="3" />
          <path d="M3.5 20c0-3.3 2.5-6 5.5-6s5.5 2.7 5.5 6" />
          <path d="M16 5.2a3 3 0 0 1 0 5.6M17.5 14.2c2.4.5 4 2.9 4 5.8" />
        </>
      );
    case "topic": // hash / tag
    case "topics":
      return (
        <>
          <path d="M4 9h16M4 15h16M9 4l-2 16M17 4l-2 16" />
        </>
      );
    case "project": // folder / layers
    case "projects":
      return (
        <>
          <path d="M3 7h7l2 2.5H21v10.5a1 1 0 0 1-1 1H4a1 1 0 0 1-1-1z" />
        </>
      );
    case "phase": // milestone / flag
      return (
        <>
          <path d="M6 4v16" />
          <path d="M6 4h11l-3 4 3 4H6" />
        </>
      );
    default:
      return <circle cx="12" cy="12" r="9" />;
  }
}

export function Icon(props: { name: string; size?: number; class?: string }): JSX.Element {
  return (
    <svg
      width={props.size ?? 18}
      height={props.size ?? 18}
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      stroke-width="1.8"
      stroke-linecap="round"
      stroke-linejoin="round"
      class={`icon ${props.class ?? ""}`}
      aria-hidden="true"
    >
      {paths(props.name)}
    </svg>
  );
}
