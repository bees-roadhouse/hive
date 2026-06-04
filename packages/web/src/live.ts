// Module-level revision counter. Every SSE event from /api/stream bumps this
// signal so any createResource that depends on liveRev() refetches automatically.
//
// Debounce: events arriving within 300 ms of each other only trigger one bump —
// a burst of mutations (e.g. journal append → tasks created → inbox delivered)
// becomes a single refetch round instead of one per wire event.

import { createSignal } from "solid-js";

const [liveRev, setLiveRev] = createSignal(0);
export { liveRev };

let debounceTimer: ReturnType<typeof setTimeout> | null = null;

function bump() {
  if (debounceTimer !== null) return; // already scheduled within the window
  debounceTimer = setTimeout(() => {
    debounceTimer = null;
    setLiveRev((r) => r + 1);
  }, 300);
}

// A single shared EventSource for the whole app, opened only once the user is
// authenticated (the stream requires a session — connecting on the login screen
// would just 401-retry). EventSource auto-reconnects on drop, so the browser
// handles reconnect for free. SSE comment lines (heartbeat) don't fire onmessage.
let es: EventSource | null = null;
export function connectLive(): void {
  if (es) return;
  es = new EventSource("/api/stream");
  es.onmessage = () => bump();
}
