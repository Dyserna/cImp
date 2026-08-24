// The Graph view's Canvas 2D renderer (#130, F5) — lifted verbatim out of
// `GraphView.svelte`'s `── Rendering ──` block, along with the chart palette
// it paints from and the three visibility predicates it shares with picking.
//
// It draws three layers, back to front: the directory discs and their
// aggregate links, the file edges, then the file nodes. `camera.project` must
// have run for this frame first — everything here reads `sx`/`sy`/`sr`/`sz`.
//
// NON-REACTIVE, same as `sim.ts` and `camera.ts`: the scene is passed by
// reference and read, never copied; `drawDirLayer` writes `discR` back onto
// each cluster because picking needs the radius the last frame actually drew.

import type { DirCluster, GraphTuning, SimEdge, SimNode, SimWorld } from './sim';
import { clamp } from './sim';

// ── Chart-specific colors (not app design tokens — Okabe-Ito categorical
// palette, colorblind-safe; the edge dash pattern carries a redundant
// non-hue channel alongside these). ─────────────────────────────────────
export const SUBSYSTEM_PALETTE = [
  '#E69F00', '#56B4E9', '#009E73', '#F0E442',
  '#0072B2', '#D55E00', '#CC79A7', '#999999',
];
export const UNCATEGORIZED_COLOR = '#8a93a3';
export const EDGE_CALL = '#4fb3ff';
export const EDGE_IMPORT = '#ff8a3d';
export const EDGE_CONTAINS = '#5fd38d';
export const EDGE_OTHER = '#9aa3b2';
export const PULSE_CLOUD = '#7fd4ff';
export const PULSE_LOCAL = '#ffb454';
export const PULSE_ADVISOR = '#d2a8ff';
export const ACCENT_COLOR = '#bb55ff';
export const PULSE_MS = 600;
export const FOCUS_MS = 900;

export function hashStr(s: string): number {
  let h = 0;
  for (let i = 0; i < s.length; i++) h = (h * 31 + s.charCodeAt(i)) | 0;
  return Math.abs(h);
}

export function subsystemColor(name: string): string {
  return SUBSYSTEM_PALETTE[hashStr(name) % SUBSYSTEM_PALETTE.length];
}

export function dashFor(confidence: string): number[] {
  if (confidence === 'inferred') return [6, 4];
  if (confidence === 'ambiguous') return [1, 3];
  return [];
}

export function basename(p: string): string {
  const i = p.lastIndexOf('/');
  return i >= 0 ? p.slice(i + 1) : p;
}

/// Everything a frame needs beyond the world itself: the indexes the renderer
/// looks things up in, the current selection/hover, and the live colour knobs.
///
/// Held by reference. `edgeCallColor` / `edgeImportColor` are read per frame
/// because the settings subscription can change them mid-session.
export interface Scene {
  world: SimWorld;
  nodeById: Map<string, SimNode>;
  edgesByNode: Map<string, SimEdge[]>;
  clusterByDir: Map<string, DirCluster>;
  selectedNodeId: string | null;
  selectedDir: string | null;
  hoveredNode: SimNode | null;
  /// When the just-selected expanding ring stops (epoch of `performance.now`).
  focusRingUntil: number;
  edgeCallColor: string;
  edgeImportColor: string;
  tune: GraphTuning;
}

/// The stroke colour for one edge kind. Takes the two live colours rather than
/// the whole scene so the component's legend and connections list can call it
/// straight off their `$state` — a scene field would be a copy, and the legend
/// would stop repainting when someone changes the colour in Settings.
export function edgeColor(kind: string, callColor: string, importColor: string): string {
  if (kind === 'call') return callColor;
  if (kind === 'import') return importColor;
  if (kind === 'contains') return EDGE_CONTAINS;
  return EDGE_OTHER;
}

