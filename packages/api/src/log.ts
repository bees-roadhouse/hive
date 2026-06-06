// One small leveled logger for the whole backend (api + worker), so the logs
// read like a friendly status feed instead of a pile of raw console calls and
// stack dumps. Keeps the 🐝 hive theme from the startup banner.
//
// Format:  🐝 2026-06-06T12:00:00.000Z INFO  [worker] cycle · polled 3 …
//
// Levels: debug < info < warn < error, gated by $HIVE_LOG_LEVEL (default info).
// debug/info/warn → stdout, error → stderr. Optional structured fields are
// appended as compact `key=value` pairs (never raw multi-line objects).
//
// EXPECTED, handled conditions (a model that won't load, a feed that 4xx'd, a
// bad request) should log a clean one-liner via warn/error. Reserve a full
// stack trace for genuinely-unexpected failures — and even then, prefix it with
// a clear human message via `unexpected()`. Don't log secrets/tokens/PII.

type Level = "debug" | "info" | "warn" | "error";
const ORDER: Record<Level, number> = { debug: 0, info: 1, warn: 2, error: 3 };

const THRESHOLD: number = ORDER[(process.env.HIVE_LOG_LEVEL?.toLowerCase() as Level) ?? "info"] ?? ORDER.info;

const ICON: Record<Level, string> = { debug: "·", info: "🐝", warn: "⚠", error: "✖" };

/** Render structured fields as compact ` key=value` pairs. Objects/arrays are
 *  JSON-stringified to a single line; nullish values are skipped. Keep these
 *  small and PII-free — they land in the log line verbatim. */
function fmtFields(fields?: Record<string, unknown>): string {
  if (!fields) return "";
  const parts: string[] = [];
  for (const [k, v] of Object.entries(fields)) {
    if (v === undefined || v === null) continue;
    const val = typeof v === "object" ? JSON.stringify(v) : String(v);
    parts.push(`${k}=${val}`);
  }
  return parts.length ? ` ${parts.join(" ")}` : "";
}

function emit(level: Level, component: string, message: string, fields?: Record<string, unknown>): void {
  if (ORDER[level] < THRESHOLD) return;
  const line = `${ICON[level]} ${new Date().toISOString()} ${level.toUpperCase().padEnd(5)} [${component}] ${message}${fmtFields(fields)}`;
  (level === "error" ? console.error : level === "warn" ? console.warn : console.log)(line);
}

/** A logger bound to a component tag, e.g. `const log = logger("worker")`. */
export function logger(component: string) {
  return {
    debug: (message: string, fields?: Record<string, unknown>) => emit("debug", component, message, fields),
    info: (message: string, fields?: Record<string, unknown>) => emit("info", component, message, fields),
    warn: (message: string, fields?: Record<string, unknown>) => emit("warn", component, message, fields),
    error: (message: string, fields?: Record<string, unknown>) => emit("error", component, message, fields),
    /** Genuinely-unexpected failure: a clear one-line message, THEN the stack
     *  (the only place a full trace belongs). Use sparingly — handled/expected
     *  errors should be a plain `warn`/`error` one-liner instead. */
    unexpected: (message: string, err: unknown) => {
      emit("error", component, message, { err: (err as Error)?.message ?? String(err) });
      if (err instanceof Error && err.stack) console.error(err.stack);
    },
  };
}

export type Logger = ReturnType<typeof logger>;
