import { createEffect, createResource, onCleanup, onMount, For, type Component } from "solid-js";
import ForceGraph from "force-graph";
import { api } from "./api.ts";
import { liveRev } from "./live.ts";
import { KIND, resolveColor } from "./kinds.ts";

/** "#rrggbb" + alpha → "#rrggbbaa" (the :root tokens are authored as 6-digit hex). */
const withAlpha = (color: string, alpha: number): string =>
  color + Math.round(alpha * 255).toString(16).padStart(2, "0");

/**
 * Live force-directed knowledge graph — a continuously-simulating, draggable,
 * zoomable canvas (the floaty Obsidian feel) rendered on the GPU-composited
 * HTML5 canvas via force-graph. Labels fade in as you zoom; click a node to
 * focus it. Fed by /api/graph (journal entries + everything anchored from them).
 */
export const Graph: Component = () => {
  const [data] = createResource(() => ({ _r: liveRev() }), () => api.graph());
  let host!: HTMLDivElement;
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  let fg: any;

  // Canvas paint can't read var() strings, so resolve each kind's token from
  // the registry ONCE at mount into a plain map. Unknown kinds (custom entity
  // slugs can appear as graph nodes) fall back to honey.
  const nodeColor: Record<string, string> = Object.fromEntries(
    Object.values(KIND).map((k) => [k.slug, resolveColor(k.color)]),
  );
  const honey = resolveColor("honey");
  const bg = resolveColor("bg");
  const dim = resolveColor("dim");
  const ink = resolveColor("ink");

  onMount(() => {
    fg = new ForceGraph(host)
      .backgroundColor(bg)
      .nodeRelSize(5)
      // Chain edges (per-author journal timeline) are subtler than entity links.
      .linkColor((l: { rel?: string }) => (l.rel === "chain" ? withAlpha(dim, 0.18) : withAlpha(dim, 0.22)))
      .linkWidth((l: { rel?: string }) => l.rel === "chain" ? 0.8 : 1.2)
      .linkDirectionalParticles(0)
      .nodeLabel((n: { kind: string; title: string }) => `${n.kind} · ${n.title}`)
      .nodeCanvasObject(
        (node: { x: number; y: number; kind: string; title: string }, ctx: CanvasRenderingContext2D, scale: number) => {
          const r = 4;
          ctx.beginPath();
          ctx.arc(node.x, node.y, r, 0, 2 * Math.PI);
          ctx.fillStyle = nodeColor[node.kind] ?? honey;
          ctx.fill();
          // Labels fade in once you've zoomed past the cluttered overview.
          if (scale > 1.3) {
            const label = node.title.length > 26 ? `${node.title.slice(0, 26)}…` : node.title;
            ctx.font = `${11 / scale}px ui-sans-serif, system-ui, sans-serif`;
            ctx.fillStyle = withAlpha(ink, 0.85);
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
          <For each={Object.values(KIND)}>
            {(k) => (
              <span class="lg">
                <i style={{ background: nodeColor[k.slug] }} />
                {k.slug}
              </span>
            )}
          </For>
        </div>
      </div>
      <div ref={host} class="graph-canvas" />
      <div class="dim sm">
        {data() && data()!.nodes.length === 0
          ? "no graph yet — journal entries and the things they link to appear here."
          : `${data() ? `${data()!.nodes.length} nodes · ${data()!.edges.length} edges · ` : ""}drag to pull · scroll to zoom · click a node to focus`}
      </div>
    </section>
  );
};
