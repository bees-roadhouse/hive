// Module-level revision counter. Every SSE event from /api/stream bumps this
// signal so any createResource that depends on liveRev() refetches automatically.
//
// Debounce: events arriving within 300 ms of each other only trigger one bump —
// a burst of mutations (e.g. journal append → tasks created → inbox delivered)
// becomes a single refetch round instead of one per wire event.

import { createSignal } from "solid-js";
import { drainOutbox, setAfterDrain } from "./outbox.ts";

const [liveRev, setLiveRev] = createSignal(0);
export { liveRev };

// Connectivity as a signal so the shell can show it. Flips drive the
// EventSource below and the outbox drain.
const [online, setOnline] = createSignal(navigator.onLine);
export { online };

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
// would just 401-retry). While OFFLINE the socket is closed rather than left
// to spin its reconnect backoff against an unreachable server; 'online'
// reopens it.
let es: EventSource | null = null;
export function connectLive(): void {
  if (es || !navigator.onLine) return;
  es = new EventSource("/api/stream");
  es.onmessage = () => bump();
  // Authenticated and reachable: anything left in the outbox from an earlier
  // offline stretch can replay now.
  void drainOutbox();
}

function disconnectLive(): void {
  es?.close();
  es = null;
}

window.addEventListener("offline", () => {
  setOnline(false);
  disconnectLive();
});

window.addEventListener("online", () => {
  setOnline(true);
  connectLive();
  bump(); // one refetch pass over whatever changed while we were away
  void drainOutbox(); // then the ordered replay; it bumps again if it applied writes
});

setAfterDrain(bump);
