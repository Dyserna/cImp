<script lang="ts">
  // V15 Feature 4 (stretch): a live, interactive 3D visualization of the
  // project's code graph. Renders the bounded `graphVizSnapshot()` subgraph
  // (FILE-level: files are the only nodes; calls are rolled up to file→file
  // backend-side — symbol nodes were too many to render or read) with a
  // small self-contained force simulation (no external deps — plain
  // Canvas 2D + TypeScript), and pulses nodes as the graph tool history shows
  // an agent (cloud or the local offload worker) touching them, so a viewer
  // can watch the agent "walk" the codebase live. Mirrors the app-rendered
  // (no PTY) tab pattern used by WorkbenchView/CodeIntelligenceView —
  // `position:absolute;inset:0` so it sits above the pane's terminal slot.
  import { onMount, onDestroy } from 'svelte';
  import {
    graphVizSnapshot,
    graphVizEgo,
    graphHistory,
    onGraphStatus,
    type VizGraphResult,
    type VizNodeRow,
    type VizEdgeRow,
    type GraphCall,
  } from './graph';
  import { settings } from './settings/store';
  import type { Settings } from './settings/types';
  import { graphReveal, clearGraphReveal } from './graphReveal';

  // Mirrors WorkbenchView/CodeIntelligenceView: an optional project root:
  // neither currently receives one from `Pane.svelte` (both default to the
  // launch directory backend-side), so this stays optional too and is only
  // threaded through to `graphVizSnapshot` when the caller supplies one.
  let { root = undefined }: { root?: string } = $props();

  // ── Chart-specific colors (not app design tokens — Okabe-Ito categorical
  // palette, colorblind-safe; the edge dash pattern carries a redundant
  // non-hue channel alongside these). ─────────────────────────────────────
  const SUBSYSTEM_PALETTE = [
    '#E69F00', '#56B4E9', '#009E73', '#F0E442',
    '#0072B2', '#D55E00', '#CC79A7', '#999999',
  ];
  const UNCATEGORIZED_COLOR = '#8a93a3';
  const EDGE_CALL = '#4fb3ff';
  const EDGE_IMPORT = '#ff8a3d';
  const EDGE_CONTAINS = '#5fd38d';
  const EDGE_OTHER = '#9aa3b2';
  const PULSE_CLOUD = '#7fd4ff';
  const PULSE_LOCAL = '#ffb454';
  const PULSE_ADVISOR = '#d2a8ff';
  const ACCENT_COLOR = '#bb55ff';
  const PULSE_MS = 600;
  const FOCUS_MS = 900;

  const MIN_DIST_3D = 80;
  const MAX_DIST_3D = 8000;
  const NEAR_3D = 8;
  // Zoom-out ceiling. The folder-spacing knob (up to 50×) stretches the
  // layout roughly linearly, so the ceiling stretches with it — otherwise a
  // spaced-out graph couldn't be framed in one view.
  function maxDist3d(): number {
    return MAX_DIST_3D * Math.max(1, tune.clusterSpacing);
  }

  // ── Settings-driven tuning (Settings → Code Intelligence → Graph View
  // tuning). Multipliers on the base constants below — one size doesn't fit
  // every repo. The numeric knobs are read at frame rate by the physics/
  // render loop, so they live in a plain object updated by the settings
  // subscription (applyTuning), NOT in $state; the two edge colors ARE
  // $state because the legend/connections templates render them too.
  let tune = {
    nodeScale: 1,
    dirScale: 1,
    edgeWidth: 1,
    nodeSpacing: 1,
    clusterSpacing: 1,
    clusterStrength: 1,
  };
  let edgeCallColor = $state(EDGE_CALL);
  let edgeImportColor = $state(EDGE_IMPORT);

  // Physics tuning. O(n^2) repulsion is fine up to a few hundred nodes (the
  // backend caps the snapshot: `graph_viz_max_nodes` nodes, ~4 edges/node);
  // above MAX_REPEL_FULL we sample a bounded number of partners per node per
  // frame instead of every pair, so a large graph never blows the frame
  // budget.
  const REPULSION_K = 1600;
  const SPRING_K = 0.05;
  const SPRING_LEN = 45;
  const GRAVITY_K = 0.0025;
  const DAMPING = 0.85;
  // Per-frame speed cap (world units). Even with degree-normalized springs
  // (see applySpring) a hot start can make the integrator overshoot; the cap
  // guarantees no node gets flung out of the cluster in a single frame.
  const MAX_VEL = 40;
  const IDLE_KE_THRESHOLD = 0.05;
  const MAX_REPEL_FULL = 500;
  const REPEL_SAMPLE = 40;
  const REPEL_CUTOFF_SQ = 90000; // 300 world units
  // Directory clustering (the only layout): files leash to an invisible
  // per-directory anchor, anchors repel hard and spring together only where
  // cross-dir file edges exist, and cross-dir file edges render as ONE
  // aggregate edge per directory pair (per-file cross links only appear
  // routed through the anchors when a node is selected).
  const DIR_MEMBER_K = 0.06; // member ↔ anchor leash spring
  const DIR_EDGE_K = 0.015; // anchor ↔ anchor aggregate spring
  const DIR_EDGE_LEN = 420;
  const DIR_REPULSION_K = 90000;
  const DIR_REPEL_CUTOFF_SQ = 1440000; // 1200 world units

  // Cooling: forces are scaled by an exponentially decaying alpha (d3-style)
  // so the layout always converges, even under sampled-repulsion noise
  // (n > MAX_REPEL_FULL) whose kinetic energy alone never drops below
  // IDLE_KE_THRESHOLD. SIM_MAX_MS stays as a hard backstop so a stuck sim
  // can never pin the webview thread indefinitely.
  const ALPHA_DECAY = 0.02; // per ~16.7ms frame
  const ALPHA_MIN = 0.02; // below this the layout counts as settled
  const SIM_MAX_MS = 10_000;

  interface SimNode extends VizNodeRow {
    x: number; y: number; z: number;
    vx: number; vy: number; vz: number;
    fx: number; fy: number; fz: number;
    r: number;
    sx: number; sy: number; sr: number; sz: number;
    pulseUntil: number; pulseColor: string | null;
    dir: string;
  }
  interface SimEdge {
    src: SimNode; dst: SimNode;
    kind: string; confidence: string;
    highlightUntil: number; highlightColor: string | null;
    intra: boolean; // both endpoints in the same directory
    // Over-quota edges arrive with drawn=false: they feed the connections
    // panel, dir-edge weights, and the selection highlight, but are neither
    // simulated as springs nor drawn as ambient lines.
    drawn: boolean;
  }
  interface DirCluster {
    name: string;
    members: SimNode[];
    leash: number; // rest length of the member↔anchor spring
    x: number; y: number; z: number;
    vx: number; vy: number; vz: number;
    fx: number; fy: number; fz: number;
    sx: number; sy: number; sz: number;
    discR: number; // projected disc radius (computed at render)
  }
  interface DirEdge {
    a: DirCluster; b: DirCluster;
    weight: number; callW: number; importW: number;
  }

  // ── DOM refs ─────────────────────────────────────────────────────────────
  let containerEl: HTMLDivElement;
  // Reactive ($state) so the lifecycle $effect acquires the 2D context off
  // the binding itself — robust to mount order and to the element ever being
  // conditionally rendered again (doing it in onMount once read an unbound
  // ref, threw, and froze the tab on its "disabled" banner).
  let canvasEl = $state<HTMLCanvasElement | null>(null);
  let ctx: CanvasRenderingContext2D | null = null;

  // ── Reactive UI state ────────────────────────────────────────────────────
  let loading = $state(true);
  let fetchError = $state<string | null>(null);
  let nodeCount = $state(0);
  let edgeCount = $state(0);
  let legendOpen = $state(true);
  let graphEnabled = $state(false);
  // The configured snapshot cap (graph.graph_viz_max_nodes) — when nodeCount
  // hits it, the header notes the graph is a top-N-by-degree truncation.
  let vizMax = $state(1500);
  let subsystemsPresent = $state<string[]>([]);
  let hasUncategorized = $state(false);
  let edgeKindsPresent = $state<string[]>([]);
  let edgeConfsPresent = $state<string[]>([]);
  let sawCloudPulse = $state(false);
  let sawLocalPulse = $state(false);
  let sawAdvisorPulse = $state(false);
  let hoveredNode = $state<SimNode | null>(null);
  let hoverPos = $state<{ x: number; y: number }>({ x: 0, y: 0 });
  // Persistent selection (drives the ego highlight + connections panel).
  let selectedNodeId = $state<string | null>(null);
  // Hop history: node ids in visit order. `hopIx` points at the current
  // entry; a new selection truncates the forward branch (browser-style).
  let hopHistory = $state<string[]>([]);
  let hopIx = $state(-1);
  // Search box (type-ahead over node file paths).
  let searchQuery = $state('');
  let searchSel = $state(0);
  // Bumped on every buildSim so $derived values that read the non-reactive
  // nodes/edges arrays recompute when the graph data changes.
  let simVersion = $state(0);
  // Transient "that file isn't in the rendered graph" notice (Workbench jump).
  let revealMiss = $state<string | null>(null);
  // Focused directory (mutually exclusive with selectedNodeId): the view
  // zooms into the cluster and shows only its members + internal edges.
  let selectedDir = $state<string | null>(null);

  // ── Non-reactive simulation state (mutated at animation-frame rate — kept
  // out of $state so every position update doesn't trigger Svelte's
  // reactivity graph). ─────────────────────────────────────────────────────
  let nodes: SimNode[] = [];
  let edges: SimEdge[] = [];
  let nodeById = new Map<string, SimNode>();
  // Incident (drawn) edges per node id — the connections panel + ego
  // highlight read this instead of scanning the whole edge list.
  let edgesByNode = new Map<string, SimEdge[]>();
  // Directory clusters (the grouping layout — see the constants block).
  let clusters: DirCluster[] = [];
  let clusterByDir = new Map<string, DirCluster>();
  let dirEdges: DirEdge[] = [];
  // Workbench jump request waiting for the snapshot to load (mount order:
  // the reveal can arrive before the first buildSim).
  let pendingReveal: string | null = null;
  let revealMissTimer: ReturnType<typeof setTimeout> | undefined;
  // The last plain snapshot from the backend, and whether the sim currently
  // carries an injected reveal ego on top of it (a jump target the top-N cut
  // dropped, plus its neighbors). Clearing the selection rebuilds from
  // `lastSnap`, which is what removes the injected nodes again.
  let lastSnap: VizGraphResult | null = null;
  let egoInjected = false;
  let egoSeq = 0;

  // View transform.
  let viewW = 0;
  let viewH = 0;
  let camTheta = 0.6;
  let camPhi = 0.35;
  let camDist = 600;
  let camFocal = 480;
  let camTargetX = 0;
  let camTargetY = 0;
  let camTargetZ = 0;

  // Animation / interaction bookkeeping.
  let idle = true;
  let alpha = 0; // force-cooling factor; kick() re-heats it
  // Pointer/wheel interaction only wakes the *render* loop — it must never
  // re-heat the physics (re-heating on every wheel tick / drag move is what
  // made a settled layout explode on zoom). While the sim is settling and
  // the user hasn't touched the view yet, the camera auto-fits each frame so
  // the expanding layout stays framed instead of reading as spontaneous
  // zooming.
  let userInteracted = false;
  let simStartTs = 0; // when the sim last left idle — for the SIM_MAX_MS deadline
  let running = false;
  let rafId = 0;
  let lastTs = 0;
  let dragging = false;
  let dragMoved = false;
  let isPanDrag = false;
  let dragStartX = 0;
  let dragStartY = 0;
  let dragLastX = 0;
  let dragLastY = 0;
  let dragLastT = 0;
  let orbitVelTheta = 0;
  let orbitVelPhi = 0;
  let focusTarget: { x: number; y: number; z: number } | null = null;
  // Animated dolly target (directory focus zooms in); null = keep camDist.
  let focusDist: number | null = null;
  let focusRingUntil = 0;

  // Visibility gating (pause when hidden).
  let intersecting = false;
  let docVisible = typeof document !== 'undefined' && document.visibilityState === 'visible';
  let containerHasSize = false;
  let visible = false;
  let ro: ResizeObserver | undefined;
  let io: IntersectionObserver | undefined;
  let resizeDebounce: ReturnType<typeof setTimeout> | undefined;
  let pendingSize: { w: number; h: number } | null = null;
  // Set when buildSim's fit ran before the canvas had a measured size (a
  // fast fetch can beat the ResizeObserver's debounce); applyResize consumes
  // it and re-fits once real dimensions arrive.
  let needsFit = false;

  // Live-activity poll. `graph.ts` exposes no push event for individual tool
  // calls (only `graph-status`/`graph-analyses`), so per the task's fallback
  // instruction this polls `graphHistory()` on a short interval and diffs
  // against the newest `ts_ms` already processed.
  let historyTimer: ReturnType<typeof setInterval> | undefined;
  let lastHistoryTs = 0;
  // The first poll after (re)mount only seeds the high-water mark — replaying
  // the whole ring as one simultaneous burst is a light show, not signal.
  let historySeeded = false;

  let unsubSettings: (() => void) | undefined;
  let unsubStatus: (() => void) | undefined;

  // ── Small math / color helpers ──────────────────────────────────────────
  function clamp(v: number, lo: number, hi: number): number {
    return Math.min(hi, Math.max(lo, v));
  }
  function hashStr(s: string): number {
    let h = 0;
    for (let i = 0; i < s.length; i++) h = (h * 31 + s.charCodeAt(i)) | 0;
    return Math.abs(h);
  }
  function subsystemColor(name: string): string {
    return SUBSYSTEM_PALETTE[hashStr(name) % SUBSYSTEM_PALETTE.length];
  }
  function edgeColor(kind: string): string {
    if (kind === 'call') return edgeCallColor;
    if (kind === 'import') return edgeImportColor;
    if (kind === 'contains') return EDGE_CONTAINS;
    return EDGE_OTHER;
  }
  function dashFor(confidence: string): number[] {
    if (confidence === 'inferred') return [6, 4];
    if (confidence === 'ambiguous') return [1, 3];
    return [];
  }
  function basename(p: string): string {
    const i = p.lastIndexOf('/');
    return i >= 0 ? p.slice(i + 1) : p;
  }
  function dirOf(file: string): string {
    const i = file.lastIndexOf('/');
    return i >= 0 ? file.slice(0, i) : '(root)';
  }
  function normalize3(x: number, y: number, z: number): [number, number, number] {
    const len = Math.hypot(x, y, z) || 1;
    return [x / len, y / len, z / len];
  }
  function cross3(
    ax: number, ay: number, az: number,
    bx: number, by: number, bz: number,
  ): [number, number, number] {
    return [ay * bz - az * by, az * bx - ax * bz, ax * by - ay * bx];
  }

  // ── Selection / search / history (derived) ──────────────────────────────
  const selectedNode = $derived.by(() => {
    void simVersion; // re-resolve against the rebuilt node set
    return selectedNodeId ? (nodeById.get(selectedNodeId) ?? null) : null;
  });
  const searchMatches = $derived.by(() => {
    void simVersion;
    const q = searchQuery.trim().toLowerCase();
    if (!q) return [];
    return nodes
      .filter((n) => n.file.toLowerCase().includes(q))
      .sort((a, b) => b.degree - a.degree || a.file.localeCompare(b.file))
      .slice(0, 12);
  });
  interface ConnGroup { title: string; kind: string; rows: SimNode[]; }
  const connGroups = $derived.by(() => {
    const sel = selectedNode;
    if (!sel) return [] as ConnGroup[];
    const inc = edgesByNode.get(sel.id) ?? [];
    const grp = (title: string, kind: string, out: boolean): ConnGroup => ({
      title,
      kind,
      rows: inc
        .filter((e) => e.kind === kind && (out ? e.src === sel : e.dst === sel))
        .map((e) => (out ? e.dst : e.src)),
    });
    return [
      grp('calls →', 'call', true),
      grp('← called by', 'call', false),
      grp('imports →', 'import', true),
      grp('← imported by', 'import', false),
    ];
  });
  const shownConnCount = $derived(connGroups.reduce((t, g) => t + g.rows.length, 0));
  // ‹ also restores a cleared selection to the current history entry.
  const canBack = $derived(hopIx > 0 || (hopIx >= 0 && !selectedNodeId && !selectedDir));
  const canForward = $derived(hopIx < hopHistory.length - 1);
  // Focused directory (mutually exclusive with a node selection).
  const selectedDirCluster = $derived.by(() => {
    void simVersion;
    return !selectedNodeId && selectedDir ? (clusterByDir.get(selectedDir) ?? null) : null;
  });
  const dirMembers = $derived.by(() => {
    const c = selectedDirCluster;
    if (!c) return [] as SimNode[];
    return [...c.members].sort((a, b) => b.degree - a.degree || a.file.localeCompare(b.file));
  });
  const dirIntraCount = $derived.by(() => {
    const c = selectedDirCluster;
    if (!c) return 0;
    let n = 0;
    for (const e of edges) if (e.src.dir === c.name && e.dst.dir === c.name) n++;
    return n;
  });

  // ── Force simulation ─────────────────────────────────────────────────────
  // Log-scaled and small: rolled-up file degrees span 1..hundreds, and a
  // sqrt scale ballooned every hub into an edge-hiding blob. The settings
  // knob multiplies the whole curve.
  function nodeRadius(deg: number): number {
    return clamp(2 + Math.log2(1 + deg) * 0.8, 2, 7) * tune.nodeScale;
  }
  // Rest length of the member↔anchor leash — the directory cluster's size.
  function leashFor(memberCount: number): number {
    return (12 + Math.sqrt(memberCount) * 6) * tune.dirScale;
  }

  function initPosition(index: number, total: number): { x: number; y: number; z: number } {
    const golden = Math.PI * (3 - Math.sqrt(5));
    const theta = index * golden;
    const r = Math.sqrt(index + 0.5) * 8 * Math.max(1, Math.sqrt(total) / 10);
    const x = Math.cos(theta) * r;
    const y = Math.sin(theta) * r;
    const z = (Math.random() - 0.5) * r;
    return { x, y, z };
  }

  function applyRepulsion(a: SimNode, b: SimNode, weight: number): void {
    const dx = a.x - b.x;
    const dy = a.y - b.y;
    const dz = a.z - b.z;
    const distSq = dx * dx + dy * dy + dz * dz + 0.01;
    // Spacing scales the repulsion (and its cutoff) quadratically so the
    // node↔node equilibrium distance scales ~linearly with the knob.
    const sp2 = tune.nodeSpacing * tune.nodeSpacing;
    if (distSq > REPEL_CUTOFF_SQ * sp2) return;
    const dist = Math.sqrt(distSq);
    const f = (REPULSION_K * sp2 * weight) / distSq;
    const fx = (dx / dist) * f;
    const fy = (dy / dist) * f;
    const fz = (dz / dist) * f;
    a.fx += fx; a.fy += fy; a.fz += fz;
    b.fx -= fx; b.fy -= fy; b.fz -= fz;
  }

  function applySpring(e: SimEdge): void {
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
    const f = (SPRING_K / Math.min(degA, degB)) * (dist - SPRING_LEN * tune.nodeSpacing);
    const biasA = degB / (degA + degB);
    const fx = (dx / dist) * f;
    const fy = (dy / dist) * f;
    const fz = (dz / dist) * f;
    a.fx += fx * biasA; a.fy += fy * biasA; a.fz += fz * biasA;
    b.fx -= fx * (1 - biasA); b.fy -= fy * (1 - biasA); b.fz -= fz * (1 - biasA);
  }

  function simulate(dtMs: number): boolean {
    const n = nodes.length;
    if (n === 0) return false;
    const dts = clamp(dtMs, 1, 32) / 16.67;
    for (const node of nodes) { node.fx = 0; node.fy = 0; node.fz = 0; }

    if (n <= MAX_REPEL_FULL) {
      for (let i = 0; i < n; i++) {
        const a = nodes[i];
        for (let j = i + 1; j < n; j++) applyRepulsion(a, nodes[j], 1);
      }
    } else {
      for (let i = 0; i < n; i++) {
        const a = nodes[i];
        for (let s = 0; s < REPEL_SAMPLE; s++) {
          const j = (Math.random() * n) | 0;
          if (j === i) continue;
          // Halved weight: applyRepulsion pushes BOTH endpoints, and every
          // node is drawn both as `a` and (in expectation) as someone's `b`,
          // so the full n/REPEL_SAMPLE weight double-counted repulsion and
          // blew large layouts apart.
          applyRepulsion(a, nodes[j], n / (2 * REPEL_SAMPLE));
        }
      }
    }
    // Cross-directory file springs are OFF (the anchors carry that
    // attraction as one aggregate spring per directory pair). Over-quota
    // (undrawn) edges never pull.
    for (const e of edges) {
      if (!e.drawn || !e.intra) continue;
      applySpring(e);
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
        c.vx = (c.vx + c.fx * alpha * dts) * DAMPING;
        c.vy = (c.vy + c.fy * alpha * dts) * DAMPING;
        c.vz = (c.vz + c.fz * alpha * dts) * DAMPING;
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
      node.vx = (node.vx + node.fx * alpha * dts) * DAMPING;
      node.vy = (node.vy + node.fy * alpha * dts) * DAMPING;
      node.vz = (node.vz + node.fz * alpha * dts) * DAMPING;
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
    alpha *= Math.pow(1 - ALPHA_DECAY, dts);
    return alpha > ALPHA_MIN && ke / n > IDLE_KE_THRESHOLD;
  }

  // ── Camera / projection ──────────────────────────────────────────────────
  function computeCameraBasis() {
    const ex = camTargetX + camDist * Math.cos(camPhi) * Math.sin(camTheta);
    const ey = camTargetY + camDist * Math.sin(camPhi);
    const ez = camTargetZ + camDist * Math.cos(camPhi) * Math.cos(camTheta);
    const [zx, zy, zz] = normalize3(ex - camTargetX, ey - camTargetY, ez - camTargetZ);
    let [xx, xy, xz] = cross3(0, 1, 0, zx, zy, zz);
    const rl = Math.hypot(xx, xy, xz) || 1;
    xx /= rl; xy /= rl; xz /= rl;
    const [yx, yy, yz] = cross3(zx, zy, zz, xx, xy, xz);
    return { ex, ey, ez, xx, xy, xz, yx, yy, yz, zx, zy, zz };
  }

  function project(): void {
    const b = computeCameraBasis();
    for (const n of nodes) {
      const dx = n.x - b.ex, dy = n.y - b.ey, dz = n.z - b.ez;
      const rx = dx * b.xx + dy * b.xy + dz * b.xz;
      const ry = dx * b.yx + dy * b.yy + dz * b.yz;
      const fz = -(dx * b.zx + dy * b.zy + dz * b.zz);
      if (fz < NEAR_3D) {
        // At/behind the near plane: cull (sz = -1, parked far off-screen)
        // rather than clamping, which smeared the node across the screen
        // and drew distorted edges to it.
        n.sx = -1e6; n.sy = -1e6; n.sr = 0; n.sz = -1;
        continue;
      }
      const scale = camFocal / fz;
      n.sx = viewW / 2 + rx * scale;
      n.sy = viewH / 2 - ry * scale;
      // The screen-radius ceiling grows with the node-scale knob so scaling
      // up isn't silently flattened for close-by nodes.
      n.sr = clamp(n.r * scale, 1, 16 * Math.max(1, tune.nodeScale));
      n.sz = fz;
    }
    for (const c of clusters) {
      const dx = c.x - b.ex, dy = c.y - b.ey, dz = c.z - b.ez;
      const rx = dx * b.xx + dy * b.xy + dz * b.xz;
      const ry = dx * b.yx + dy * b.yy + dz * b.yz;
      const fz = -(dx * b.zx + dy * b.zy + dz * b.zz);
      if (fz < NEAR_3D) {
        c.sx = -1e6; c.sy = -1e6; c.sz = -1; c.discR = 0;
        continue;
      }
      const scale = camFocal / fz;
      c.sx = viewW / 2 + rx * scale;
      c.sy = viewH / 2 - ry * scale;
      c.sz = fz;
    }
  }

  // ── Rendering ─────────────────────────────────────────────────────────────
  // The selected node's 1-hop neighborhood (via ALL incident edges, drawn or
  // not). Null when no node is selected. With a selection active, everything
  // outside this set is HIDDEN, not dimmed.
  function currentEgoIds(): Set<string> | null {
    if (!selectedNodeId || !nodeById.has(selectedNodeId)) return null;
    const ego = new Set([selectedNodeId]);
    for (const e of edgesByNode.get(selectedNodeId) ?? []) {
      ego.add(e.src.id);
      ego.add(e.dst.id);
    }
    return ego;
  }
  // Which nodes render/pick at all: node selection → its ego set; directory
  // focus → that directory's members; otherwise everything.
  function visibleNodeIds(): Set<string> | null {
    const ego = currentEgoIds();
    if (ego) return ego;
    if (selectedDir) {
      const c = clusterByDir.get(selectedDir);
      if (c) return new Set(c.members.map((m) => m.id));
    }
    return null;
  }
  // Which directory discs render/pick: ego dirs, the focused dir, or all.
  function visibleDirNames(): Set<string> | null {
    const ego = currentEgoIds();
    if (ego) {
      const dirs = new Set<string>();
      for (const id of ego) {
        const n = nodeById.get(id);
        if (n) dirs.add(n.dir);
      }
      return dirs;
    }
    if (selectedDir) return new Set([selectedDir]);
    return null;
  }

  // Directory layer: translucent discs around each cluster, one aggregate
  // edge per connected directory pair (thicker = more file links), labels.
  // Drawn beneath the file edges/nodes. With a selection active only the
  // directories that contain ego nodes keep their (dimmed) discs — the
  // aggregate edges disappear in favor of the explicit routed links.
  function drawDirLayer(): void {
    if (!ctx) return;
    const egoDirs = visibleDirNames();
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
        ctx.lineWidth = Math.min(5, 1 + Math.log2(1 + de.weight)) * tune.edgeWidth;
        ctx.strokeStyle = de.callW >= de.importW ? edgeCallColor : edgeImportColor;
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
  function strokeEdgePath(e: SimEdge): void {
    if (!ctx) return;
    ctx.beginPath();
    ctx.moveTo(e.src.sx, e.src.sy);
    if (!e.intra) {
      const ca = clusterByDir.get(e.src.dir);
      const cb = clusterByDir.get(e.dst.dir);
      if (ca && ca.sz >= 0) ctx.lineTo(ca.sx, ca.sy);
      if (cb && cb.sz >= 0) ctx.lineTo(cb.sx, cb.sy);
    }
    ctx.lineTo(e.dst.sx, e.dst.sy);
    ctx.stroke();
  }

  function drawEdges(now: number): void {
    if (!ctx) return;
    // Batch by style: one beginPath/setLineDash/stroke PER (kind, confidence)
    // GROUP (≤ ~9 of them), not per edge — per-edge strokes made frame cost
    // O(edges) canvas state changes and pinned the webview thread on large
    // snapshots. Highlighted edges are few; they draw individually on top.
    // The hovered/selected node's own edges draw bright on top of the dimmed
    // field, so connectivity is traceable by pointing at (or selecting) a
    // node. An active selection dims the rest of the field even further.
    const selNode = selectedNodeId ? (nodeById.get(selectedNodeId) ?? null) : null;
    const selDir = !selNode ? selectedDir : null;
    const hovNode = hoveredNode;
    const groups = new Map<string, SimEdge[]>();
    const highlighted: SimEdge[] = [];
    const emphasized: SimEdge[] = [];
    for (const e of edges) {
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
    ctx.lineWidth = tune.edgeWidth;
    for (const g of groups.values()) {
      ctx.beginPath();
      for (const e of g) {
        ctx.moveTo(e.src.sx, e.src.sy);
        ctx.lineTo(e.dst.sx, e.dst.sy);
      }
      // The dash pattern restarts at each moveTo subpath, so one shared path
      // renders identically to the old per-edge strokes.
      ctx.setLineDash(dashFor(g[0].confidence));
      ctx.strokeStyle = edgeColor(g[0].kind);
      ctx.stroke();
    }
    ctx.globalAlpha = 0.85;
    ctx.lineWidth = 1.5 * tune.edgeWidth;
    for (const e of emphasized) {
      ctx.setLineDash(dashFor(e.confidence));
      ctx.strokeStyle = edgeColor(e.kind);
      strokeEdgePath(e);
    }
    ctx.globalAlpha = 0.95;
    ctx.lineWidth = 2.5 * tune.edgeWidth;
    for (const e of highlighted) {
      ctx.setLineDash(dashFor(e.confidence));
      ctx.strokeStyle = e.highlightColor ?? edgeColor(e.kind);
      strokeEdgePath(e);
    }
    ctx.setLineDash([]);
    ctx.globalAlpha = 1;
  }

  function drawNodes(now: number): void {
    if (!ctx) return;
    // With a node selection only the ego set renders; under directory focus
    // only that directory's members do.
    const egoIds = visibleNodeIds();
    const order = [...nodes].sort((a, b) => b.sz - a.sz);
    for (const n of order) {
      if (n.sz < 0) continue;
      if (egoIds !== null && !egoIds.has(n.id)) continue;
      ctx.fillStyle = n.subsystem ? subsystemColor(n.subsystem) : UNCATEGORIZED_COLOR;
      ctx.globalAlpha = 1;
      ctx.beginPath();
      ctx.arc(n.sx, n.sy, n.sr, 0, Math.PI * 2);
      ctx.fill();
      const isHovered = hoveredNode?.id === n.id;
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
      if (n.id === selectedNodeId) {
        // Persistent selection ring, plus the expanding just-selected pulse.
        ctx.save();
        ctx.globalAlpha = 0.9;
        ctx.strokeStyle = ACCENT_COLOR;
        ctx.lineWidth = 2;
        ctx.beginPath();
        ctx.arc(n.sx, n.sy, n.sr + 3, 0, Math.PI * 2);
        ctx.stroke();
        if (focusRingUntil > now) {
          const alpha = clamp((focusRingUntil - now) / FOCUS_MS, 0, 1);
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

  function render(): void {
    if (!ctx || viewW <= 0 || viewH <= 0) return;
    project();
    ctx.clearRect(0, 0, viewW, viewH);
    const now = performance.now();
    drawDirLayer();
    drawEdges(now);
    drawNodes(now);
  }

  // ── Animation loop ───────────────────────────────────────────────────────
  function decayActive(now: number): boolean {
    let active = false;
    for (const n of nodes) if (n.pulseUntil > now) { active = true; break; }
    if (!active) for (const e of edges) if (e.highlightUntil > now) { active = true; break; }
    if (!active && selectedNodeId && focusRingUntil > now) active = true;
    return active;
  }

  function stepViewAnimation(dt: number): boolean {
    if (!focusTarget && focusDist === null) return false;
    const k = 1 - Math.exp(-dt * 0.008);
    let moving = false;
    if (focusTarget) {
      camTargetX += (focusTarget.x - camTargetX) * k;
      camTargetY += (focusTarget.y - camTargetY) * k;
      camTargetZ += (focusTarget.z - camTargetZ) * k;
      const d = Math.hypot(focusTarget.x - camTargetX, focusTarget.y - camTargetY, focusTarget.z - camTargetZ);
      if (d < 0.5) {
        camTargetX = focusTarget.x; camTargetY = focusTarget.y; camTargetZ = focusTarget.z; focusTarget = null;
      } else {
        moving = true;
      }
    }
    if (focusDist !== null) {
      camDist += (focusDist - camDist) * k;
      if (Math.abs(focusDist - camDist) < 1) {
        camDist = focusDist; focusDist = null;
      } else {
        moving = true;
      }
    }
    return moving;
  }

  function stepMomentum(dt: number): boolean {
    if (dragging) return false;
    let moving = false;
    if (Math.abs(orbitVelTheta) > 0.00002 || Math.abs(orbitVelPhi) > 0.00002) {
      camTheta += orbitVelTheta * dt;
      camPhi = clamp(camPhi + orbitVelPhi * dt, -1.45, 1.45);
      orbitVelTheta *= 0.88; orbitVelPhi *= 0.88;
      moving = true;
    }
    return moving;
  }

  function loop(ts: number): void {
    if (!visible) { running = false; rafId = 0; return; }
    const dt = lastTs ? ts - lastTs : 16.67;
    lastTs = ts;

    if (!idle) {
      if (ts - simStartTs > SIM_MAX_MS) {
        idle = true;
        // Drop residual velocities — a deadline stop can land mid-flight,
        // and carrying that momentum into the next re-heat made nodes burst.
        for (const n of nodes) { n.vx = 0; n.vy = 0; n.vz = 0; }
      } else {
        const moving = simulate(dt);
        if (!moving) idle = true;
        else if (!userInteracted && !dragging) {
          // Keep the settling layout framed; stops at the first user touch.
          fitView();
        }
      }
    }
    const now = performance.now();
    const pulsing = decayActive(now);
    const animatingView = stepViewAnimation(dt);
    const momentum = stepMomentum(dt);
    render();

    if (!idle || pulsing || animatingView || momentum || dragging) {
      rafId = requestAnimationFrame(loop);
    } else {
      running = false;
      rafId = 0;
      lastTs = 0;
    }
  }

  function wake(): void {
    if (!visible || nodes.length === 0) return;
    if (running) return;
    running = true;
    lastTs = 0;
    rafId = requestAnimationFrame(loop);
  }
  // Re-heat the physics. Only data/layout changes call this — pointer and
  // wheel handlers call wake() instead, so moving the camera never re-boils
  // a settled layout.
  function kick(heat = 1): void {
    alpha = Math.max(alpha, heat);
    // Only re-arm the deadline on an idle→active transition, so a stream of
    // kicks can't extend it forever.
    if (idle) simStartTs = performance.now();
    idle = false;
    wake();
  }
  function stopLoop(): void {
    if (rafId) cancelAnimationFrame(rafId);
    rafId = 0;
    running = false;
    lastTs = 0;
  }

  // ── Data loading ─────────────────────────────────────────────────────────
  function buildSim(snap: VizGraphResult): void {
    const prev = nodeById;
    const newNodes: SimNode[] = [];
    const newById = new Map<string, SimNode>();
    snap.nodes.forEach((row, i) => {
      const old = prev.get(row.id);
      const pos = old
        ? { x: old.x, y: old.y, z: old.z }
        : initPosition(i, snap.nodes.length);
      const r = nodeRadius(row.degree || 0);
      const sn: SimNode = {
        ...row,
        x: pos.x, y: pos.y, z: pos.z,
        vx: old?.vx ?? 0, vy: old?.vy ?? 0, vz: old?.vz ?? 0,
        fx: 0, fy: 0, fz: 0,
        r, sx: 0, sy: 0, sr: 0, sz: 0,
        pulseUntil: 0, pulseColor: null,
        dir: dirOf(row.file),
      };
      newNodes.push(sn);
      newById.set(row.id, sn);
    });
    const newEdges: SimEdge[] = [];
    for (const row of snap.edges) {
      const s = newById.get(row.src);
      const d = newById.get(row.dst);
      if (!s || !d) continue;
      newEdges.push({
        src: s, dst: d, kind: row.kind, confidence: row.confidence,
        highlightUntil: 0, highlightColor: null,
        intra: s.dir === d.dir,
        drawn: row.drawn,
      });
    }

    // Directory clusters: one anchor per directory (position preserved across
    // refreshes), plus ONE aggregate edge per connected directory pair.
    const prevClusters = clusterByDir;
    const membersByDir = new Map<string, SimNode[]>();
    for (const n of newNodes) {
      let m = membersByDir.get(n.dir);
      if (!m) membersByDir.set(n.dir, (m = []));
      m.push(n);
    }
    const newClusterByDir = new Map<string, DirCluster>();
    const newClusters: DirCluster[] = [];
    for (const [dir, members] of membersByDir) {
      const old = prevClusters.get(dir);
      let ax = old?.x ?? 0, ay = old?.y ?? 0, az = old?.z ?? 0;
      if (!old) {
        for (const m of members) { ax += m.x; ay += m.y; az += m.z; }
        ax /= members.length; ay /= members.length; az /= members.length;
      }
      const c: DirCluster = {
        name: dir, members,
        leash: leashFor(members.length),
        x: ax, y: ay, z: az,
        vx: 0, vy: 0, vz: 0, fx: 0, fy: 0, fz: 0,
        sx: 0, sy: 0, sz: 0, discR: 0,
      };
      newClusterByDir.set(dir, c);
      newClusters.push(c);
    }
    const newDirEdges: DirEdge[] = [];
    const dirEdgeIx = new Map<string, DirEdge>();
    for (const e of newEdges) {
      if (e.intra) continue;
      const [a, b] = e.src.dir < e.dst.dir ? [e.src.dir, e.dst.dir] : [e.dst.dir, e.src.dir];
      const key = a + '\n' + b;
      let de = dirEdgeIx.get(key);
      if (!de) {
        de = { a: newClusterByDir.get(a)!, b: newClusterByDir.get(b)!, weight: 0, callW: 0, importW: 0 };
        dirEdgeIx.set(key, de);
        newDirEdges.push(de);
      }
      de.weight += 1;
      if (e.kind === 'call') de.callW += 1;
      else if (e.kind === 'import') de.importW += 1;
    }

    const newByNode = new Map<string, SimEdge[]>();
    for (const e of newEdges) {
      let a = newByNode.get(e.src.id);
      if (!a) newByNode.set(e.src.id, (a = []));
      a.push(e);
      let b = newByNode.get(e.dst.id);
      if (!b) newByNode.set(e.dst.id, (b = []));
      b.push(e);
    }

    nodes = newNodes;
    edges = newEdges;
    nodeById = newById;
    edgesByNode = newByNode;
    clusters = newClusters;
    clusterByDir = newClusterByDir;
    dirEdges = newDirEdges;
    nodeCount = newNodes.length;
    edgeCount = newEdges.filter((e) => e.drawn).length;
    subsystemsPresent = [...new Set(newNodes.map((n) => n.subsystem).filter((s) => s))].sort();
    hasUncategorized = newNodes.some((n) => !n.subsystem);
    edgeKindsPresent = [...new Set(newEdges.map((e) => e.kind))].sort();
    edgeConfsPresent = [...new Set(newEdges.map((e) => e.confidence))].sort();
    // Selection survives a refresh as long as its node/dir is still present.
    if (selectedNodeId && !newById.has(selectedNodeId)) selectedNodeId = null;
    if (selectedDir && !newClusterByDir.has(selectedDir)) selectedDir = null;
    hoveredNode = null;
    simVersion += 1;
    const isFresh = prev.size === 0;
    if (isFresh) {
      // First data: hand the camera to the settle-time auto-fit. Incremental
      // refreshes (Refresh button, index-pass auto-refresh) keep the prior
      // positions AND the user's camera — no view yank mid-look.
      userInteracted = false;
      resetView();
      // If the canvas has no measured size yet (a fast fetch can beat the
      // ResizeObserver), defer a re-fit to the first real applyResize.
      needsFit = viewW <= 0 || viewH <= 0;
    }
    // A fresh graph settles from scratch; a refresh that kept the previous
    // positions only needs a gentle nudge into the new equilibrium.
    kick(isFresh ? 1 : 0.3);
    tryReveal();
  }

  async function refresh(): Promise<void> {
    loading = true;
    fetchError = null;
    try {
      const snap = await graphVizSnapshot(root);
      // A fresh snapshot replaces any injected reveal ego wholesale.
      lastSnap = snap;
      egoInjected = false;
      buildSim(snap);
    } catch (e) {
      fetchError = String(e);
    } finally {
      loading = false;
    }
  }

  // ── View fitting ─────────────────────────────────────────────────────────
  function fitView(): void {
    if (nodes.length === 0) return;
    let cx = 0, cy = 0, cz = 0;
    for (const n of nodes) { cx += n.x; cy += n.y; cz += n.z; }
    cx /= nodes.length; cy /= nodes.length; cz /= nodes.length;
    let maxR = 0;
    for (const n of nodes) maxR = Math.max(maxR, Math.hypot(n.x - cx, n.y - cy, n.z - cz));
    camTargetX = cx; camTargetY = cy; camTargetZ = cz;
    camTheta = 0.6; camPhi = 0.35;
    camDist = clamp(maxR * 2.2 + 80, MIN_DIST_3D, maxDist3d());
  }
  function resetView(): void {
    // Camera refit only — it does not re-heat the physics. Handing the view
    // back also re-enables the settle-time auto-fit.
    fitView();
    focusTarget = null;
    focusDist = null;
    orbitVelTheta = 0; orbitVelPhi = 0;
    userInteracted = false;
    wake();
  }

  // ── Selection + hop history ──────────────────────────────────────────────
  // History entries: node ids (`file:<path>`) and focused dirs (`dir:<name>`).
  function historyPush(id: string): void {
    if (hopHistory[hopIx] === id) return;
    hopHistory = [...hopHistory.slice(0, hopIx + 1), id];
    hopIx = hopHistory.length - 1;
  }
  function selectNode(node: SimNode, push = true): void {
    selectedNodeId = node.id;
    selectedDir = null;
    focusRingUntil = performance.now() + FOCUS_MS;
    focusTarget = { x: node.x, y: node.y, z: node.z };
    focusDist = null;
    if (push) historyPush(node.id);
    wake();
  }
  function selectDir(c: DirCluster, push = true): void {
    selectedNodeId = null;
    selectedDir = c.name;
    focusTarget = { x: c.x, y: c.y, z: c.z };
    // Dolly in far enough to frame the whole cluster (~58° vertical FOV).
    let maxR = c.leash;
    for (const m of c.members) {
      maxR = Math.max(maxR, Math.hypot(m.x - c.x, m.y - c.y, m.z - c.z));
    }
    focusDist = clamp(maxR * 2.4 + 60, MIN_DIST_3D, maxDist3d());
    if (push) historyPush('dir:' + c.name);
    wake();
  }
  function clearSelection(): void {
    selectedNodeId = null;
    selectedDir = null;
    wake();
  }
  function hopResolvable(id: string): boolean {
    return id.startsWith('dir:') ? clusterByDir.has(id.slice(4)) : nodeById.has(id);
  }
  function hopTo(ix: number): void {
    const id = hopHistory[ix];
    if (!hopResolvable(id)) return;
    hopIx = ix;
    if (id.startsWith('dir:')) selectDir(clusterByDir.get(id.slice(4))!, false);
    else selectNode(nodeById.get(id)!, false);
  }
  function hopBack(): void {
    // A cleared selection restores the current entry before stepping back.
    if (!selectedNodeId && !selectedDir && hopIx >= 0 && hopResolvable(hopHistory[hopIx])) {
      hopTo(hopIx);
      return;
    }
    // Skip entries whose node/dir fell out of the snapshot since.
    for (let i = hopIx - 1; i >= 0; i--) {
      if (hopResolvable(hopHistory[i])) { hopTo(i); return; }
    }
  }
  function hopForward(): void {
    for (let i = hopIx + 1; i < hopHistory.length; i++) {
      if (hopResolvable(hopHistory[i])) { hopTo(i); return; }
    }
  }

  // ── Search ───────────────────────────────────────────────────────────────
  function pickSearch(n: SimNode): void {
    selectNode(n);
    searchQuery = '';
    searchSel = 0;
  }
  function onSearchKey(e: KeyboardEvent): void {
    const matches = searchMatches;
    if (e.key === 'ArrowDown') {
      e.preventDefault();
      searchSel = Math.min(searchSel + 1, matches.length - 1);
    } else if (e.key === 'ArrowUp') {
      e.preventDefault();
      searchSel = Math.max(searchSel - 1, 0);
    } else if (e.key === 'Enter' && matches.length > 0) {
      e.preventDefault();
      pickSearch(matches[clamp(searchSel, 0, matches.length - 1)]);
    } else if (e.key === 'Escape') {
      searchQuery = '';
    }
  }

  // ── Workbench reveal (graphReveal store) ─────────────────────────────────
  function showRevealMiss(msg: string): void {
    revealMiss = msg;
    if (revealMissTimer) clearTimeout(revealMissTimer);
    revealMissTimer = setTimeout(() => (revealMiss = null), 5000);
  }
  function tryReveal(): void {
    if (!pendingReveal || nodes.length === 0) return;
    const path = pendingReveal;
    pendingReveal = null;
    const node = nodeById.get('file:' + path);
    if (node) {
      selectNode(node);
      return;
    }
    // Not in the rendered snapshot (below the top-N-by-degree cut) — inject
    // its ego from the full graph instead of giving up.
    void revealViaEgo(path);
  }
  // Fetch the jump target's 1-hop file ego (computed on the FULL rollup,
  // ignoring the top-N cut), merge it into the sim, and select it. The
  // injected nodes/edges live only until the selection is cleared — the
  // $effect below then rebuilds from the plain snapshot.
  async function revealViaEgo(path: string): Promise<void> {
    const seq = ++egoSeq;
    try {
      const ego = await graphVizEgo(path, root);
      if (seq !== egoSeq || !lastSnap) return; // superseded by a newer jump / teardown
      const target = ego.nodes.find((n) => n.file === path);
      if (!target) {
        showRevealMiss(`${path} isn't in the code graph`);
        return;
      }
      if (ego.edges.length === 0) {
        showRevealMiss(`${path} has no imports or calls in the code graph`);
        return;
      }
      const baseIds = new Set(lastSnap.nodes.map((n) => n.id));
      buildSim(mergeEgo(lastSnap, ego));
      egoInjected = true;
      // buildSim's tail runs tryReveal — a jump that arrived mid-fetch may
      // have started a newer reveal; don't fight it for the selection.
      if (seq !== egoSeq) return;
      // Spawn each injected node next to an already-placed neighbor instead
      // of buildSim's golden-spiral fringe, so the focus jump lands inside
      // the layout rather than panning across it.
      for (const row of ego.nodes) {
        if (baseIds.has(row.id)) continue;
        const sn = nodeById.get(row.id);
        if (!sn) continue;
        const anchor = (edgesByNode.get(row.id) ?? [])
          .map((e) => (e.src.id === row.id ? e.dst : e.src))
          .find((n) => baseIds.has(n.id));
        if (anchor) {
          sn.x = anchor.x + (Math.random() - 0.5) * 40;
          sn.y = anchor.y + (Math.random() - 0.5) * 40;
          sn.z = anchor.z + (Math.random() - 0.5) * 40;
        }
      }
      const node = nodeById.get(target.id);
      if (node) selectNode(node);
    } catch (e) {
      showRevealMiss(`couldn't look up ${path} in the graph: ${e}`);
    }
  }
  // The snapshot plus the ego's extra nodes/edges (deduplicated — neighbors
  // that already render keep their existing node; an ego edge that the
  // snapshot already carries keeps the snapshot's drawn flag).
  function mergeEgo(base: VizGraphResult, ego: VizGraphResult): VizGraphResult {
    const nodeIds = new Set(base.nodes.map((n) => n.id));
    const edgeKey = (e: VizEdgeRow) => `${e.src}\n${e.dst}\n${e.kind}`;
    const edgeIds = new Set(base.edges.map(edgeKey));
    return {
      nodes: [...base.nodes, ...ego.nodes.filter((n) => !nodeIds.has(n.id))],
      edges: [...base.edges, ...ego.edges.filter((e) => !edgeIds.has(edgeKey(e)))],
    };
  }
  // "Once the selection is cleared, hide the file again": with an ego
  // injected and no selection left at all (empty-canvas click, Esc), rebuild
  // from the plain snapshot. A directory focus or hopping to a neighbor is
  // still a selection, so the injected nodes survive those.
  $effect(() => {
    if (selectedNodeId || selectedDir) return;
    if (!egoInjected || !lastSnap) return;
    egoInjected = false;
    buildSim(lastSnap);
  });
  $effect(() => {
    const req = $graphReveal;
    if (!req) return;
    pendingReveal = req.path;
    // Consume so a later (re)mount doesn't replay this request.
    clearGraphReveal();
    tryReveal();
  });

  // ── Live activity (poll graphHistory — no push event exists per-call) ────
  function matchNodes(target: string): SimNode[] {
    // Nodes are files only, so a symbol-name target can't be resolved here —
    // it matches only when the call target names (or contains) a file path.
    if (!target) return [];
    const exact = nodes.filter((n) => n.file === target);
    if (exact.length) return exact.slice(0, 3);
    const bare = target.split(':')[0];
    const byFile = nodes.filter((n) => n.file === bare || bare.endsWith(n.file) || n.file.endsWith(bare));
    return byFile.slice(0, 3);
  }

  function applyActivity(call: GraphCall): void {
    const matched = matchNodes(call.target);
    if (matched.length === 0) return;
    const isCloud = call.source === 'claude' || call.source === 'opencode';
    // read_advisor/auto_check are backend-internal services, not offload
    // worker traffic — they get their own pulse bucket.
    const isAdvisor = call.source === 'read_advisor' || call.source === 'auto_check';
    const color = isCloud ? PULSE_CLOUD : isAdvisor ? PULSE_ADVISOR : PULSE_LOCAL;
    if (isCloud) sawCloudPulse = true;
    else if (isAdvisor) sawAdvisorPulse = true;
    else sawLocalPulse = true;
    const now = performance.now();
    for (const n of matched) {
      n.pulseUntil = now + PULSE_MS; n.pulseColor = color;
    }
    // No per-edge result is available from GraphCall (tool + target only) —
    // approximate "traversed edge" for callers/callees calls by lighting up
    // the call-edges incident to the matched node(s); everything else just
    // pulses the node.
    if (/callers|callees/i.test(call.tool)) {
      for (const n of matched) {
        for (const e of edges) {
          if (e.kind === 'call' && (e.src === n || e.dst === n)) {
            e.highlightUntil = now + PULSE_MS;
            e.highlightColor = color;
          }
        }
      }
    }
    wake();
  }

  async function pollHistory(): Promise<void> {
    if (!graphEnabled) return;
    try {
      // Scoped: the store spans every indexed root, and another project's
      // calls would light up same-named nodes in this graph. After the first
      // (seeding) poll, only entries past the high-water mark are fetched —
      // the steady-state poll returns ~0 rows instead of the whole history.
      const calls = await graphHistory({
        root,
        scoped: true,
        sinceTs: historySeeded ? lastHistoryTs : undefined,
      });
      const fresh = calls.filter((c) => c.ts_ms > lastHistoryTs).sort((a, b) => a.ts_ms - b.ts_ms);
      for (const c of calls) if (c.ts_ms > lastHistoryTs) lastHistoryTs = c.ts_ms;
      // First poll: seed the high-water mark only. Parked (hidden/unsized):
      // advance past what won't be shown rather than saving it up for a
      // burst on return. Either way, no replayed backlog.
      if (!historySeeded) { historySeeded = true; return; }
      if (!visible) return;
      for (const c of fresh) applyActivity(c);
    } catch {
      // Non-fatal — keep the last-known pulses/state and retry next tick.
    }
  }
  function startHistoryPoll(): void {
    if (historyTimer) return;
    // Re-seed on every (re)start — calls that landed while polling was off
    // (graph disabled) are backlog, not live activity.
    historySeeded = false;
    void pollHistory();
    historyTimer = setInterval(() => void pollHistory(), 1500);
  }
  function stopHistoryPoll(): void {
    if (historyTimer) { clearInterval(historyTimer); historyTimer = undefined; }
  }

  // ── Settings tuning ──────────────────────────────────────────────────────
  // Adopt the settings' Graph View tuning knobs (clamped to the UI's range —
  // 0.2–5, except folder spacing which goes to 50 — in case a hand-edited
  // settings file goes wild). Geometry
  // knobs (spacing / cluster size / tightness) re-heat the sim so the layout
  // re-settles into the new equilibrium; appearance knobs (node size, edge
  // width/color) only need a repaint. Values buildSim baked into the live
  // nodes/clusters (radius, leash) are recomputed in place.
  function applyTuning(g: Settings['graph']): void {
    const knob = (v: number, max = 5) => clamp(Number(v) || 1, 0.2, max);
    const next = {
      nodeScale: knob(g.graph_viz_node_scale),
      dirScale: knob(g.graph_viz_dir_scale),
      edgeWidth: knob(g.graph_viz_edge_width),
      nodeSpacing: knob(g.graph_viz_node_spacing),
      clusterSpacing: knob(g.graph_viz_cluster_spacing, 50),
      clusterStrength: knob(g.graph_viz_cluster_strength),
    };
    const nextCall = g.graph_viz_color_call || EDGE_CALL;
    const nextImport = g.graph_viz_color_import || EDGE_IMPORT;
    const geomChanged =
      next.dirScale !== tune.dirScale ||
      next.nodeSpacing !== tune.nodeSpacing ||
      next.clusterSpacing !== tune.clusterSpacing ||
      next.clusterStrength !== tune.clusterStrength;
    const lookChanged =
      next.nodeScale !== tune.nodeScale ||
      next.edgeWidth !== tune.edgeWidth ||
      nextCall !== edgeCallColor ||
      nextImport !== edgeImportColor;
    tune = next;
    edgeCallColor = nextCall;
    edgeImportColor = nextImport;
    if ((!geomChanged && !lookChanged) || nodes.length === 0) return;
    for (const n of nodes) n.r = nodeRadius(n.degree);
    for (const c of clusters) c.leash = leashFor(c.members.length);
    if (geomChanged) kick(0.5);
    else wake(); // a single repaint frame picks up sizes/widths/colors
  }

  // ── Resize / visibility plumbing ─────────────────────────────────────────
  function applyResize(): void {
    if (!pendingSize || !ctx || !canvasEl) return;
    const { w, h } = pendingSize;
    if (w <= 0 || h <= 0) {
      containerHasSize = false;
      updateVisibility();
      return;
    }
    containerHasSize = true;
    const dpr = Math.min(window.devicePixelRatio || 1, 2);
    canvasEl.width = Math.max(1, Math.round(w * dpr));
    canvasEl.height = Math.max(1, Math.round(h * dpr));
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    viewW = w; viewH = h;
    // ~58° vertical FOV regardless of window size (a fixed focal length made
    // the effective FOV a function of how big the pane happened to be).
    camFocal = Math.max(200, viewH * 0.9);
    updateVisibility();
    if (needsFit && nodes.length > 0) {
      needsFit = false;
      resetView();
    }
    wake();
  }
  function handleResize(entries: ResizeObserverEntry[]): void {
    const entry = entries[0];
    if (!entry) return;
    const box = entry.contentBoxSize && entry.contentBoxSize[0];
    const w = box ? box.inlineSize : entry.contentRect.width;
    const h = box ? box.blockSize : entry.contentRect.height;
    pendingSize = { w, h };
    if (resizeDebounce) clearTimeout(resizeDebounce);
    resizeDebounce = setTimeout(applyResize, 80);
  }
  function updateVisibility(): void {
    const vis = intersecting && docVisible && containerHasSize;
    if (vis && !visible) { visible = true; wake(); }
    else if (!vis && visible) { visible = false; stopLoop(); }
  }
  function handleIntersect(entries: IntersectionObserverEntry[]): void {
    const entry = entries[0];
    if (!entry) return;
    intersecting = entry.isIntersecting && entry.intersectionRatio > 0;
    updateVisibility();
  }
  function handleVisChange(): void {
    docVisible = document.visibilityState === 'visible';
    updateVisibility();
  }

  // ── Mouse interaction ────────────────────────────────────────────────────
  function pickNode(mx: number, my: number): SimNode | null {
    // Front-most (smallest camera depth) within the hit radius wins —
    // nearest-in-2D kept selecting nodes hidden far behind the one the
    // cursor is visibly on in a dense cluster. Only visible nodes pick.
    const vis = visibleNodeIds();
    let best: SimNode | null = null;
    let bestZ = Infinity;
    for (const n of nodes) {
      if (n.sz < 0) continue;
      if (vis && !vis.has(n.id)) continue;
      const d = Math.hypot(mx - n.sx, my - n.sy);
      if (d <= Math.max(n.sr, 10) && n.sz < bestZ) { best = n; bestZ = n.sz; }
    }
    return best;
  }

  function pickDir(mx: number, my: number): DirCluster | null {
    // Front-most visible disc under the cursor (checked only after pickNode
    // misses, so nodes win over their disc). discR comes from the last
    // drawDirLayer pass.
    const dirs = visibleDirNames();
    let best: DirCluster | null = null;
    let bestZ = Infinity;
    for (const c of clusters) {
      if (c.sz < 0 || c.discR <= 0) continue;
      if (dirs && !dirs.has(c.name)) continue;
      const d = Math.hypot(mx - c.sx, my - c.sy);
      if (d <= c.discR && c.sz < bestZ) { best = c; bestZ = c.sz; }
    }
    return best;
  }

  function onCanvasMouseDown(e: MouseEvent): void {
    if (nodes.length === 0) return;
    dragStartX = e.clientX; dragStartY = e.clientY;
    dragLastX = e.clientX; dragLastY = e.clientY;
    dragLastT = performance.now();
    dragMoved = false;
    isPanDrag = e.button === 2 || e.shiftKey;
    dragging = true;
    orbitVelTheta = 0; orbitVelPhi = 0;
    focusTarget = null;
    focusDist = null;
    userInteracted = true;
    window.addEventListener('mousemove', onWindowMouseMove);
    window.addEventListener('mouseup', onWindowMouseUp);
    wake();
  }

  function onWindowMouseMove(e: MouseEvent): void {
    if (!dragging) return;
    const dx = e.clientX - dragLastX;
    const dy = e.clientY - dragLastY;
    const now = performance.now();
    const dt = Math.max(1, now - dragLastT);
    if (Math.abs(e.clientX - dragStartX) + Math.abs(e.clientY - dragStartY) > 4) dragMoved = true;

    if (isPanDrag) {
      const b = computeCameraBasis();
      const worldPerPx = camDist / camFocal;
      camTargetX -= (b.xx * dx - b.yx * dy) * worldPerPx;
      camTargetY -= (b.xy * dx - b.yy * dy) * worldPerPx;
      camTargetZ -= (b.xz * dx - b.yz * dy) * worldPerPx;
    } else {
      camTheta -= dx * 0.006;
      camPhi = clamp(camPhi - dy * 0.006, -1.45, 1.45);
      orbitVelTheta = -dx * 0.006 / dt;
      orbitVelPhi = -dy * 0.006 / dt;
    }
    dragLastX = e.clientX; dragLastY = e.clientY; dragLastT = now;
    wake();
  }

  function onWindowMouseUp(e: MouseEvent): void {
    if (!dragging) return;
    dragging = false;
    window.removeEventListener('mousemove', onWindowMouseMove);
    window.removeEventListener('mouseup', onWindowMouseUp);
    if (!dragMoved && canvasEl) {
      const rect = canvasEl.getBoundingClientRect();
      const mx = e.clientX - rect.left;
      const my = e.clientY - rect.top;
      const hit = pickNode(mx, my);
      if (hit) {
        selectNode(hit);
      } else {
        const dirHit = pickDir(mx, my);
        if (dirHit) selectDir(dirHit);
        else clearSelection();
      }
    }
    wake();
  }

  function onWindowKeyDown(e: KeyboardEvent): void {
    if (!visible) return;
    const t = e.target as HTMLElement | null;
    const typing = !!t && (t.tagName === 'INPUT' || t.tagName === 'TEXTAREA' || t.isContentEditable);
    if (e.key === 'Escape') {
      // Close the search dropdown first, then clear the selection.
      if (searchQuery) searchQuery = '';
      else if (selectedNodeId || selectedDir) clearSelection();
    } else if (e.key === 'Backspace' && !typing && canBack) {
      e.preventDefault();
      hopBack();
    }
  }

  function onWheel(e: WheelEvent): void {
    if (nodes.length === 0 || !canvasEl) return;
    e.preventDefault();
    camDist = clamp(camDist * Math.exp(e.deltaY * 0.0015), MIN_DIST_3D, maxDist3d());
    focusDist = null; // the wheel takes over any in-flight dolly
    userInteracted = true;
    wake();
  }

  function onCanvasMouseMove(e: MouseEvent): void {
    if (dragging || !canvasEl) return;
    const rect = canvasEl.getBoundingClientRect();
    const mx = e.clientX - rect.left;
    const my = e.clientY - rect.top;
    const hit = pickNode(mx, my);
    if (hit !== hoveredNode) {
      hoveredNode = hit;
      render();
    }
    if (hit) hoverPos = { x: mx, y: my };
  }
  function onCanvasMouseLeave(): void {
    if (hoveredNode) { hoveredNode = null; render(); }
  }

  // ── Lifecycle ────────────────────────────────────────────────────────────
  // Acquire the 2D context off the binding rather than in onMount — safe
  // against mount order and against the canvas ever being conditionally
  // rendered again (the onMount version read an unbound ref, threw, and took
  // the settings subscription below down with it).
  $effect(() => {
    if (!canvasEl) {
      ctx = null;
      return;
    }
    ctx = canvasEl.getContext('2d');
    // Size the fresh canvas immediately — the ResizeObserver's debounced
    // pass may not have fired yet.
    if (!pendingSize && containerEl) {
      const r = containerEl.getBoundingClientRect();
      pendingSize = { w: r.width, h: r.height };
    }
    applyResize();
  });

  onMount(() => {
    ro = new ResizeObserver(handleResize);
    ro.observe(containerEl);
    io = new IntersectionObserver(handleIntersect, { threshold: 0 });
    io.observe(containerEl);
    document.addEventListener('visibilitychange', handleVisChange);
    window.addEventListener('keydown', onWindowKeyDown);
    // Fires synchronously with the current value, so this both seeds
    // `graphEnabled` and kicks off the first load/poll without a separate
    // one-off read.
    unsubSettings = settings.subscribe((s) => {
      vizMax = Math.max(1, s.graph.graph_viz_max_nodes);
      applyTuning(s.graph);
      const en = s.graph.enabled;
      if (en === graphEnabled) return;
      graphEnabled = en;
      if (en) { void refresh(); startHistoryPoll(); }
      else { stopHistoryPoll(); stopLoop(); }
    });
    // The startup fetch races the background index build — a snapshot pulled
    // mid-build only holds the files indexed so far. Re-pull whenever an
    // index pass for this root completes (also keeps the graph current with
    // the fs-watcher's incremental re-indexes; positions/camera are
    // preserved by buildSim's incremental path).
    void onGraphStatus((s) => {
      if (s.state !== 'ready' || !graphEnabled) return;
      if (root && s.root !== root) return;
      void refresh();
    }).then((un) => (unsubStatus = un));
  });

  onDestroy(() => {
    stopLoop();
    stopHistoryPoll();
    ro?.disconnect();
    io?.disconnect();
    document.removeEventListener('visibilitychange', handleVisChange);
    window.removeEventListener('keydown', onWindowKeyDown);
    window.removeEventListener('mousemove', onWindowMouseMove);
    window.removeEventListener('mouseup', onWindowMouseUp);
    if (resizeDebounce) clearTimeout(resizeDebounce);
    if (revealMissTimer) clearTimeout(revealMissTimer);
    unsubSettings?.();
    unsubStatus?.();
  });
</script>

<div class="graph-view" bind:this={containerEl}>
  <header class="controls">
    <div class="search">
      <input
        type="text"
        placeholder="Find file…"
        bind:value={searchQuery}
        oninput={() => (searchSel = 0)}
        onkeydown={onSearchKey}
        disabled={nodeCount === 0}
      />
      {#if searchMatches.length > 0}
        <ul class="search-results">
          {#each searchMatches as m, i (m.id)}
            <li>
              <button type="button" class:sel={i === searchSel} title={m.file} onclick={() => pickSearch(m)}>
                <span class="sr-file">{m.file}</span>
                <span class="sr-deg tnum">{m.degree}</span>
              </button>
            </li>
          {/each}
        </ul>
      {/if}
    </div>
    <button type="button" class="secondary" onclick={resetView} disabled={nodeCount === 0}>Reset view</button>
    <button type="button" class="secondary" onclick={() => void refresh()} disabled={loading || !graphEnabled}>
      {loading ? 'Loading…' : 'Refresh'}
    </button>
    <span class="counts tnum">
      {nodeCount} nodes · {edgeCount} edges{nodeCount >= vizMax ? ` · top ${vizMax} hubs by degree` : ''}
    </span>
  </header>

  <!-- The canvas stays mounted through every state (banners overlay it):
       swapping it out on each refresh recreated the element + 2D context and,
       worse, meant the view could never be size-fitted before first paint. -->
  <div class="canvas-wrap">
    <canvas
      bind:this={canvasEl}
      class="graph-canvas"
      onmousedown={onCanvasMouseDown}
      onmousemove={onCanvasMouseMove}
      onmouseleave={onCanvasMouseLeave}
      onwheel={onWheel}
      oncontextmenu={(e) => e.preventDefault()}
    ></canvas>

    {#if !graphEnabled}
      <p class="banner">
        Graph View is disabled. Turn on the code graph (Settings → Code Intelligence) to use it.
      </p>
    {:else if fetchError}
      <p class="banner err">
        Couldn't load the graph: {fetchError}
        <button type="button" class="secondary" onclick={() => void refresh()}>Retry</button>
      </p>
    {:else if loading && nodeCount === 0}
      <p class="banner">Loading graph…</p>
    {:else if nodeCount === 0}
      <p class="banner">No indexed graph yet. Build the code graph from the Code Intelligence tab first.</p>
    {:else if revealMiss}
      <p class="banner">{revealMiss}</p>
    {/if}

    {#if nodeCount > 0}
      {#if hoveredNode}
        <div class="tooltip" style="left:{hoverPos.x + 14}px; top:{hoverPos.y + 14}px;">
          <strong>{hoveredNode.file}</strong>
          <div>{hoveredNode.subsystem || '(uncategorized)'} · degree {hoveredNode.degree}</div>
        </div>
      {/if}

      {#if selectedNode}
        <div class="conn-panel">
          <div class="conn-head">
            <button type="button" class="nav" onclick={hopBack} disabled={!canBack} title="Back (Backspace)">‹</button>
            <button type="button" class="nav" onclick={hopForward} disabled={!canForward} title="Forward">›</button>
            <div class="conn-title">
              <strong title={selectedNode.file}>{basename(selectedNode.file)}</strong>
              <div class="conn-path">{selectedNode.file}</div>
              <div class="conn-meta">
                <span
                  class="swatch"
                  style="background:{selectedNode.subsystem ? subsystemColor(selectedNode.subsystem) : UNCATEGORIZED_COLOR}"
                ></span>
                {selectedNode.subsystem || '(uncategorized)'} · degree {selectedNode.degree}
              </div>
            </div>
            <button type="button" class="nav" onclick={clearSelection} title="Clear selection (Esc)">✕</button>
          </div>
          <div class="conn-body">
            {#each connGroups as g (g.title)}
              {#if g.rows.length > 0}
                <div class="legend-section">
                  <h4>{g.title}</h4>
                  {#each g.rows as nb, i (g.title + nb.id + i)}
                    <button type="button" class="conn-row" title={nb.file} onclick={() => selectNode(nb)}>
                      <span class="line" style="background:{edgeColor(g.kind)}"></span>
                      <span class="conn-file">{basename(nb.file)}</span>
                    </button>
                  {/each}
                </div>
              {/if}
            {/each}
            {#if shownConnCount === 0}
              <div class="conn-note">no drawn connections for this file</div>
            {:else if shownConnCount < selectedNode.degree}
              <div class="conn-note">strongest {shownConnCount} of ~{selectedNode.degree} connections shown</div>
            {/if}
          </div>
        </div>
      {:else if selectedDirCluster}
        <div class="conn-panel">
          <div class="conn-head">
            <button type="button" class="nav" onclick={hopBack} disabled={!canBack} title="Back (Backspace)">‹</button>
            <button type="button" class="nav" onclick={hopForward} disabled={!canForward} title="Forward">›</button>
            <div class="conn-title">
              <strong title={selectedDirCluster.name}>{selectedDirCluster.name}</strong>
              <div class="conn-meta">{dirMembers.length} files · {dirIntraCount} internal links</div>
            </div>
            <button type="button" class="nav" onclick={clearSelection} title="Clear selection (Esc)">✕</button>
          </div>
          <div class="conn-body">
            <div class="legend-section">
              <h4>files</h4>
              {#each dirMembers as m (m.id)}
                <button type="button" class="conn-row" title={m.file} onclick={() => selectNode(m)}>
                  <span class="swatch" style="background:{m.subsystem ? subsystemColor(m.subsystem) : UNCATEGORIZED_COLOR}"></span>
                  <span class="conn-file">{basename(m.file)}</span>
                  <span class="sr-deg tnum">{m.degree}</span>
                </button>
              {/each}
            </div>
          </div>
        </div>
      {/if}

      <div class="legend" class:collapsed={!legendOpen}>
        <button type="button" class="legend-toggle" onclick={() => (legendOpen = !legendOpen)}>
          Legend {legendOpen ? '▾' : '▸'}
        </button>
        {#if legendOpen}
          <div class="legend-body">
            {#if subsystemsPresent.length > 0 || hasUncategorized}
              <div class="legend-section">
                <h4>Subsystem (node color)</h4>
                {#each subsystemsPresent as s (s)}
                  <div class="legend-row"><span class="swatch" style="background:{subsystemColor(s)}"></span>{s}</div>
                {/each}
                {#if hasUncategorized}
                  <div class="legend-row"><span class="swatch" style="background:{UNCATEGORIZED_COLOR}"></span>(uncategorized)</div>
                {/if}
              </div>
            {/if}
            <div class="legend-section">
              <h4>Node (one per file)</h4>
              <div class="legend-row">size = call/import degree</div>
            </div>
            {#if edgeKindsPresent.length > 0}
              <div class="legend-section">
                <h4>Edge color (kind)</h4>
                {#each edgeKindsPresent as k (k)}
                  <div class="legend-row"><span class="line" style="background:{edgeColor(k)}"></span>{k}</div>
                {/each}
              </div>
            {/if}
            {#if edgeConfsPresent.length > 0}
              <div class="legend-section">
                <h4>Edge dash (confidence)</h4>
                {#each edgeConfsPresent as c (c)}
                  <div class="legend-row">
                    <span
                      class="dashline"
                      class:solid={c === 'extracted'}
                      class:dashed={c === 'inferred'}
                      class:dotted={c === 'ambiguous'}
                    ></span>{c}
                  </div>
                {/each}
              </div>
            {/if}
            {#if sawCloudPulse || sawLocalPulse || sawAdvisorPulse}
              <div class="legend-section">
                <h4>Live activity pulse</h4>
                {#if sawCloudPulse}
                  <div class="legend-row"><span class="swatch" style="background:{PULSE_CLOUD}"></span>cloud agent (Claude / OpenCode)</div>
                {/if}
                {#if sawLocalPulse}
                  <div class="legend-row"><span class="swatch" style="background:{PULSE_LOCAL}"></span>local offload worker</div>
                {/if}
                {#if sawAdvisorPulse}
                  <div class="legend-row"><span class="swatch" style="background:{PULSE_ADVISOR}"></span>advisor / auto-check (background)</div>
                {/if}
              </div>
            {/if}
          </div>
        {/if}
      </div>
    {/if}
  </div>
</div>

<style>
  .graph-view {
    /* Sit ABOVE the pane's absolutely-positioned (empty) terminal slot —
       same convention as WorkbenchView/CodeIntelligenceView. */
    position: absolute;
    inset: 0;
    display: flex;
    flex-direction: column;
    color: var(--text, #ddd);
    font-size: 13px;
    box-sizing: border-box;
  }
  header.controls {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 10px 12px;
    border-bottom: 1px solid var(--border, #333);
    flex: 0 0 auto;
  }
  .search {
    position: relative;
    flex: 0 1 260px;
    min-width: 140px;
  }
  .search input {
    width: 100%;
    box-sizing: border-box;
    background: var(--panel, #1e1e1e);
    border: 1px solid var(--border, #444);
    color: var(--text, #ddd);
    border-radius: 5px;
    padding: 4px 8px;
    font-size: 12px;
  }
  .search input:disabled {
    opacity: 0.5;
  }
  .search-results {
    position: absolute;
    top: calc(100% + 4px);
    left: 0;
    right: 0;
    z-index: 8;
    margin: 0;
    padding: 4px;
    list-style: none;
    background: var(--panel, #1e1e1e);
    border: 1px solid var(--border, #444);
    border-radius: 6px;
    box-shadow: 0 6px 16px rgba(0, 0, 0, 0.45);
    max-height: 280px;
    overflow-y: auto;
  }
  .search-results button {
    display: flex;
    width: 100%;
    gap: 8px;
    align-items: baseline;
    border: none;
    background: transparent;
    color: var(--text, #ddd);
    padding: 4px 6px;
    border-radius: 4px;
    cursor: pointer;
    font-size: 12px;
    text-align: left;
  }
  .search-results button.sel,
  .search-results button:hover {
    background: var(--accent, #3b6ea5);
    color: #fff;
  }
  .sr-file {
    flex: 1 1 auto;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .sr-deg {
    flex: 0 0 auto;
    opacity: 0.65;
    font-size: 11px;
  }
  button.secondary {
    padding: 4px 10px;
    border-radius: 5px;
    border: 1px solid var(--border, #444);
    background: transparent;
    color: var(--text, #ddd);
    cursor: pointer;
    font-size: 12px;
  }
  button.secondary:disabled {
    opacity: 0.5;
    cursor: default;
  }
  .counts {
    margin-left: auto;
    opacity: 0.7;
    font-size: 12px;
  }
  .canvas-wrap {
    position: relative;
    flex: 1 1 auto;
    overflow: hidden;
  }
  .graph-canvas {
    display: block;
    width: 100%;
    height: 100%;
    cursor: grab;
  }
  .banner {
    /* Overlays the always-mounted canvas (above the tooltip's z-index 5). */
    position: absolute;
    top: 16px;
    left: 16px;
    right: 16px;
    z-index: 6;
    margin: 0;
    padding: 8px 10px;
    border-radius: 6px;
    font-size: 12px;
    border: 1px solid var(--border, #444);
    background: var(--panel, #1e1e1e);
    opacity: 0.95;
  }
  .banner.err {
    background: rgba(179, 38, 30, 0.18);
    border-color: #b3261e;
    color: #ffb4ab;
  }
  .banner.err button {
    margin-left: 8px;
  }
  .tooltip {
    position: absolute;
    z-index: 5;
    pointer-events: none;
    max-width: 320px;
    padding: 6px 8px;
    border-radius: 5px;
    border: 1px solid var(--border, #444);
    background: var(--panel, #1e1e1e);
    color: var(--text, #ddd);
    font-size: 11px;
    line-height: 1.4;
    box-shadow: 0 4px 12px rgba(0, 0, 0, 0.4);
    word-break: break-all;
  }
  .tooltip strong {
    display: block;
    font-size: 12px;
  }
  .conn-panel {
    position: absolute;
    right: 10px;
    top: 10px;
    z-index: 5;
    width: 260px;
    max-width: 45%;
    max-height: calc(100% - 20px);
    display: flex;
    flex-direction: column;
    border-radius: 6px;
    border: 1px solid var(--border, #444);
    background: var(--panel, #1e1e1e);
    color: var(--text, #ddd);
    font-size: 11px;
    box-shadow: 0 4px 12px rgba(0, 0, 0, 0.35);
  }
  .conn-head {
    display: flex;
    align-items: flex-start;
    gap: 4px;
    padding: 8px;
    border-bottom: 1px solid var(--border, #444);
  }
  .conn-head .nav {
    flex: 0 0 auto;
    width: 20px;
    height: 20px;
    padding: 0;
    line-height: 1;
    border: 1px solid var(--border, #444);
    border-radius: 4px;
    background: transparent;
    color: var(--text, #ddd);
    font-size: 12px;
    cursor: pointer;
  }
  .conn-head .nav:disabled {
    opacity: 0.35;
    cursor: default;
  }
  .conn-head .nav:not(:disabled):hover {
    background: var(--accent, #3b6ea5);
    color: #fff;
  }
  .conn-title {
    flex: 1 1 auto;
    min-width: 0;
  }
  .conn-title strong {
    display: block;
    font-size: 12px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .conn-path {
    opacity: 0.65;
    word-break: break-all;
    margin-top: 1px;
  }
  .conn-meta {
    display: flex;
    align-items: center;
    gap: 5px;
    margin-top: 3px;
    opacity: 0.85;
  }
  .conn-body {
    padding: 0 8px 8px;
    /* ~8 rows visible; the rest scrolls so the box never grows huge. */
    max-height: 200px;
    overflow-y: auto;
  }
  .conn-row {
    display: flex;
    align-items: center;
    gap: 6px;
    width: 100%;
    border: none;
    background: transparent;
    color: var(--text, #ddd);
    padding: 2px;
    border-radius: 4px;
    cursor: pointer;
    font-size: 11px;
    text-align: left;
  }
  .conn-row:hover {
    background: var(--accent, #3b6ea5);
    color: #fff;
  }
  .conn-file {
    flex: 1 1 auto;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .conn-note {
    margin-top: 6px;
    opacity: 0.55;
    font-style: italic;
  }
  .legend {
    position: absolute;
    left: 10px;
    bottom: 10px;
    z-index: 4;
    max-width: 240px;
    border-radius: 6px;
    border: 1px solid var(--border, #444);
    background: var(--panel, #1e1e1e);
    color: var(--text, #ddd);
    font-size: 11px;
    box-shadow: 0 4px 12px rgba(0, 0, 0, 0.35);
  }
  .legend-toggle {
    display: block;
    width: 100%;
    text-align: left;
    padding: 6px 10px;
    border: none;
    background: transparent;
    color: var(--text, #ddd);
    font-size: 11px;
    font-weight: 600;
    cursor: pointer;
  }
  .legend-body {
    padding: 0 10px 8px;
    max-height: 300px;
    overflow-y: auto;
  }
  .legend-section {
    margin-top: 6px;
  }
  .legend-section h4 {
    margin: 0 0 4px;
    font-size: 10px;
    text-transform: uppercase;
    letter-spacing: 0.03em;
    opacity: 0.65;
  }
  .legend-row {
    display: flex;
    align-items: center;
    gap: 6px;
    padding: 1px 0;
  }
  .swatch {
    width: 10px;
    height: 10px;
    border-radius: 50%;
    flex: 0 0 auto;
    display: inline-block;
  }
  .line {
    width: 16px;
    height: 3px;
    border-radius: 1px;
    flex: 0 0 auto;
    display: inline-block;
  }
  .dashline {
    width: 16px;
    height: 0;
    flex: 0 0 auto;
    display: inline-block;
    border-top: 2px solid var(--text, #ddd);
  }
  .dashline.solid {
    border-top-style: solid;
  }
  .dashline.dashed {
    border-top-style: dashed;
  }
  .dashline.dotted {
    border-top-style: dotted;
  }
</style>
