import { createResource, createSignal, For, Show, type Component } from "solid-js";
import type { Flow } from "@hive/shared";
import { api, getCurrentUser } from "./api.ts";
import { liveRev } from "./live.ts";
import { EmptyState, SectionHead } from "./primitives.tsx";
import { Icon } from "./icons.tsx";

// The flow registry (docs/FLOWS.md): every installed flow with its declared
// operations and triggers. Each enabled flow's operations are served to MCP
// clients as flow_<slug>_<op> tools, so this page is the human view of the
// same registry agents see. F5 grows the per-flow visualization (run
// history, manifest-driven diagram); this lists and toggles.

export const Flows: Component = () => {
  const [flows, { refetch }] = createResource(
    () => ({ _r: liveRev() }),
    () => api.flows(),
  );
  const [err, setErr] = createSignal<string | null>(null);
  const isAdmin = getCurrentUser()?.role === "admin";

  const toggle = async (flow: Flow, enabled: boolean) => {
    setErr(null);
    try {
      await api.setFlowEnabled(flow.slug, enabled);
      await refetch();
    } catch (e) {
      setErr(e instanceof Error ? e.message : String(e));
    }
  };

  return (
    <section class="flows">
      <SectionHead title="Flows" icon="graph" count={(flows() ?? []).length} />
      <p class="dim sm">
        Pluggable workflows. An enabled flow's operations are served to connected AI clients as{" "}
        <code>flow_&lt;slug&gt;_&lt;op&gt;</code> MCP tools; disabling a flow withdraws them and keeps its
        data.
      </p>
      <Show when={err()}>
        <p class="error sm">{err()}</p>
      </Show>
      <For
        each={flows() ?? []}
        fallback={
          <EmptyState
            icon="graph"
            title="No flows registered."
            hint="The builtin wire flow seeds itself on first use; wasm flows arrive with the flow-exec host."
          />
        }
      >
        {(f) => (
          <div class="source-row" classList={{ off: !f.enabled }}>
            <Show when={isAdmin}>
              <label class="sw">
                <input
                  type="checkbox"
                  checked={f.enabled}
                  onChange={(e) => void toggle(f, e.currentTarget.checked)}
                />
              </label>
            </Show>
            <div class="source-main">
              <div class="source-name">
                <Icon name={f.kind === "builtin" ? "wire" : "hex"} size={14} />
                {f.name}
                <span class="badge">{f.kind}</span>
                <span class="badge dim">v{f.version}</span>
                <span class="badge dim">{f.slug}</span>
                <Show when={!f.enabled}>
                  <span class="badge">disabled</span>
                </Show>
              </div>
              <div class="dim sm">{f.description}</div>
              <Show when={f.manifest.operations.length > 0}>
                <div class="phases">
                  <For each={f.manifest.operations}>
                    {(op) => (
                      <span class="phase-chip" title={op.description}>
                        <code>
                          flow_{f.slug}_{op.name}
                        </code>
                        <Show when={op.admin}> · admin</Show>
                      </span>
                    )}
                  </For>
                </div>
              </Show>
              <Show when={f.manifest.triggers.length > 0}>
                <div class="dim sm">
                  triggers: {f.manifest.triggers.map((t) => t.kind).join(", ")}
                </div>
              </Show>
            </div>
          </div>
        )}
      </For>
    </section>
  );
};
