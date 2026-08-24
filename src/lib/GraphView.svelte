<script lang="ts">
  // V15 Feature 4 (stretch): a live, interactive 3D visualization of the
  // project's code graph. Renders the bounded `graphVizSnapshot()` subgraph
  // (FILE-level: files are the only nodes; calls are rolled up to file→file
  // backend-side — symbol nodes were too many to render or read) with a
  // small self-contained force simulation (no external deps — plain
  // Canvas 2D + TypeScript), and pulses nodes as the graph tool history shows
  // an agent (cloud or the local offload worker) touching them, so a viewer
  // can watch the agent "walk" the codebase live. Formerly its own reserved
  // tab (retired in schema v26) — now the "Graph view" section inside the
  // Tool Activity tab, mounted lazily and kept alive hidden on section
  // switches (ToolActivityView); the IntersectionObserver below pauses the
  // render loop whenever the section (or tab) is off-screen.
  import { onMount, onDestroy } from 'svelte';
  import {
    graphVizSnapshot,
    graphVizEgo,
    graphHistory,
    onGraphStatus,
    type VizGraphResult,
    type VizEdgeRow,
    type GraphCall,
  } from './graph';
  import { settings } from './settings/store';
  import type { Settings } from './settings/types';
  import { graphReveal, clearGraphReveal } from './graphReveal';
  import { findHarness, harnesses, harnessLabels } from './harness';
  import {
    clamp,
    defaultTuning,
    dirOf,
    emptyWorld,
    initPosition,
    leashFor,
    nodeRadius,
    simulate,
    SIM_MAX_MS,
    type DirCluster,
    type DirEdge,
    type SimEdge,
    type SimNode,
  } from './graph/sim';
  import {
    computeCameraBasis,
    defaultCamera,
    maxDist3d,
    MIN_DIST_3D,
    project,
  } from './graph/camera';
  import {
    basename,
    drawScene,
    EDGE_CALL,
    EDGE_IMPORT,
    edgeColor,
    FOCUS_MS,
    PULSE_ADVISOR,
    PULSE_CLOUD,
    PULSE_LOCAL,
    PULSE_MS,
    subsystemColor,
    UNCATEGORIZED_COLOR,
    visibleDirNames,
    visibleNodeIds,
    type Scene,
  } from './graph/render';

  // Mirrors WorkbenchView/CodeIntelligenceView: an optional project root:
  // neither currently receives one from `Pane.svelte` (both default to the
  // launch directory backend-side), so this stays optional too and is only
  // threaded through to `graphVizSnapshot` when the caller supplies one.
  let { root = undefined }: { root?: string } = $props();

  // The chart palette, the force constants, the projection and the canvas
  // renderer moved to `lib/graph/{sim,camera,render}.ts` (#130). What is left
  // in this file is the COMPONENT: data loading, selection and history, the
  // pointer handlers, the visibility plumbing, and the rAF loop that drives
  // the three of them.
  /// Who the cloud swatch stands for, named from the registry rather than from
  /// a hand-kept pair — the legend lists the harnesses this build actually has.
  const cloudHarnessLabels = $derived(
    harnessLabels($harnesses.filter((h) => h.affordances.tier === 'cloud')),
  );

  // Settings-driven tuning (Settings → Code Intelligence → Graph view → Graph
  // view tuning). Multipliers on the engine's base constants — one size doesn't
  // fit every repo. The numeric knobs are read at frame rate by the physics and
  // render loops, so they live in a plain object updated by the settings
  // subscription (applyTuning), NOT in $state; the two edge colors ARE $state
  // because the legend/connections templates render them too.
  const tune = defaultTuning();
  let edgeCallColor = $state(EDGE_CALL);
  let edgeImportColor = $state(EDGE_IMPORT);

  // ── DOM refs ─────────────────────────────────────────────────────────────
  // oxlint-disable-next-line no-unassigned-vars -- assigned via bind:this
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

  // ── Non-reactive simulation state ────────────────────────────────────────
  // Every position, velocity and force the frame loop touches, plus the
  // cooling factor. Kept OUT of `$state` so a position update doesn't enter
  // Svelte's reactivity graph — `simulate` writes nine floats per node per
  // frame, and `project` four more. ONE object for the component's lifetime
  // (`buildSim` replaces its arrays, never the object), handed by reference to
  // `simulate`, `project` and the renderer, none of which allocate per frame.
  // `tests/graphEngineBoundary.test.ts` enforces the `const`-and-no-rune half
  // of that; `graph/sim.test.ts` enforces the mutate-in-place half.
  const world = emptyWorld();
  let nodeById = new Map<string, SimNode>();
  // Incident (drawn) edges per node id — the connections panel + ego
  // highlight read this instead of scanning the whole edge list.
  let edgesByNode = new Map<string, SimEdge[]>();
  let clusterByDir = new Map<string, DirCluster>();
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

  // View transform. Plain object, mutated in place by the drag/wheel/fit
  // handlers and read by `project` each frame — same non-reactive contract as
  // `world` above.
  const cam = defaultCamera();

  // Animation / interaction bookkeeping.
  let idle = true;
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


  // ── Selection / search / history (derived) ──────────────────────────────
  const selectedNode = $derived.by(() => {
    void simVersion; // re-resolve against the rebuilt node set
    return selectedNodeId ? (nodeById.get(selectedNodeId) ?? null) : null;
  });
  const searchMatches = $derived.by(() => {
    void simVersion;
    const q = searchQuery.trim().toLowerCase();
    if (!q) return [];
    return world.nodes
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
    for (const e of world.edges) if (e.src.dir === c.name && e.dst.dir === c.name) n++;
    return n;
  });

  // ── Rendering ─────────────────────────────────────────────────────────────
  // The drawing itself is `lib/graph/render.ts`. This component owns the
  // canvas, the projection call and the frame clock; the scene below is the
  // bundle the renderer reads, held for the view's lifetime so that a frame
  // allocates nothing.
  //
  // The `$state`-backed fields are COPIED in on each sync rather than read
  // through a getter: the renderer runs from `requestAnimationFrame`, outside
  // any reactive scope, where a live read buys no reactivity and would only
  // put a lookup on the hot path.
  const scene: Scene = {
    world,
    nodeById,
    edgesByNode,
    clusterByDir,
    selectedNodeId: null,
    selectedDir: null,
    hoveredNode: null,
    focusRingUntil: 0,
    // Seeded with the shipped defaults rather than with the two `$state`
    // colours: reading them here would capture their value at construction
    // and never update (Svelte warns about exactly that), and `syncScene`
    // below is what keeps every one of these fields current.
    edgeCallColor: EDGE_CALL,
    edgeImportColor: EDGE_IMPORT,
    tune,
  };

  /// Refresh the scene's view of this component's state and hand it back.
  /// Called before every draw and by both pickers, which share the renderer's
  /// visibility rules — a node you cannot see must not be clickable either.
  function syncScene(): Scene {
    scene.nodeById = nodeById;
    scene.edgesByNode = edgesByNode;
    scene.clusterByDir = clusterByDir;
    scene.selectedNodeId = selectedNodeId;
    scene.selectedDir = selectedDir;
    scene.hoveredNode = hoveredNode;
    scene.focusRingUntil = focusRingUntil;
    scene.edgeCallColor = edgeCallColor;
    scene.edgeImportColor = edgeImportColor;
    return scene;
  }

  function render(): void {
    if (!ctx || cam.viewW <= 0 || cam.viewH <= 0) return;
    project(cam, world, tune.nodeScale);
    ctx.clearRect(0, 0, cam.viewW, cam.viewH);
    drawScene(ctx, syncScene(), performance.now());
  }
  // ── Animation loop ───────────────────────────────────────────────────────
  function decayActive(now: number): boolean {
    let active = false;
    for (const n of world.nodes) if (n.pulseUntil > now) { active = true; break; }
    if (!active) for (const e of world.edges) if (e.highlightUntil > now) { active = true; break; }
    if (!active && selectedNodeId && focusRingUntil > now) active = true;
    return active;
  }

  function stepViewAnimation(dt: number): boolean {
    if (!focusTarget && focusDist === null) return false;
    const k = 1 - Math.exp(-dt * 0.008);
    let moving = false;
    if (focusTarget) {
      cam.targetX += (focusTarget.x - cam.targetX) * k;
      cam.targetY += (focusTarget.y - cam.targetY) * k;
      cam.targetZ += (focusTarget.z - cam.targetZ) * k;
      const d = Math.hypot(focusTarget.x - cam.targetX, focusTarget.y - cam.targetY, focusTarget.z - cam.targetZ);
      if (d < 0.5) {
        cam.targetX = focusTarget.x; cam.targetY = focusTarget.y; cam.targetZ = focusTarget.z; focusTarget = null;
      } else {
        moving = true;
      }
    }
    if (focusDist !== null) {
      cam.dist += (focusDist - cam.dist) * k;
      if (Math.abs(focusDist - cam.dist) < 1) {
        cam.dist = focusDist; focusDist = null;
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
      cam.theta += orbitVelTheta * dt;
      cam.phi = clamp(cam.phi + orbitVelPhi * dt, -1.45, 1.45);
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
        for (const n of world.nodes) { n.vx = 0; n.vy = 0; n.vz = 0; }
      } else {
        const moving = simulate(world, tune, dt);
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
    if (!visible || world.nodes.length === 0) return;
    if (running) return;
    running = true;
    lastTs = 0;
    rafId = requestAnimationFrame(loop);
  }
  // Re-heat the physics. Only data/layout changes call this — pointer and
  // wheel handlers call wake() instead, so moving the camera never re-boils
  // a settled layout.
  function kick(heat = 1): void {
    world.alpha = Math.max(world.alpha, heat);
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
      const r = nodeRadius(row.degree || 0, tune.nodeScale);
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
        leash: leashFor(members.length, tune.dirScale),
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

    world.nodes = newNodes;
    world.edges = newEdges;
    nodeById = newById;
    edgesByNode = newByNode;
    world.clusters = newClusters;
    clusterByDir = newClusterByDir;
    world.dirEdges = newDirEdges;
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
      needsFit = cam.viewW <= 0 || cam.viewH <= 0;
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
    if (world.nodes.length === 0) return;
    let cx = 0, cy = 0, cz = 0;
    for (const n of world.nodes) { cx += n.x; cy += n.y; cz += n.z; }
    cx /= world.nodes.length; cy /= world.nodes.length; cz /= world.nodes.length;
    let maxR = 0;
    for (const n of world.nodes) maxR = Math.max(maxR, Math.hypot(n.x - cx, n.y - cy, n.z - cz));
    cam.targetX = cx; cam.targetY = cy; cam.targetZ = cz;
    cam.theta = 0.6; cam.phi = 0.35;
    cam.dist = clamp(maxR * 2.2 + 80, MIN_DIST_3D, maxDist3d(tune.clusterSpacing));
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
    focusDist = clamp(maxR * 2.4 + 60, MIN_DIST_3D, maxDist3d(tune.clusterSpacing));
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
    if (!pendingReveal || world.nodes.length === 0) return;
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
    const exact = world.nodes.filter((n) => n.file === target);
    if (exact.length) return exact.slice(0, 3);
    const bare = target.split(':')[0];
    const byFile = world.nodes.filter((n) => n.file === bare || bare.endsWith(n.file) || n.file.endsWith(bare));
    return byFile.slice(0, 3);
  }

  function applyActivity(call: GraphCall): void {
    const matched = matchNodes(call.target);
    if (matched.length === 0) return;
    // V40 Phase F (locked decision 27): a call's source is in the cloud bucket
    // when the registry says the harness that made it runs there — `tier`, not
    // a list of the harnesses this build happens to ship. A source no harness
    // declares (an offload backend, an internal service) falls through to the
    // buckets below, which is what it did before.
    const isCloud = findHarness($harnesses, call.source)?.affordances.tier === 'cloud';
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
        for (const e of world.edges) {
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
    // Gated on `visible`, per the app-view contract (appViews.ts): this
    // component is kept alive for the app's lifetime, so an ungated tick polled
    // the backend every 1.5s forever — including while the Graph view sub-tab
    // was off-screen and every result was discarded. `updateVisibility` re-seeds
    // on the way back so the skipped interval doesn't replay as a burst.
    historyTimer = setInterval(() => {
      if (visible) void pollHistory();
    }, 1500);
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
    // `tune` is the object the frame loop holds by reference — write the new
    // knobs INTO it rather than swapping it out from under `simulate`,
    // `project` and the scene.
    Object.assign(tune, next);
    edgeCallColor = nextCall;
    edgeImportColor = nextImport;
    if ((!geomChanged && !lookChanged) || world.nodes.length === 0) return;
    for (const n of world.nodes) n.r = nodeRadius(n.degree, tune.nodeScale);
    for (const c of world.clusters) c.leash = leashFor(c.members.length, tune.dirScale);
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
    cam.viewW = w; cam.viewH = h;
    // ~58° vertical FOV regardless of window size (a fixed focal length made
    // the effective FOV a function of how big the pane happened to be).
    cam.focal = Math.max(200, cam.viewH * 0.9);
    updateVisibility();
    if (needsFit && world.nodes.length > 0) {
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
    if (vis && !visible) {
      visible = true;
      // Re-seed rather than replay: the history poll below is skipped while
      // parked, so the high-water mark is stale on return and the next fetch
      // would hand `applyActivity` every call that landed meanwhile as a burst
      // of pulses. Seeding advances the mark and applies nothing — the same
      // "no replayed backlog" contract the parked path used to get by polling
      // through the whole time it was hidden.
      historySeeded = false;
      wake();
    } else if (!vis && visible) {
      visible = false;
      stopLoop();
    }
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
    const vis = visibleNodeIds(syncScene());
    let best: SimNode | null = null;
    let bestZ = Infinity;
    for (const n of world.nodes) {
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
    const dirs = visibleDirNames(syncScene());
    let best: DirCluster | null = null;
    let bestZ = Infinity;
    for (const c of world.clusters) {
      if (c.sz < 0 || c.discR <= 0) continue;
      if (dirs && !dirs.has(c.name)) continue;
      const d = Math.hypot(mx - c.sx, my - c.sy);
      if (d <= c.discR && c.sz < bestZ) { best = c; bestZ = c.sz; }
    }
    return best;
  }

  function onCanvasMouseDown(e: MouseEvent): void {
    if (world.nodes.length === 0) return;
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
      const b = computeCameraBasis(cam);
      const worldPerPx = cam.dist / cam.focal;
      cam.targetX -= (b.xx * dx - b.yx * dy) * worldPerPx;
      cam.targetY -= (b.xy * dx - b.yy * dy) * worldPerPx;
      cam.targetZ -= (b.xz * dx - b.yz * dy) * worldPerPx;
    } else {
      cam.theta -= dx * 0.006;
      cam.phi = clamp(cam.phi - dy * 0.006, -1.45, 1.45);
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
    if (world.nodes.length === 0 || !canvasEl) return;
    e.preventDefault();
    cam.dist = clamp(cam.dist * Math.exp(e.deltaY * 0.0015), MIN_DIST_3D, maxDist3d(tune.clusterSpacing));
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
      <p class="banner">No indexed graph yet. Build the code graph first (Tools → Graph index → Rebuild index).</p>
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
                      <span class="line" style="background:{edgeColor(g.kind, edgeCallColor, edgeImportColor)}"></span>
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
                  <div class="legend-row"><span class="line" style="background:{edgeColor(k, edgeCallColor, edgeImportColor)}"></span>{k}</div>
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
                  <div class="legend-row"><span class="swatch" style="background:{PULSE_CLOUD}"></span>cloud agent ({cloudHarnessLabels})</div>
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
    /* Fill the host: the Tool Activity tab's `.graph-host` (position:
       relative, flex-grown to the remaining pane height) — kept from the
       retired-tab days, when this filled the pane directly. */
    position: absolute;
    inset: 0;
    display: flex;
    flex-direction: column;
    color: var(--text-primary, #ddd);
    font-size: 13px;
    box-sizing: border-box;
  }
  header.controls {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 10px 12px;
    border-bottom: 1px solid var(--border-subtle, #333);
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
    background: var(--surface-input, #1e1e1e);
    border: 1px solid var(--border-default, #444);
    color: var(--text-primary, #ddd);
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
    background: var(--surface-2, #1e1e1e);
    border: 1px solid var(--border-default, #444);
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
    color: var(--text-primary, #ddd);
    padding: 4px 6px;
    border-radius: 4px;
    cursor: pointer;
    font-size: 12px;
    text-align: left;
  }
  .search-results button.sel,
  .search-results button:hover {
    background: var(--accent, #3b6ea5);
    color: var(--accent-fg, #fff);
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
    border: 1px solid var(--border-default, #444);
    background: transparent;
    color: var(--text-primary, #ddd);
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
    border: 1px solid var(--border-default, #444);
    background: var(--surface-2, #1e1e1e);
    opacity: 0.95;
  }
  .banner.err {
    background: var(--surface-danger, rgba(179, 38, 30, 0.18));
    border-color: var(--border-danger, #b3261e);
    color: var(--text-danger-soft, #ffb4ab);
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
    border: 1px solid var(--border-default, #444);
    background: var(--surface-2, #1e1e1e);
    color: var(--text-primary, #ddd);
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
    border: 1px solid var(--border-default, #444);
    background: var(--surface-2, #1e1e1e);
    color: var(--text-primary, #ddd);
    font-size: 11px;
    box-shadow: 0 4px 12px rgba(0, 0, 0, 0.35);
  }
  .conn-head {
    display: flex;
    align-items: flex-start;
    gap: 4px;
    padding: 8px;
    border-bottom: 1px solid var(--border-default, #444);
  }
  .conn-head .nav {
    flex: 0 0 auto;
    width: 20px;
    height: 20px;
    padding: 0;
    line-height: 1;
    border: 1px solid var(--border-default, #444);
    border-radius: 4px;
    background: transparent;
    color: var(--text-primary, #ddd);
    font-size: 12px;
    cursor: pointer;
  }
  .conn-head .nav:disabled {
    opacity: 0.35;
    cursor: default;
  }
  .conn-head .nav:not(:disabled):hover {
    background: var(--accent, #3b6ea5);
    color: var(--accent-fg, #fff);
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
    color: var(--text-primary, #ddd);
    padding: 2px;
    border-radius: 4px;
    cursor: pointer;
    font-size: 11px;
    text-align: left;
  }
  .conn-row:hover {
    background: var(--accent, #3b6ea5);
    color: var(--accent-fg, #fff);
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
    border: 1px solid var(--border-default, #444);
    background: var(--surface-2, #1e1e1e);
    color: var(--text-primary, #ddd);
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
    color: var(--text-primary, #ddd);
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
    border-top: 2px solid var(--text-primary, #ddd);
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