// The selected node's 1-hop neighborhood (via ALL incident edges, drawn or
// not). Null when no node is selected. With a selection active, everything
// outside this set is HIDDEN, not dimmed.
export function currentEgoIds(scene: Scene): Set<string> | null {
  const id = scene.selectedNodeId;
  if (!id || !scene.nodeById.has(id)) return null;
  const ego = new Set([id]);
  for (const e of scene.edgesByNode.get(id) ?? []) {
    ego.add(e.src.id);
    ego.add(e.dst.id);
  }
  return ego;
}

// Which nodes render/pick at all: node selection → its ego set; directory
// focus → that directory's members; otherwise everything.
export function visibleNodeIds(scene: Scene): Set<string> | null {
  const ego = currentEgoIds(scene);
  if (ego) return ego;
  if (scene.selectedDir) {
    const c = scene.clusterByDir.get(scene.selectedDir);
    if (c) return new Set(c.members.map((m) => m.id));
  }
  return null;
}

// Which directory discs render/pick: ego dirs, the focused dir, or all.
export function visibleDirNames(scene: Scene): Set<string> | null {
  const ego = currentEgoIds(scene);
  if (ego) {
    const dirs = new Set<string>();
    for (const id of ego) {
      const n = scene.nodeById.get(id);
      if (n) dirs.add(n.dir);
    }
    return dirs;
  }
  if (scene.selectedDir) return new Set([scene.selectedDir]);
  return null;
}

// Directory layer: translucent discs around each cluster, one aggregate
// edge per connected directory pair (thicker = more file links), labels.
// Drawn beneath the file edges/nodes. With a selection active only the
// directories that contain ego nodes keep their (dimmed) discs — the
// aggregate edges disappear in favor of the explicit routed links.
export function drawDirLayer(ctx: CanvasRenderingContext2D, scene: Scene): void {
  const { clusters, dirEdges } = scene.world;
  const egoDirs = visibleDirNames(scene);
  for (const c of clusters) {
    if (c.sz < 0) { c.discR = 0; continue; }
    let r = 10;
    for (const m of c.members) {
      if (m.sz < 0) continue;
      const d = Math.hypot(m.sx - c.sx, m.sy - c.sy) + m.sr;
      if (d > r) r = d;
    }
    c.discR = r + 6;
  }
  if (!egoDirs) {
    ctx.setLineDash([]);
    for (const de of dirEdges) {
      if (de.a.sz < 0 || de.b.sz < 0) continue;
      ctx.beginPath();
      ctx.moveTo(de.a.sx, de.a.sy);
      ctx.lineTo(de.b.sx, de.b.sy);
      ctx.lineWidth = Math.min(5, 1 + Math.log2(1 + de.weight)) * scene.tune.edgeWidth;
      ctx.strokeStyle = de.callW >= de.importW ? scene.edgeCallColor : scene.edgeImportColor;
      ctx.globalAlpha = 0.3;
      ctx.stroke();
    }
  }
  ctx.lineWidth = 1;
  ctx.font = '10px system-ui, sans-serif';
  ctx.textAlign = 'center';
  for (const c of clusters) {
    if (c.sz < 0) continue;
    if (egoDirs && !egoDirs.has(c.name)) continue;
    ctx.beginPath();
    ctx.arc(c.sx, c.sy, c.discR, 0, Math.PI * 2);
    ctx.globalAlpha = egoDirs ? 0.04 : 0.06;
    ctx.fillStyle = '#8fa4c0';
    ctx.fill();
    ctx.globalAlpha = egoDirs ? 0.1 : 0.16;
    ctx.strokeStyle = '#8fa4c0';
    ctx.stroke();
    const label = c.name.length > 26 ? '…' + c.name.slice(-25) : c.name;
    ctx.globalAlpha = egoDirs ? 0.4 : 0.6;
    ctx.fillStyle = '#dde2eb';
    ctx.fillText(label, c.sx, c.sy - c.discR - 4);
  }
  ctx.globalAlpha = 1;
}

