import { createEffect, createResource, onCleanup, onMount, For, type Component } from "solid-js";
import ForceGraph from "force-graph";
import { api } from "./api.ts";

// Kind → node color.
const KIND_COLOR: Record<string, string> = {
  journal: "#7c9cff",
  task: "#5ec8a0",
  decision: "#e0a44a",
  event: "#c77dff",
  note: "#9aa0a6",
  person: "#ff8fab",
  topic: "#6ee7d6",
  project: "#ffd24a",
  phase: "#ffb86b",
};

/**
 * Live force-directed knowledge graph — a continuously-simulating, draggable,
 * zoomable canvas (the floaty Obsidian feel) rendered on the GPU-composited
 * HTML5 canvas via force-graph. Labels fade in as you zoom; click a node to
 * focus it. Fed by /api/graph (journal entries + everything anchored from them).
 */
export const Graph: Component = () => {
  const [data] = createResource(() => api.graph());
  let host!: HTMLDivElement;
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  let fg: any;

  onMount(() => {
    fg = new ForceGraph(host)
      .backgroundColor("#14110c")
      .nodeRelSize(5)
      // Chain edges (per-author journal timeline) are subtle/dashed; entity links are warmer.
      .linkColor((l: { rel?: string }) => l.rel === "chain" ? "rgba(124,156,255,0.18)" : "rgba(169,155,120,0.22)")
      .linkWidth((l: { rel?: string }) => l.rel === "chain" ? 0.8 : 1.2)
      .linkDirectionalParticles(0)
      .nodeLabel((n: { kind: string; title: string }) => `${n.kind} · ${n.title}`)
      .nodeCanvasObject(
        (node: { x: number; y: number; kind: string; title: string }, ctx: CanvasRenderingContext2D, scale: number) => {
          const r = 4;
          ctx.beginPath();
          ctx.arc(node.x, node.y, r, 0, 2 * Math.PI);
          ctx.fillStyle = KIND_COLOR[node.kind] ?? "#888";
          ctx.fill();
          // Labels fade in once you've zoomed past the cluttered overview.
          if (scale > 1.3) {
            const label = node.title.length > 26 ? `${node.title.slice(0, 26)}…` : node.title;
            ctx.font = `${11 / scale}px ui-sans-serif, system-ui, sans-serif`;
            ctx.fillStyle = "rgba(243,234,214,0.85)";
            ctx.textAlign = "left";
            ctx.textBaseline = "middle";
            ctx.fillText(label, node.x + r + 2 / scale, node.y);
          }
        },
      )
      .onNodeClick((node: { x: number; y: number }) => {
        fg.centerAt(node.x, node.y, 600);
        fg.zoom(4, 600);
      })
      // Keep the simulation alive so the graph stays gently floaty + reacts to drags.
      .cooldownTime(Infinity)
      .d3VelocityDecay(0.28);

    // Spread things out a touch for the airy Obsidian look.
    fg.d3Force("charge")?.strength(-120);
    fg.d3Force("link")?.distance(46);

    const resize = () => fg.width(host.clientWidth).height(host.clientHeight);
    resize();
    window.addEventListener("resize", resize);
    onCleanup(() => {
      window.removeEventListener("resize", resize);
      fg._destructor?.();
    });
  });

  // Feed graph data once it loads (force-graph mutates node objects, so copy).
  let fitted = false;
  createEffect(() => {
    const g = data();
    if (!g || !fg) return;
    fg.graphData({
      nodes: g.nodes.map((n) => ({ ...n })),
      links: g.edges.map((e) => ({ source: e.source, target: e.target, rel: e.rel })),
    });
    // Continuous simulation never auto-fits, so frame the graph once it spreads.
    if (!fitted) {
      fitted = true;
      setTimeout(() => fg.zoomToFit(800, 60), 1400);
    }
  });

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
      <div ref={host} class="graph-canvas" />
      <div class="dim sm">
        {data() ? `${data()!.nodes.length} nodes · ${data()!.edges.length} edges · ` : ""}
        drag to pull · scroll to zoom · click a node to focus
      </div>
    </section>
  );
};
