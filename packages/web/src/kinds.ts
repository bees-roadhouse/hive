// Kind registry — the one place that says how an entity kind presents:
// glyph, icon, color token, labels, and the empty-state voice. Boards,
// autocomplete, and the graph read from here instead of keeping their own
// per-kind maps, so adding a kind is a one-row change — and user-defined
// entity types (see kindForType) plug into the exact same seam.
//
// Color rule: the palette whispers — every kind sits in the warm/gold
// range and the GLYPH carries identity. --danger never appears here.

export type ColorToken = "task" | "decision" | "event" | "honey" | "accent" | "dim" | "ink";

export interface KindPresentation {
  slug: string;
  label: string;
  labelPlural: string;
  /** One display character (◻ ◆ ◷ ◈ # @ …) — the glyph carries kind identity. */
  glyph: string;
  /** Name of a stroke icon in icons.tsx. */
  icon: string;
  color: ColorToken;
  /** EmptyState copy — voice rule: state the state, point the way, period. */
  empty: { title: string; hint: string };
}

export const KIND: Record<string, KindPresentation> = {
  task: {
    slug: "task",
    label: "task",
    labelPlural: "tasks",
    glyph: "◻",
    icon: "tasks",
    color: "task",
    empty: { title: "No tasks yet.", hint: "Tag !task text in a journal entry to create one." },
  },
  decision: {
    slug: "decision",
    label: "decision",
    labelPlural: "decisions",
    glyph: "◆",
    icon: "decisions",
    color: "honey",
    empty: { title: "No decisions yet.", hint: "Anchor one in a journal entry to start the log." },
  },
  event: {
    slug: "event",
    label: "event",
    labelPlural: "events",
    glyph: "◷",
    icon: "events",
    color: "event",
    empty: { title: "No events yet.", hint: "Anchor one in a journal entry to put it on the record." },
  },
  person: {
    slug: "person",
    label: "person",
    labelPlural: "people",
    glyph: "@",
    icon: "person",
    color: "dim",
    empty: { title: "No people yet.", hint: "Mention someone in a journal entry to introduce them." },
  },
  topic: {
    slug: "topic",
    label: "topic",
    labelPlural: "topics",
    glyph: "#",
    icon: "topic",
    color: "ink",
    empty: { title: "No topics yet.", hint: "Tag #topic in a journal entry to open one." },
  },
  project: {
    slug: "project",
    label: "project",
    labelPlural: "projects",
    glyph: "◈",
    icon: "project",
    color: "accent",
    empty: { title: "No projects yet.", hint: "Give a task a +project in a journal entry to start one." },
  },
  phase: {
    slug: "phase",
    label: "phase",
    labelPlural: "phases",
    glyph: "◷",
    icon: "phase",
    color: "event",
    empty: { title: "No phases yet.", hint: "Phases appear as a project's plan takes shape." },
  },
  journal: {
    slug: "journal",
    label: "journal entry",
    labelPlural: "journal entries",
    glyph: "⬡",
    icon: "journal",
    color: "ink",
    empty: { title: "No entries yet.", hint: "Write the first one." },
  },
  note: {
    slug: "note",
    label: "note",
    labelPlural: "notes",
    glyph: "≡",
    icon: "journal",
    color: "dim",
    empty: { title: "No notes yet.", hint: "Capture one in a journal entry." },
  },
  mail: {
    slug: "mail",
    label: "mail",
    labelPlural: "mail",
    glyph: "✉",
    icon: "mail",
    color: "honey",
    empty: { title: "No mail yet.", hint: "Connect sync and searchable messages will appear here." },
  },
};

/** CSS custom-property reference for a color token: "task" → "var(--task)". */
export const colorVar = (t: ColorToken): string => `var(--${t})`;

/** Resolve a token to its computed color value (for canvas/SVG painting). */
export const resolveColor = (name: string): string =>
  getComputedStyle(document.documentElement).getPropertyValue(`--${name}`).trim();

const COLOR_TOKENS: ReadonlySet<string> = new Set<ColorToken>([
  "task", "decision", "event", "honey", "accent", "dim", "ink",
]);

// The stroke-icon names icons.tsx actually draws (its switch falls back to a
// generic circle for anything else; we prefer the hex cell for unknowns).
const ICON_NAMES: ReadonlySet<string> = new Set([
  "journal", "inbox", "dashboard", "tasks", "decisions", "events", "graph",
  "search", "wire", "admin", "settings", "workspaces", "chats", "hex", "more",
  "person", "account", "people", "topic", "topics", "project", "projects",
  "phase", "link", "quote", "chev-l", "chev-r", "mail",
]);

/**
 * Presentation for a user-defined entity type (see EntityTypeView in
 * @hive/shared). Registry icon/color pass through with safe fallbacks:
 * an icon must exist in icons.tsx (else the hex cell), a color must be a
 * ColorToken (else honey). Custom kinds share the neutral cell glyph.
 */
export function kindForType(t: {
  slug: string;
  name: string;
  name_plural: string;
  icon: string;
  color: string;
}): KindPresentation {
  return {
    slug: t.slug,
    label: t.name,
    labelPlural: t.name_plural,
    glyph: "⬡",
    icon: ICON_NAMES.has(t.icon) ? t.icon : "hex",
    color: COLOR_TOKENS.has(t.color) ? (t.color as ColorToken) : "honey",
    empty: {
      title: `No ${t.name_plural.toLowerCase()} yet.`,
      hint: `Add the first ${t.name.toLowerCase()} to start the collection.`,
    },
  };
}
