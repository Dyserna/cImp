// The Graph view's force simulation — the layout half of the 3D code-graph
// visualization, lifted verbatim out of `GraphView.svelte` (#130, F5).
//
// (`src/lib/graph.ts`, the file next door, is the code graph's IPC layer.
// `src/lib/graph/` is `GraphView.svelte`'s ENGINE: this file, `camera.ts` and
// `render.ts`. Two different `graph` things, deliberately kept apart — nothing
// here calls the backend, and nothing there knows a node has a velocity.)
//
// WHY IT MOVED. It was ~180 lines of banner-delimited, DOM-free arithmetic
// inside a 2,269-line component, which made it the one piece of this view
// nobody could test: `graph.test.ts` covered the IPC layer and stopped there.
// Out here a settle is a function call.
//
// THE INVARIANT THIS FILE EXISTS TO PRESERVE. Every array below is mutated at
// animation-frame rate — `simulate` writes nine floats per node per frame — and
// none of it is Svelte state. That is deliberate, and it is documented in the
// component at the `SimWorld` it owns: routing frame-rate mutation through
// `$state` would put a reactivity notification on each of those writes. So:
//
//   * `simulate` takes the world BY REFERENCE and mutates it in place. It
//     allocates nothing per frame and returns a boolean, not a new state;
//   * the caller keeps ONE `SimWorld` object for the component's lifetime
//     (`buildSim` replaces its arrays, it does not replace the object);
//   * nothing in this file may be wrapped in `$state` by a caller. Reading a
//     proxied node in the frame loop is the regression to watch for.
//
// `alpha` lives on the world rather than in the caller because `simulate` both
// reads and decays it; the caller re-heats it (`kick`) by assignment.

import type { VizNodeRow } from '../graph';

/// One file, as the layout sees it: the backend row plus position, velocity,
/// accumulated force, world radius, its projected screen values (written by
/// `camera.project`), its pulse state (read by `render`), and its directory.
export interface SimNode extends VizNodeRow {
  x: number; y: number; z: number;
  vx: number; vy: number; vz: number;
  fx: number; fy: number; fz: number;
  r: number;
  sx: number; sy: number; sr: number; sz: number;
  pulseUntil: number; pulseColor: string | null;
  dir: string;
}

export interface SimEdge {
  src: SimNode; dst: SimNode;
  kind: string; confidence: string;
  highlightUntil: number; highlightColor: string | null;
  intra: boolean; // both endpoints in the same directory
  // Over-quota edges arrive with drawn=false: they feed the connections
  // panel, dir-edge weights, and the selection highlight, but are neither
  // simulated as springs nor drawn as ambient lines.
  drawn: boolean;
}

export interface DirCluster {
  name: string;
  members: SimNode[];
  leash: number; // rest length of the member↔anchor spring
  x: number; y: number; z: number;
  vx: number; vy: number; vz: number;
  fx: number; fy: number; fz: number;
  sx: number; sy: number; sz: number;
  discR: number; // projected disc radius (computed at render)
}

export interface DirEdge {
  a: DirCluster; b: DirCluster;
  weight: number; callW: number; importW: number;
}

/// Everything the frame loop mutates, in one object passed by reference.
///
/// The arrays are REPLACED by a rebuild and mutated in place by every frame in
/// between. Hold one of these for the view's lifetime; do not rebuild the
/// wrapper per frame, and do not put it in `$state`.
export interface SimWorld {
  nodes: SimNode[];
  edges: SimEdge[];
  clusters: DirCluster[];
  dirEdges: DirEdge[];
  /// Force-cooling factor (d3-style). `simulate` decays it; the caller raises
  /// it to re-heat the layout after a data or layout change.
  alpha: number;
}

/// The settings-driven multipliers (Settings → Code Intelligence → Graph view →
/// Graph view tuning). Read at frame rate, so the caller keeps them in a plain
/// object, not in `$state`. `edgeWidth` is render-only and is here so one type
/// describes the whole knob set.
export interface GraphTuning {
  nodeScale: number;
  dirScale: number;
  edgeWidth: number;
  nodeSpacing: number;
  clusterSpacing: number;
  clusterStrength: number;
}

