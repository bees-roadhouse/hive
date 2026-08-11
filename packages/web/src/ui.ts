// Cross-cutting UI signals shared between the shell and pages, so the command
// palette and sidebar can drive page-level actions (e.g. "new entry") without
// prop-drilling through the router.

import { createSignal } from "solid-js";

// Command palette visibility (⌘K / Ctrl+K). Owned here so both the shell's
// keyboard handler and the sidebar hint button can toggle it.
const [paletteOpen, setPaletteOpen] = createSignal(false);
export { paletteOpen, setPaletteOpen };

// Monotonic counter: bump to ask the journal to open its composer. Pages
// listen with `on(composeReq, …, { defer: true })` so mounting never
// auto-opens it.
const [composeReq, setComposeReq] = createSignal(0);
export { composeReq };

// The palette can fire "new entry" from another route; the bump then lands
// before the journal mounts and its deferred listener exists. Latch the
// request so the journal can also consume it at mount time.
let composePending = false;
export const requestCompose = () => {
  composePending = true;
  setComposeReq((n) => n + 1);
};
export const consumeComposeRequest = (): boolean => {
  const was = composePending;
  composePending = false;
  return was;
};
