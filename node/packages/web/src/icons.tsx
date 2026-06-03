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
    case "decisions": // diamond — echoes the ◆ anchor glyph
      return <path d="M12 2.5 21.5 12 12 21.5 2.5 12z" />;
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
    case "wire": // activity pulse
      return <path d="M22 12h-4l-3 8L9 4l-3 8H2" />;
    case "admin": // sliders
      return (
        <>
          <path d="M3 7h9M16 7h5M3 17h5M12 17h9" />
          <circle cx="14" cy="7" r="2" />
          <circle cx="10" cy="17" r="2" />
        </>
      );
    case "settings": // cog
      return (
        <>
          <circle cx="12" cy="12" r="3" />
          <path d="M12 2v3M12 19v3M4.2 4.2l2.1 2.1M17.7 17.7l2.1 2.1M2 12h3M19 12h3M4.2 19.8l2.1-2.1M17.7 6.3l2.1-2.1" />
        </>
      );
    case "hex": // honeycomb cell — the day-page marker
      return <path d="M12 2.5l8.2 4.75v9.5L12 21.5l-8.2-4.75v-9.5z" />;
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