// Stroke one file edge; in cluster mode a cross-directory edge is routed
// through the two directory anchors (file → dirA → dirB → file), so the
// selected file's remote links visibly travel the directory connection.
function strokeEdgePath(ctx: CanvasRenderingContext2D, scene: Scene, e: SimEdge): void {
  ctx.beginPath();
  ctx.moveTo(e.src.sx, e.src.sy);
  if (!e.intra) {
    const ca = scene.clusterByDir.get(e.src.dir);
    const cb = scene.clusterByDir.get(e.dst.dir);
    if (ca && ca.sz >= 0) ctx.lineTo(ca.sx, ca.sy);
    if (cb && cb.sz >= 0) ctx.lineTo(cb.sx, cb.sy);
  }
  ctx.lineTo(e.dst.sx, e.dst.sy);
  ctx.stroke();
}

export function drawEdges(ctx: CanvasRenderingContext2D, scene: Scene, now: number): void {
  // Batch by style: one beginPath/setLineDash/stroke PER (kind, confidence)
  // GROUP (≤ ~9 of them), not per edge — per-edge strokes made frame cost
  // O(edges) canvas state changes and pinned the webview thread on large
  // snapshots. Highlighted edges are few; they draw individually on top.
  // The hovered/selected node's own edges draw bright on top of the dimmed
  // field, so connectivity is traceable by pointing at (or selecting) a
  // node. An active selection dims the rest of the field even further.
  const selNode = scene.selectedNodeId
    ? (scene.nodeById.get(scene.selectedNodeId) ?? null)
    : null;
  const selDir = !selNode ? scene.selectedDir : null;
  const hovNode = scene.hoveredNode;
  const groups = new Map<string, SimEdge[]>();
  const highlighted: SimEdge[] = [];
  const emphasized: SimEdge[] = [];
  for (const e of scene.world.edges) {
    // Skip edges with a near-plane-culled endpoint.
    if (e.src.sz < 0 || e.dst.sz < 0) continue;
    if (e.highlightUntil > now) {
      highlighted.push(e);
      continue;
    }
    // With a node selection, emphasis belongs to the selected node only —
    // hover-emphasizing a neighbor would draw bright lines to endpoints
    // that the ego view hides. Under directory focus, hover emphasis is
    // limited to fully-internal edges for the same reason.
    if (
      selNode
        ? e.src === selNode || e.dst === selNode
        : hovNode &&
          (e.src === hovNode || e.dst === hovNode) &&
          (!selDir || (e.src.dir === selDir && e.dst.dir === selDir))
    ) {
      emphasized.push(e);
      continue;
    }
    // With a node selection the ambient field is hidden entirely — only
    // the selected node's own (emphasized) edges remain.
    if (selNode) continue;
    if (selDir) {
      // Directory focus: ALL of the directory's internal edges (drawn or
      // over-quota), nothing else.
      if (e.src.dir !== selDir || e.dst.dir !== selDir) continue;
    } else {
      // Over-quota edges are list/highlight-only, never ambient lines,
      // and ambient cross-directory links are represented by the
      // aggregate directory edge, not drawn individually.
      if (!e.drawn || !e.intra) continue;
    }
    const key = e.kind + '|' + e.confidence;
    let g = groups.get(key);
    if (!g) groups.set(key, (g = []));
    g.push(e);
  }
  ctx.globalAlpha = selDir ? 0.4 : 0.16;
  ctx.lineWidth = scene.tune.edgeWidth;
  for (const g of groups.values()) {
    ctx.beginPath();
    for (const e of g) {
      ctx.moveTo(e.src.sx, e.src.sy);
      ctx.lineTo(e.dst.sx, e.dst.sy);
    }
    // The dash pattern restarts at each moveTo subpath, so one shared path
    // renders identically to the old per-edge strokes.
    ctx.setLineDash(dashFor(g[0].confidence));
    ctx.strokeStyle = edgeColor(g[0].kind, scene.edgeCallColor, scene.edgeImportColor);
    ctx.stroke();
  }
  ctx.globalAlpha = 0.85;
  ctx.lineWidth = 1.5 * scene.tune.edgeWidth;
  for (const e of emphasized) {
    ctx.setLineDash(dashFor(e.confidence));
    ctx.strokeStyle = edgeColor(e.kind, scene.edgeCallColor, scene.edgeImportColor);
    strokeEdgePath(ctx, scene, e);
  }
  ctx.globalAlpha = 0.95;
  ctx.lineWidth = 2.5 * scene.tune.edgeWidth;
  for (const e of highlighted) {
    ctx.setLineDash(dashFor(e.confidence));
    ctx.strokeStyle =
      e.highlightColor ?? edgeColor(e.kind, scene.edgeCallColor, scene.edgeImportColor);
    strokeEdgePath(ctx, scene, e);
  }
  ctx.setLineDash([]);
  ctx.globalAlpha = 1;
}