/// The tuning a fresh view starts from — every knob neutral.
export function defaultTuning(): GraphTuning {
  return {
    nodeScale: 1,
    dirScale: 1,
    edgeWidth: 1,
    nodeSpacing: 1,
    clusterSpacing: 1,
    clusterStrength: 1,
  };
}

/// An empty world. One per view; `buildSim` replaces the arrays in place.
export function emptyWorld(): SimWorld {
  return { nodes: [], edges: [], clusters: [], dirEdges: [], alpha: 0 };
}

// Physics tuning. O(n^2) repulsion is fine up to a few hundred nodes (the
// backend caps the snapshot: `graph_viz_max_nodes` nodes, ~4 edges/node);
// above MAX_REPEL_FULL we sample a bounded number of partners per node per
// frame instead of every pair, so a large graph never blows the frame
// budget.
export const REPULSION_K = 1600;
export const SPRING_K = 0.05;
export const SPRING_LEN = 45;
export const GRAVITY_K = 0.0025;
export const DAMPING = 0.85;
// Per-frame speed cap (world units). Even with degree-normalized springs
// (see applySpring) a hot start can make the integrator overshoot; the cap
// guarantees no node gets flung out of the cluster in a single frame.
export const MAX_VEL = 40;
export const IDLE_KE_THRESHOLD = 0.05;
export const MAX_REPEL_FULL = 500;
export const REPEL_SAMPLE = 40;
export const REPEL_CUTOFF_SQ = 90000; // 300 world units
// Directory clustering (the only layout): files leash to an invisible
// per-directory anchor, anchors repel hard and spring together only where
// cross-dir file edges exist, and cross-dir file edges render as ONE
// aggregate edge per directory pair (per-file cross links only appear
// routed through the anchors when a node is selected).
export const DIR_MEMBER_K = 0.06; // member ↔ anchor leash spring
export const DIR_EDGE_K = 0.015; // anchor ↔ anchor aggregate spring
export const DIR_EDGE_LEN = 420;
export const DIR_REPULSION_K = 90000;
export const DIR_REPEL_CUTOFF_SQ = 1440000; // 1200 world units

// Cooling: forces are scaled by an exponentially decaying alpha (d3-style)
// so the layout always converges, even under sampled-repulsion noise
// (n > MAX_REPEL_FULL) whose kinetic energy alone never drops below
// IDLE_KE_THRESHOLD. SIM_MAX_MS stays as a hard backstop so a stuck sim
// can never pin the webview thread indefinitely.
export const ALPHA_DECAY = 0.02; // per ~16.7ms frame
export const ALPHA_MIN = 0.02; // below this the layout counts as settled
export const SIM_MAX_MS = 10_000;

export function clamp(v: number, lo: number, hi: number): number {
  return Math.min(hi, Math.max(lo, v));
}

export function dirOf(file: string): string {
  const i = file.lastIndexOf('/');
  return i >= 0 ? file.slice(0, i) : '(root)';
}

// Log-scaled and small: rolled-up file degrees span 1..hundreds, and a
// sqrt scale ballooned every hub into an edge-hiding blob. The settings
// knob multiplies the whole curve.
export function nodeRadius(deg: number, nodeScale: number): number {
  return clamp(2 + Math.log2(1 + deg) * 0.8, 2, 7) * nodeScale;
}

// Rest length of the member↔anchor leash — the directory cluster's size.
export function leashFor(memberCount: number, dirScale: number): number {
  return (12 + Math.sqrt(memberCount) * 6) * dirScale;
}

/// Fibonacci-spiral seeding. `rand` is injectable ONLY so a test can settle a
/// layout deterministically; every caller in the app takes the default.
export function initPosition(
  index: number,
  total: number,
  rand: () => number = Math.random,
): { x: number; y: number; z: number } {
  const golden = Math.PI * (3 - Math.sqrt(5));
  const theta = index * golden;
  const r = Math.sqrt(index + 0.5) * 8 * Math.max(1, Math.sqrt(total) / 10);
  const x = Math.cos(theta) * r;
  const y = Math.sin(theta) * r;
  const z = (rand() - 0.5) * r;
  return { x, y, z };
}

