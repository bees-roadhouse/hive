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

// A single shared EventSource for the whole app. EventSource auto-reconnects on
// drop, so reconnect logic is handled by the browser for free.
const es = new EventSource("/api/stream");
es.onmessage = () => bump();
// SSE comment lines (heartbeat) do not fire onmessage, so they don't cause spurious bumps.
