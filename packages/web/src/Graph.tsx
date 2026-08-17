import { createEffect, createResource, onCleanup, onMount, For, type Component } from "solid-js";
import ForceGraph, { type LinkObject, type NodeObject } from "force-graph";
import type { GraphNode } from "@hive/shared";
import { api } from "./api.ts";
import { liveRev } from "./live.ts";
import { KIND, resolveColor } from "./kinds.ts";

/** Our node payload plus the simulation's own fields. x/y stay optional: the
 *  layout owns them and hasn't placed anything before the first tick. */
type SimNode = NodeObject & GraphNode;
/** force-graph rewrites source/target into node objects, so only `rel` (which
 *  we set and it never touches) is ours to read. */
type SimLink = LinkObject<SimNode> & { rel: string };

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
  let fg: ForceGraph<SimNode, SimLink> | undefined;

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
    // Annotated so the onNodeClick closure can name it without the initializer
    // referring to its own inferred type.
    const graph: ForceGraph<SimNode, SimLink> = new ForceGraph<SimNode, SimLink>(host)
      .backgroundColor(bg)
      .nodeRelSize(5)
      // Chain edges (per-author journal timeline) are subtler than entity links.
      .linkColor((l) => (l.rel === "chain" ? withAlpha(dim, 0.18) : withAlpha(dim, 0.22)))
      .linkWidth((l) => l.rel === "chain" ? 0.8 : 1.2)
      .linkDirectionalParticles(0)
      .nodeLabel((n) => `${n.kind} · ${n.title}`)
      .nodeCanvasObject((node, ctx, scale) => {
        // Unplaced node (added between ticks) ... there is no coordinate to paint at.
        if (node.x === undefined || node.y === undefined) return;
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
      })
      .onNodeClick((node) => {
        graph.centerAt(node.x, node.y, 600);
        graph.zoom(4, 600);
      })
      // Keep the simulation alive so the graph stays gently floaty + reacts to drags.
      .cooldownTime(Infinity)
      .d3VelocityDecay(0.28);
    fg = graph;

    // Spread things out a touch for the airy Obsidian look.
    graph.d3Force("charge")?.strength(-120);
    graph.d3Force("link")?.distance(46);

    const resize = () => graph.width(host.clientWidth).height(host.clientHeight);
    resize();
    window.addEventListener("resize", resize);
    onCleanup(() => {
      window.removeEventListener("resize", resize);
      graph._destructor();
    });
  });

  // Feed graph data once it loads (force-graph mutates node objects, so copy).
  let fitted = false;
  createEffect(() => {
    const g = data();
    const graph = fg;
    if (!g || !graph) return;
    graph.graphData({
      nodes: g.nodes.map((n) => ({ ...n })),
      links: g.edges.map((e) => ({ source: e.source, target: e.target, rel: e.rel })),
    });
    // Continuous simulation never auto-fits, so frame the graph once it spreads.
    if (!fitted) {
      fitted = true;
      setTimeout(() => graph.zoomToFit(800, 60), 1400);
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