export function applyRepulsion(a: SimNode, b: SimNode, weight: number, nodeSpacing: number): void {
  const dx = a.x - b.x;
  const dy = a.y - b.y;
  const dz = a.z - b.z;
  const distSq = dx * dx + dy * dy + dz * dz + 0.01;
  // Spacing scales the repulsion (and its cutoff) quadratically so the
  // node↔node equilibrium distance scales ~linearly with the knob.
  const sp2 = nodeSpacing * nodeSpacing;
  if (distSq > REPEL_CUTOFF_SQ * sp2) return;
  const dist = Math.sqrt(distSq);
  const f = (REPULSION_K * sp2 * weight) / distSq;
  const fx = (dx / dist) * f;
  const fy = (dy / dist) * f;
  const fz = (dz / dist) * f;
  a.fx += fx; a.fy += fy; a.fz += fz;
  b.fx -= fx; b.fy -= fy; b.fz -= fz;
}

export function applySpring(e: SimEdge, nodeSpacing: number): void {
  const a = e.src, b = e.dst;
  const dx = b.x - a.x;
  const dy = b.y - a.y;
  const dz = b.z - a.z;
  const dist = Math.sqrt(dx * dx + dy * dy + dz * dz) || 0.01;
  // d3-force-style degree normalization: an edge's strength is divided by
  // its smaller endpoint degree and the displacement is biased toward the
  // lighter endpoint. Without this a hub accumulates the stiffness of ALL
  // its springs — the integrator overshoots, the hub oscillates outward
  // and freezes stranded far from the cluster with its edges fanning back
  // as a cone. This way leaves come to the hub, not the reverse.
  const degA = Math.max(1, a.degree || 1);
  const degB = Math.max(1, b.degree || 1);
  const f = (SPRING_K / Math.min(degA, degB)) * (dist - SPRING_LEN * nodeSpacing);
  const biasA = degB / (degA + degB);
  const fx = (dx / dist) * f;
  const fy = (dy / dist) * f;
  const fz = (dz / dist) * f;
  a.fx += fx * biasA; a.fy += fy * biasA; a.fz += fz * biasA;
  b.fx -= fx * (1 - biasA); b.fy -= fy * (1 - biasA); b.fz -= fz * (1 - biasA);
}