export function drawNodes(ctx: CanvasRenderingContext2D, scene: Scene, now: number): void {
  // With a node selection only the ego set renders; under directory focus
  // only that directory's members do.
  const egoIds = visibleNodeIds(scene);
  const order = [...scene.world.nodes].sort((a, b) => b.sz - a.sz);
  for (const n of order) {
    if (n.sz < 0) continue;
    if (egoIds !== null && !egoIds.has(n.id)) continue;
    ctx.fillStyle = n.subsystem ? subsystemColor(n.subsystem) : UNCATEGORIZED_COLOR;
    ctx.globalAlpha = 1;
    ctx.beginPath();
    ctx.arc(n.sx, n.sy, n.sr, 0, Math.PI * 2);
    ctx.fill();
    const isHovered = scene.hoveredNode?.id === n.id;
    ctx.lineWidth = isHovered ? 2 : 1;
    ctx.strokeStyle = isHovered ? '#ffffff' : 'rgba(10,12,16,0.55)';
    ctx.stroke();

    if (n.pulseUntil > now) {
      const alpha = clamp((n.pulseUntil - now) / PULSE_MS, 0, 1);
      ctx.save();
      ctx.globalAlpha = alpha * 0.9;
      ctx.strokeStyle = n.pulseColor ?? PULSE_CLOUD;
      ctx.lineWidth = 3;
      ctx.beginPath();
      ctx.arc(n.sx, n.sy, n.sr + 4 + (1 - alpha) * 12, 0, Math.PI * 2);
      ctx.stroke();
      ctx.restore();
    }
    if (n.id === scene.selectedNodeId) {
      // Persistent selection ring, plus the expanding just-selected pulse.
      ctx.save();
      ctx.globalAlpha = 0.9;
      ctx.strokeStyle = ACCENT_COLOR;
      ctx.lineWidth = 2;
      ctx.beginPath();
      ctx.arc(n.sx, n.sy, n.sr + 3, 0, Math.PI * 2);
      ctx.stroke();
      if (scene.focusRingUntil > now) {
        const alpha = clamp((scene.focusRingUntil - now) / FOCUS_MS, 0, 1);
        ctx.globalAlpha = alpha * 0.8;
        ctx.beginPath();
        ctx.arc(n.sx, n.sy, n.sr + 6 + (1 - alpha) * 16, 0, Math.PI * 2);
        ctx.stroke();
      }
      ctx.restore();
    }
  }
  ctx.globalAlpha = 1;
}

/// One frame's worth of drawing, in the order the layers stack. The caller
/// projects and clears first — it owns the canvas and the rAF loop.
export function drawScene(ctx: CanvasRenderingContext2D, scene: Scene, now: number): void {
  drawDirLayer(ctx, scene);
  drawEdges(ctx, scene, now);
  drawNodes(ctx, scene, now);
}
