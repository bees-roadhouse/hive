import { createMemo, createResource, createSignal, For, Show, type Component } from "solid-js";
import type { GraphData } from "@hive/shared";
import { api } from "./api.ts";

const KIND_COLOR: Record<string, string> = {
  journal: "#7c9cff",
  task: "#5ec8a0",
  decision: "#e0a44a",
  event: "#c77dff",
  note: "#9aa0a6",
};
const W = 920;
const H = 620;

type Pt = { x: number; y: number; vx: number; vy: number };

/** A tiny dependency-free force layout: Coulomb repulsion between all nodes,
 * Hooke springs along edges, a weak pull to center. Runs once per dataset
 * (sub-10ms for the hive's small graphs) and returns final positions. */
function layout(g: GraphData): Map<string, Pt> {
  const pos = new Map<string, Pt>();
  const n = g.nodes.length;
  g.nodes.forEach((node, i) => {
    const a = (2 * Math.PI * i) / Math.max(n, 1);
    pos.set(node.id, {
      x: W / 2 + Math.cos(a) * Math.min(W, H) * 0.34,
      y: H / 2 + Math.sin(a) * Math.min(W, H) * 0.34,
      vx: 0,
      vy: 0,
    });
  });
  for (let iter = 0; iter < 300; iter++) {
    for (let i = 0; i < n; i++) {
      for (let j = i + 1; j < n; j++) {
        const a = pos.get(g.nodes[i].id)!;
        const b = pos.get(g.nodes[j].id)!;
        let dx = a.x - b.x;
        let dy = a.y - b.y;
        const d2 = dx * dx + dy * dy || 0.01;
        const d = Math.sqrt(d2);
        const f = 2600 / d2;
        dx = (dx / d) * f;
        dy = (dy / d) * f;
        a.vx += dx;
        a.vy += dy;
        b.vx -= dx;
        b.vy -= dy;
      }
    }
    for (const e of g.edges) {
      const a = pos.get(e.source);
      const b = pos.get(e.target);
      if (!a || !b) continue;
      const dx = b.x - a.x;
      const dy = b.y - a.y;
      const d = Math.sqrt(dx * dx + dy * dy) || 0.01;
      const f = (d - 96) * 0.02;
      const fx = (dx / d) * f;
      const fy = (dy / d) * f;
      a.vx += fx;
      a.vy += fy;
      b.vx -= fx;
      b.vy -= fy;
    }
    for (const node of g.nodes) {
      const p = pos.get(node.id)!;
      p.vx += (W / 2 - p.x) * 0.001;
      p.vy += (H / 2 - p.y) * 0.001;
      p.x += Math.max(-12, Math.min(12, p.vx));
      p.y += Math.max(-12, Math.min(12, p.vy));
      p.vx *= 0.85;
      p.vy *= 0.85;
    }
  }
  return pos;
}

/** Knowledge graph: every linked entity (journal entries and the tasks/
 * decisions/events anchored from them, plus supersedes edges) as a node-link
 * diagram. Click a node to focus its neighborhood. */
export const Graph: Component = () => {
  const [data] = createResource(() => api.graph());
  const [sel, setSel] = createSignal<string | null>(null);

  const pos = createMemo(() => {
    const g = data();
    return g ? layout(g) : new Map<string, Pt>();
  });
  const neighbors = createMemo(() => {
    const g = data();
    const s = sel();
    if (!g || !s) return new Set<string>();
    const set = new Set<string>([s]);
    for (const e of g.edges) {
      if (e.source === s) set.add(e.target);
      if (e.target === s) set.add(e.source);
    }
    return set;
  });
  const clip = (t: string) => (t.length > 26 ? `${t.slice(0, 26)}…` : t);

  return (
    <section class="graph">
      <div class="graph-head">
        <h3 class="sec">Knowledge graph</h3>
        <div class="legend">
          <For each={Object.entries(KIND_COLOR)}>
            {([k, c]) => (
              <span class="lg">
                <i style={{ background: c }} />
                {k}
              </span>
            )}
          </For>
        </div>
      </div>

      <Show when={data()} fallback={<p class="dim sm">loading…</p>}>
        {(g) => (
          <Show
            when={g().nodes.length}
            fallback={<p class="dim sm">no links yet — anchor spans in the journal to grow the graph.</p>}
          >
            <svg class="graph-svg" viewBox={`0 0 ${W} ${H}`} onClick={() => setSel(null)}>
              <For each={g().edges}>
                {(e) => {
                  const a = () => pos().get(e.source);
                  const b = () => pos().get(e.target);
                  const faded = () => {
                    const ns = neighbors();
                    return ns.size > 0 && !(ns.has(e.source) && ns.has(e.target));
                  };
                  return (
                    <Show when={a() && b()}>
                      <line
                        class="edge"
                        classList={{ faded: faded() }}
                        x1={a()!.x}
                        y1={a()!.y}
                        x2={b()!.x}
                        y2={b()!.y}
                      />
                    </Show>
                  );
                }}
              </For>
              <For each={g().nodes}>
                {(node) => {
                  const p = () => pos().get(node.id);
                  const faded = () => {
                    const ns = neighbors();
                    return ns.size > 0 && !ns.has(node.id);
                  };
                  return (
                    <Show when={p()}>
                      <g
                        class="node"
                        classList={{ faded: faded(), sel: sel() === node.id }}
                        transform={`translate(${p()!.x},${p()!.y})`}
                        onClick={(ev) => {
                          ev.stopPropagation();
                          setSel(sel() === node.id ? null : node.id);
                        }}
                      >
                        <circle r={sel() === node.id ? 9 : 6} fill={KIND_COLOR[node.kind] ?? "#888"} />
                        <text x="11" y="4">{clip(node.title)}</text>
                      </g>
                    </Show>
                  );
                }}
              </For>
            </svg>
            <div class="dim sm">
              {g().nodes.length} nodes · {g().edges.length} edges ·{" "}
              {sel() ? "click background to clear" : "click a node to focus its neighborhood"}
            </div>
          </Show>
        )}
      </Show>
    </section>
  );
};