/// One integration step over `w`, in place. Returns whether the layout is
/// still moving — false means it has settled and the caller may stop stepping.
///
/// `rand` is the sampled-repulsion partner picker, injectable ONLY for tests.
export function simulate(
  w: SimWorld,
  tune: GraphTuning,
  dtMs: number,
  rand: () => number = Math.random,
): boolean {
  const { nodes, edges, clusters, dirEdges } = w;
  const n = nodes.length;
  if (n === 0) return false;
  const dts = clamp(dtMs, 1, 32) / 16.67;
  for (const node of nodes) { node.fx = 0; node.fy = 0; node.fz = 0; }

  if (n <= MAX_REPEL_FULL) {
    for (let i = 0; i < n; i++) {
      const a = nodes[i];
      for (let j = i + 1; j < n; j++) applyRepulsion(a, nodes[j], 1, tune.nodeSpacing);
    }
  } else {
    for (let i = 0; i < n; i++) {
      const a = nodes[i];
      for (let s = 0; s < REPEL_SAMPLE; s++) {
        const j = (rand() * n) | 0;
        if (j === i) continue;
        // Halved weight: applyRepulsion pushes BOTH endpoints, and every
        // node is drawn both as `a` and (in expectation) as someone's `b`,
        // so the full n/REPEL_SAMPLE weight double-counted repulsion and
        // blew large layouts apart.
        applyRepulsion(a, nodes[j], n / (2 * REPEL_SAMPLE), tune.nodeSpacing);
      }
    }
  }
  // Cross-directory file springs are OFF (the anchors carry that
  // attraction as one aggregate spring per directory pair). Over-quota
  // (undrawn) edges never pull.
  for (const e of edges) {
    if (!e.drawn || !e.intra) continue;
    applySpring(e, tune.nodeSpacing);
  }

  {
    for (const c of clusters) { c.fx = 0; c.fy = 0; c.fz = 0; }
    // Anchor↔anchor repulsion: the gross cluster separation. The spacing
    // knob scales it (and its cutoff) quadratically, same rationale as
    // applyRepulsion's node-spacing scaling.
    const csp2 = tune.clusterSpacing * tune.clusterSpacing;
    for (let i = 0; i < clusters.length; i++) {
      const a = clusters[i];
      for (let j = i + 1; j < clusters.length; j++) {
        const b = clusters[j];
        const dx = a.x - b.x, dy = a.y - b.y, dz = a.z - b.z;
        const d2 = dx * dx + dy * dy + dz * dz + 0.01;
        if (d2 > DIR_REPEL_CUTOFF_SQ * csp2) continue;
        const dist = Math.sqrt(d2);
        const f = (DIR_REPULSION_K * csp2) / d2;
        const fx = (dx / dist) * f, fy = (dy / dist) * f, fz = (dz / dist) * f;
        a.fx += fx; a.fy += fy; a.fz += fz;
        b.fx -= fx; b.fy -= fy; b.fz -= fz;
      }
    }
    // Aggregate directory springs (weight-boosted, capped).
    for (const de of dirEdges) {
      const a = de.a, b = de.b;
      const dx = b.x - a.x, dy = b.y - a.y, dz = b.z - a.z;
      const dist = Math.sqrt(dx * dx + dy * dy + dz * dz) || 0.01;
      const k = DIR_EDGE_K * Math.min(4, 1 + Math.log2(1 + de.weight));
      const f = k * (dist - DIR_EDGE_LEN * tune.clusterSpacing);
      const fx = (dx / dist) * f, fy = (dy / dist) * f, fz = (dz / dist) * f;
      a.fx += fx; a.fy += fy; a.fz += fz;
      b.fx -= fx; b.fy -= fy; b.fz -= fz;
    }
    // Member leash: files orbit their directory anchor at ~leash distance.
    for (const c of clusters) {
      c.fx += -c.x * GRAVITY_K;
      c.fy += -c.y * GRAVITY_K;
      c.fz += -c.z * GRAVITY_K;
      const inv = 1 / c.members.length;
      for (const m of c.members) {
        const dx = c.x - m.x, dy = c.y - m.y, dz = c.z - m.z;
        const dist = Math.sqrt(dx * dx + dy * dy + dz * dz) || 0.01;
        const f = DIR_MEMBER_K * tune.clusterStrength * (dist - c.leash);
        const fx = (dx / dist) * f, fy = (dy / dist) * f, fz = (dz / dist) * f;
        m.fx += fx; m.fy += fy; m.fz += fz;
        c.fx -= fx * inv; c.fy -= fy * inv; c.fz -= fz * inv;
      }
    }
    // Integrate anchors (same cooling/damping/cap as nodes).
    for (const c of clusters) {
      c.vx = (c.vx + c.fx * w.alpha * dts) * DAMPING;
      c.vy = (c.vy + c.fy * w.alpha * dts) * DAMPING;
      c.vz = (c.vz + c.fz * w.alpha * dts) * DAMPING;
      const sp = Math.hypot(c.vx, c.vy, c.vz);
      if (sp > MAX_VEL) {
        const s = MAX_VEL / sp;
        c.vx *= s; c.vy *= s; c.vz *= s;
      }
      c.x += c.vx * dts;
      c.y += c.vy * dts;
      c.z += c.vz * dts;
    }
  }

  let ke = 0;
  for (const node of nodes) {
    node.fx += -node.x * GRAVITY_K;
    node.fy += -node.y * GRAVITY_K;
    node.fz += -node.z * GRAVITY_K;
    node.vx = (node.vx + node.fx * w.alpha * dts) * DAMPING;
    node.vy = (node.vy + node.fy * w.alpha * dts) * DAMPING;
    node.vz = (node.vz + node.fz * w.alpha * dts) * DAMPING;
    const sp = Math.hypot(node.vx, node.vy, node.vz);
    if (sp > MAX_VEL) {
      const s = MAX_VEL / sp;
      node.vx *= s; node.vy *= s; node.vz *= s;
    }
    node.x += node.vx * dts;
    node.y += node.vy * dts;
    node.z += node.vz * dts;
    ke += node.vx * node.vx + node.vy * node.vy + node.vz * node.vz;
  }
  w.alpha *= Math.pow(1 - ALPHA_DECAY, dts);
  return w.alpha > ALPHA_MIN && ke / n > IDLE_KE_THRESHOLD;
}
