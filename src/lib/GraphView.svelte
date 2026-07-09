<script lang="ts">
  // V15 Feature 4 (stretch): a live, interactive 2D/3D visualization of the
  // project's code graph. Renders the bounded `graphVizSnapshot()` subgraph
  // with a small self-contained force simulation (no external deps — plain
  // Canvas 2D + TypeScript), and pulses nodes as the graph tool history shows
  // an agent (cloud or the local offload worker) touching them, so a viewer
  // can watch the agent "walk" the codebase live. Mirrors the app-rendered
  // (no PTY) tab pattern used by WorkbenchView/CodeIntelligenceView —
  // `position:absolute;inset:0` so it sits above the pane's terminal slot.
  import { onMount, onDestroy } from 'svelte';
  import {
    graphVizSnapshot,
    graphHistory,
    type VizGraphResult,
    type VizNodeRow,
    type GraphCall,
  } from './graph';
  import { settings } from './settings/store';

  // Mirrors WorkbenchView/CodeIntelligenceView: an optional project root:
  // neither currently receives one from `Pane.svelte` (both default to the
  // launch directory backend-side), so this stays optional too and is only
  // threaded through to `graphVizSnapshot` when the caller supplies one.
  let { root = undefined }: { root?: string } = $props();

  // ── Chart-specific colors (not app design tokens — Okabe-Ito categorical
  // palette, colorblind-safe; node shape (circle/square) and edge dash
  // pattern carry redundant non-hue channels alongside these). ────────────
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
  const ACCENT_COLOR = '#bb55ff';
  const PULSE_MS = 600;
  const FOCUS_MS = 900;

  const MIN_ZOOM_2D = 0.05;
  const MAX_ZOOM_2D = 8;
  const MIN_DIST_3D = 80;
  const MAX_DIST_3D = 4000;
  const NEAR_3D = 8;

  // Physics tuning. O(n^2) repulsion is fine up to a few hundred nodes (the
  // backend caps the snapshot around ~1500); above MAX_REPEL_FULL we sample a
  // bounded number of partners per node per frame instead of every pair, so a
  // large graph never blows the frame budget.
  const REPULSION_K = 2600;
  const SPRING_K = 0.02;
  const SPRING_LEN = 70;
  const GRAVITY_K = 0.0025;
  const DAMPING = 0.85;
  const IDLE_KE_THRESHOLD = 0.05;
  const MAX_REPEL_FULL = 500;
  const REPEL_SAMPLE = 40;
  const REPEL_CUTOFF_SQ = 90000; // 300 world units

  interface SimNode extends VizNodeRow {
    x: number; y: number; z: number;
    vx: number; vy: number; vz: number;
    fx: number; fy: number; fz: number;
    r: number;
    sx: number; sy: number; sr: number; sz: number;
    pulseStart: number; pulseUntil: number; pulseColor: string | null;
  }
  interface SimEdge {
    src: SimNode; dst: SimNode;
    kind: string; confidence: string;
    highlightUntil: number; highlightColor: string | null;
  }

  // ── DOM refs ─────────────────────────────────────────────────────────────
  let containerEl: HTMLDivElement;
  let canvasEl: HTMLCanvasElement;
  let ctx: CanvasRenderingContext2D | null = null;

  // ── Reactive UI state ────────────────────────────────────────────────────
  let mode = $state<'2d' | '3d'>('2d');
  let loading = $state(true);
  let fetchError = $state<string | null>(null);
  let nodeCount = $state(0);
  let edgeCount = $state(0);
  let legendOpen = $state(true);
  let graphEnabled = $state(false);
  let subsystemsPresent = $state<string[]>([]);
  let hasUncategorized = $state(false);
  let edgeKindsPresent = $state<string[]>([]);
  let edgeConfsPresent = $state<string[]>([]);
  let sawCloudPulse = $state(false);
  let sawLocalPulse = $state(false);
  let hoveredNode = $state<SimNode | null>(null);
  let hoverPos = $state<{ x: number; y: number }>({ x: 0, y: 0 });
  let focusedNodeId = $state<string | null>(null);

  // ── Non-reactive simulation state (mutated at animation-frame rate — kept
  // out of $state so every position update doesn't trigger Svelte's
  // reactivity graph). ─────────────────────────────────────────────────────
  let nodes: SimNode[] = [];
  let edges: SimEdge[] = [];
  let nodeById = new Map<string, SimNode>();
  let everEntered3D = false;

  // View transform.
  let viewW = 0;
  let viewH = 0;
  let zoom2D = 1;
  let viewCenterX = 0;
  let viewCenterY = 0;
  let camTheta = 0.6;
  let camPhi = 0.35;
  let camDist = 600;
  let camFocal = 480;
  let camTargetX = 0;
  let camTargetY = 0;
  let camTargetZ = 0;

  // Animation / interaction bookkeeping.
  let idle = true;
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
  let panVelX = 0;
  let panVelY = 0;
  let orbitVelTheta = 0;
  let orbitVelPhi = 0;
  let focusTarget: { x: number; y: number; z: number } | null = null;
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

  // Live-activity poll. `graph.ts` exposes no push event for individual tool
  // calls (only `graph-status`/`graph-analyses`), so per the task's fallback
  // instruction this polls `graphHistory()` on a short interval and diffs
  // against the newest `ts_ms` already processed.
  let historyTimer: ReturnType<typeof setInterval> | undefined;
  let lastHistoryTs = 0;

  let unsubSettings: (() => void) | undefined;

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
    if (kind === 'call') return EDGE_CALL;
    if (kind === 'import') return EDGE_IMPORT;
    if (kind === 'contains') return EDGE_CONTAINS;
    return EDGE_OTHER;
  }
  function dashFor(confidence: string): number[] {
    if (confidence === 'inferred') return [6, 4];
    if (confidence === 'ambiguous') return [1, 3];
    return [];
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

  // ── Force simulation ─────────────────────────────────────────────────────
  function initPosition(index: number, total: number, seedZ: boolean): { x: number; y: number; z: number } {
    const golden = Math.PI * (3 - Math.sqrt(5));
    const theta = index * golden;
    const r = Math.sqrt(index + 0.5) * 8 * Math.max(1, Math.sqrt(total) / 10);
    const x = Math.cos(theta) * r;
    const y = Math.sin(theta) * r;
    const z = seedZ ? (Math.random() - 0.5) * r : 0;
    return { x, y, z };
  }

  function applyRepulsion(a: SimNode, b: SimNode, weight: number): void {
    const dx = a.x - b.x;
    const dy = a.y - b.y;
    const dz = mode === '3d' ? a.z - b.z : 0;
    const distSq = dx * dx + dy * dy + dz * dz + 0.01;
    if (distSq > REPEL_CUTOFF_SQ) return;
    const dist = Math.sqrt(distSq);
    const f = (REPULSION_K * weight) / distSq;
    const fx = (dx / dist) * f;
    const fy = (dy / dist) * f;
    const fz = mode === '3d' ? (dz / dist) * f : 0;
    a.fx += fx; a.fy += fy; a.fz += fz;
    b.fx -= fx; b.fy -= fy; b.fz -= fz;
  }

  function applySpring(e: SimEdge): void {
    const a = e.src, b = e.dst;
    const dx = b.x - a.x;
    const dy = b.y - a.y;
    const dz = mode === '3d' ? b.z - a.z : 0;
    const dist = Math.sqrt(dx * dx + dy * dy + dz * dz) || 0.01;
    const f = SPRING_K * (dist - SPRING_LEN);
    const fx = (dx / dist) * f;
    const fy = (dy / dist) * f;
    const fz = mode === '3d' ? (dz / dist) * f : 0;
    a.fx += fx; a.fy += fy; a.fz += fz;
    b.fx -= fx; b.fy -= fy; b.fz -= fz;
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
          applyRepulsion(a, nodes[j], n / REPEL_SAMPLE);
        }
      }
    }
    for (const e of edges) applySpring(e);

    let ke = 0;
    const is3d = mode === '3d';
    for (const node of nodes) {
      node.fx += -node.x * GRAVITY_K;
      node.fy += -node.y * GRAVITY_K;
      node.vx = (node.vx + node.fx * dts) * DAMPING;
      node.vy = (node.vy + node.fy * dts) * DAMPING;
      node.x += node.vx * dts;
      node.y += node.vy * dts;
      ke += node.vx * node.vx + node.vy * node.vy;
      if (is3d) {
        node.fz += -node.z * GRAVITY_K;
        node.vz = (node.vz + node.fz * dts) * DAMPING;
        node.z += node.vz * dts;
        ke += node.vz * node.vz;
      }
    }
    return ke / n > IDLE_KE_THRESHOLD;
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
    if (mode === '2d') {
      for (const n of nodes) {
        n.sx = viewW / 2 + (n.x - viewCenterX) * zoom2D;
        n.sy = viewH / 2 + (n.y - viewCenterY) * zoom2D;
        n.sr = Math.max(2, n.r * zoom2D);
        n.sz = 0;
      }
    } else {
      const b = computeCameraBasis();
      for (const n of nodes) {
        const dx = n.x - b.ex, dy = n.y - b.ey, dz = n.z - b.ez;
        const rx = dx * b.xx + dy * b.xy + dz * b.xz;
        const ry = dx * b.yx + dy * b.yy + dz * b.yz;
        let fz = -(dx * b.zx + dy * b.zy + dz * b.zz);
        if (fz < NEAR_3D) fz = NEAR_3D;
        const scale = camFocal / fz;
        n.sx = viewW / 2 + rx * scale;
        n.sy = viewH / 2 - ry * scale;
        n.sr = clamp(n.r * scale, 1.5, 40);
        n.sz = fz;
      }
    }
  }

  // ── Rendering ─────────────────────────────────────────────────────────────
  function drawEdges(now: number): void {
    if (!ctx) return;
    for (const e of edges) {
      const highlighted = e.highlightUntil > now;
      ctx.beginPath();
      ctx.moveTo(e.src.sx, e.src.sy);
      ctx.lineTo(e.dst.sx, e.dst.sy);
      ctx.setLineDash(dashFor(e.confidence));
      ctx.strokeStyle = highlighted ? (e.highlightColor ?? edgeColor(e.kind)) : edgeColor(e.kind);
      ctx.globalAlpha = highlighted ? 0.95 : 0.4;
      ctx.lineWidth = highlighted ? 2.5 : 1;
      ctx.stroke();
    }
    ctx.setLineDash([]);
    ctx.globalAlpha = 1;
  }

  function drawNodes(now: number): void {
    if (!ctx) return;
    const order = mode === '3d' ? [...nodes].sort((a, b) => b.sz - a.sz) : nodes;
    for (const n of order) {
      const isFile = n.kind === 'file';
      ctx.fillStyle = n.subsystem ? subsystemColor(n.subsystem) : UNCATEGORIZED_COLOR;
      ctx.globalAlpha = 1;
      ctx.beginPath();
      if (isFile) {
        const s = n.sr * 1.6;
        ctx.rect(n.sx - s / 2, n.sy - s / 2, s, s);
      } else {
        ctx.arc(n.sx, n.sy, n.sr, 0, Math.PI * 2);
      }
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
      if (n.id === focusedNodeId && focusRingUntil > now) {
        const alpha = clamp((focusRingUntil - now) / FOCUS_MS, 0, 1);
        ctx.save();
        ctx.globalAlpha = alpha * 0.8;
        ctx.strokeStyle = ACCENT_COLOR;
        ctx.lineWidth = 2;
        ctx.beginPath();
        ctx.arc(n.sx, n.sy, n.sr + 6 + (1 - alpha) * 16, 0, Math.PI * 2);
        ctx.stroke();
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
    drawEdges(now);
    drawNodes(now);
  }

  // ── Animation loop ───────────────────────────────────────────────────────
  function decayActive(now: number): boolean {
    let active = false;
    for (const n of nodes) if (n.pulseUntil > now) { active = true; break; }
    if (!active) for (const e of edges) if (e.highlightUntil > now) { active = true; break; }
    if (!active && focusedNodeId && focusRingUntil > now) active = true;
    return active;
  }

  function stepViewAnimation(dt: number): boolean {
    if (!focusTarget) return false;
    const k = 1 - Math.exp(-dt * 0.008);
    if (mode === '2d') {
      viewCenterX += (focusTarget.x - viewCenterX) * k;
      viewCenterY += (focusTarget.y - viewCenterY) * k;
      if (Math.hypot(focusTarget.x - viewCenterX, focusTarget.y - viewCenterY) < 0.5) {
        viewCenterX = focusTarget.x; viewCenterY = focusTarget.y; focusTarget = null; return false;
      }
    } else {
      camTargetX += (focusTarget.x - camTargetX) * k;
      camTargetY += (focusTarget.y - camTargetY) * k;
      camTargetZ += (focusTarget.z - camTargetZ) * k;
      const d = Math.hypot(focusTarget.x - camTargetX, focusTarget.y - camTargetY, focusTarget.z - camTargetZ);
      if (d < 0.5) {
        camTargetX = focusTarget.x; camTargetY = focusTarget.y; camTargetZ = focusTarget.z; focusTarget = null; return false;
      }
    }
    return true;
  }

  function stepMomentum(dt: number): boolean {
    if (dragging) return false;
    let moving = false;
    if (mode === '2d') {
      if (Math.abs(panVelX) > 0.0005 || Math.abs(panVelY) > 0.0005) {
        viewCenterX += panVelX * dt;
        viewCenterY += panVelY * dt;
        panVelX *= 0.9; panVelY *= 0.9;
        moving = true;
      }
    } else if (Math.abs(orbitVelTheta) > 0.00002 || Math.abs(orbitVelPhi) > 0.00002) {
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
      const moving = simulate(dt);
      if (!moving) idle = true;
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
  function kick(): void {
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
        : initPosition(i, snap.nodes.length, mode === '3d');
      const deg = row.degree || 0;
      const r = clamp(4 + Math.sqrt(deg) * 2.2, 4, 22);
      const sn: SimNode = {
        ...row,
        x: pos.x, y: pos.y, z: pos.z,
        vx: old?.vx ?? 0, vy: old?.vy ?? 0, vz: old?.vz ?? 0,
        fx: 0, fy: 0, fz: 0,
        r, sx: 0, sy: 0, sr: 0, sz: 0,
        pulseStart: 0, pulseUntil: 0, pulseColor: null,
      };
      newNodes.push(sn);
      newById.set(row.id, sn);
    });
    const newEdges: SimEdge[] = [];
    for (const row of snap.edges) {
      const s = newById.get(row.src);
      const d = newById.get(row.dst);
      if (!s || !d) continue;
      newEdges.push({ src: s, dst: d, kind: row.kind, confidence: row.confidence, highlightUntil: 0, highlightColor: null });
    }

    nodes = newNodes;
    edges = newEdges;
    nodeById = newById;
    nodeCount = newNodes.length;
    edgeCount = newEdges.length;
    subsystemsPresent = [...new Set(newNodes.map((n) => n.subsystem).filter((s) => s))].sort();
    hasUncategorized = newNodes.some((n) => !n.subsystem);
    edgeKindsPresent = [...new Set(newEdges.map((e) => e.kind))].sort();
    edgeConfsPresent = [...new Set(newEdges.map((e) => e.confidence))].sort();
    focusedNodeId = null;
    hoveredNode = null;
    idle = false;
    resetView();
  }

  async function refresh(): Promise<void> {
    loading = true;
    fetchError = null;
    try {
      const snap = await graphVizSnapshot(root);
      buildSim(snap);
    } catch (e) {
      fetchError = String(e);
    } finally {
      loading = false;
    }
  }

  // ── View fitting ─────────────────────────────────────────────────────────
  function fitView2D(): void {
    if (nodes.length === 0 || viewW <= 0 || viewH <= 0) return;
    let minX = Infinity, maxX = -Infinity, minY = Infinity, maxY = -Infinity;
    for (const n of nodes) {
      if (n.x < minX) minX = n.x; if (n.x > maxX) maxX = n.x;
      if (n.y < minY) minY = n.y; if (n.y > maxY) maxY = n.y;
    }
    viewCenterX = (minX + maxX) / 2;
    viewCenterY = (minY + maxY) / 2;
    const spanX = Math.max(1, maxX - minX);
    const spanY = Math.max(1, maxY - minY);
    zoom2D = clamp(Math.min(viewW / spanX, viewH / spanY) * 0.85, MIN_ZOOM_2D, MAX_ZOOM_2D);
  }
  function fitView3D(): void {
    if (nodes.length === 0) return;
    let cx = 0, cy = 0, cz = 0;
    for (const n of nodes) { cx += n.x; cy += n.y; cz += n.z; }
    cx /= nodes.length; cy /= nodes.length; cz /= nodes.length;
    let maxR = 0;
    for (const n of nodes) maxR = Math.max(maxR, Math.hypot(n.x - cx, n.y - cy, n.z - cz));
    camTargetX = cx; camTargetY = cy; camTargetZ = cz;
    camTheta = 0.6; camPhi = 0.35;
    camDist = clamp(maxR * 2.2 + 80, MIN_DIST_3D, MAX_DIST_3D);
  }
  function resetView(): void {
    if (mode === '2d') fitView2D(); else fitView3D();
    focusTarget = null;
    panVelX = 0; panVelY = 0; orbitVelTheta = 0; orbitVelPhi = 0;
    kick();
  }

  function setMode(m: '2d' | '3d'): void {
    if (mode === m) return;
    mode = m;
    if (m === '3d' && !everEntered3D) {
      everEntered3D = true;
      for (const n of nodes) n.z = (Math.random() - 0.5) * 200;
    }
    resetView();
  }

  function focusNode(node: SimNode): void {
    focusedNodeId = node.id;
    focusRingUntil = performance.now() + FOCUS_MS;
    focusTarget = { x: node.x, y: node.y, z: node.z };
    kick();
  }

  // ── Live activity (poll graphHistory — no push event exists per-call) ────
  function matchNodes(target: string): SimNode[] {
    if (!target) return [];
    const exact = nodes.filter((n) => n.label === target || n.file === target);
    if (exact.length) return exact.slice(0, 3);
    const bare = target.split(':')[0];
    const byFile = nodes.filter((n) => n.kind === 'file' && (n.file === bare || bare.endsWith(n.file) || n.file.endsWith(bare)));
    if (byFile.length) return byFile.slice(0, 3);
    const bySymbol = nodes.filter((n) => n.kind !== 'file' && n.label.length >= 3 && target.includes(n.label));
    return bySymbol.slice(0, 3);
  }

  function applyActivity(call: GraphCall): void {
    const matched = matchNodes(call.target);
    if (matched.length === 0) return;
    const isCloud = call.source === 'claude' || call.source === 'opencode';
    const color = isCloud ? PULSE_CLOUD : PULSE_LOCAL;
    if (isCloud) sawCloudPulse = true; else sawLocalPulse = true;
    const now = performance.now();
    for (const n of matched) {
      n.pulseStart = now; n.pulseUntil = now + PULSE_MS; n.pulseColor = color;
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
    kick();
  }

  async function pollHistory(): Promise<void> {
    if (!graphEnabled) return;
    try {
      const calls = await graphHistory();
      if (calls.length === 0) return;
      const fresh = calls.filter((c) => c.ts_ms > lastHistoryTs).sort((a, b) => a.ts_ms - b.ts_ms);
      let newest = lastHistoryTs;
      for (const c of calls) if (c.ts_ms > newest) newest = c.ts_ms;
      lastHistoryTs = newest;
      for (const c of fresh) applyActivity(c);
    } catch {
      // Non-fatal — keep the last-known pulses/state and retry next tick.
    }
  }
  function startHistoryPoll(): void {
    if (historyTimer) return;
    void pollHistory();
    historyTimer = setInterval(() => void pollHistory(), 1500);
  }
  function stopHistoryPoll(): void {
    if (historyTimer) { clearInterval(historyTimer); historyTimer = undefined; }
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
    updateVisibility();
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
    let best: SimNode | null = null;
    let bestDist = Infinity;
    for (const n of nodes) {
      const d = Math.hypot(mx - n.sx, my - n.sy);
      const hitR = Math.max(n.sr, 7);
      if (d <= hitR && d < bestDist) { best = n; bestDist = d; }
    }
    return best;
  }

  function onCanvasMouseDown(e: MouseEvent): void {
    if (nodes.length === 0) return;
    dragStartX = e.clientX; dragStartY = e.clientY;
    dragLastX = e.clientX; dragLastY = e.clientY;
    dragLastT = performance.now();
    dragMoved = false;
    isPanDrag = mode === '3d' && (e.button === 2 || e.shiftKey);
    dragging = true;
    panVelX = 0; panVelY = 0; orbitVelTheta = 0; orbitVelPhi = 0;
    focusTarget = null;
    window.addEventListener('mousemove', onWindowMouseMove);
    window.addEventListener('mouseup', onWindowMouseUp);
    kick();
  }

  function onWindowMouseMove(e: MouseEvent): void {
    if (!dragging) return;
    const dx = e.clientX - dragLastX;
    const dy = e.clientY - dragLastY;
    const now = performance.now();
    const dt = Math.max(1, now - dragLastT);
    if (Math.abs(e.clientX - dragStartX) + Math.abs(e.clientY - dragStartY) > 4) dragMoved = true;

    if (mode === '2d') {
      viewCenterX -= dx / zoom2D;
      viewCenterY -= dy / zoom2D;
      panVelX = -dx / zoom2D / dt;
      panVelY = -dy / zoom2D / dt;
    } else if (isPanDrag) {
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
    kick();
  }

  function onWindowMouseUp(e: MouseEvent): void {
    if (!dragging) return;
    dragging = false;
    window.removeEventListener('mousemove', onWindowMouseMove);
    window.removeEventListener('mouseup', onWindowMouseUp);
    if (!dragMoved && canvasEl) {
      const rect = canvasEl.getBoundingClientRect();
      const hit = pickNode(e.clientX - rect.left, e.clientY - rect.top);
      if (hit) focusNode(hit); else focusedNodeId = null;
    }
    kick();
  }

  function onWheel(e: WheelEvent): void {
    if (nodes.length === 0) return;
    e.preventDefault();
    const rect = canvasEl.getBoundingClientRect();
    const mx = e.clientX - rect.left;
    const my = e.clientY - rect.top;
    if (mode === '2d') {
      const worldBeforeX = viewCenterX + (mx - viewW / 2) / zoom2D;
      const worldBeforeY = viewCenterY + (my - viewH / 2) / zoom2D;
      zoom2D = clamp(zoom2D * Math.exp(-e.deltaY * 0.0015), MIN_ZOOM_2D, MAX_ZOOM_2D);
      viewCenterX = worldBeforeX - (mx - viewW / 2) / zoom2D;
      viewCenterY = worldBeforeY - (my - viewH / 2) / zoom2D;
    } else {
      camDist = clamp(camDist * Math.exp(e.deltaY * 0.0015), MIN_DIST_3D, MAX_DIST_3D);
    }
    kick();
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
  onMount(() => {
    ctx = canvasEl.getContext('2d');
    ro = new ResizeObserver(handleResize);
    ro.observe(containerEl);
    io = new IntersectionObserver(handleIntersect, { threshold: 0 });
    io.observe(containerEl);
    document.addEventListener('visibilitychange', handleVisChange);
    // Fires synchronously with the current value, so this both seeds
    // `graphEnabled` and kicks off the first load/poll without a separate
    // one-off read.
    unsubSettings = settings.subscribe((s) => {
      const en = s.graph.enabled;
      if (en === graphEnabled) return;
      graphEnabled = en;
      if (en) { void refresh(); startHistoryPoll(); }
      else { stopHistoryPoll(); stopLoop(); }
    });
  });

  onDestroy(() => {
    stopLoop();
    stopHistoryPoll();
    ro?.disconnect();
    io?.disconnect();
    document.removeEventListener('visibilitychange', handleVisChange);
    window.removeEventListener('mousemove', onWindowMouseMove);
    window.removeEventListener('mouseup', onWindowMouseUp);
    if (resizeDebounce) clearTimeout(resizeDebounce);
    unsubSettings?.();
  });
</script>

<div class="graph-view" bind:this={containerEl}>
  <header class="controls">
    <div class="toggle-group" role="group" aria-label="View mode">
      <button type="button" class="seg" class:active={mode === '2d'} onclick={() => setMode('2d')}>2D</button>
      <button type="button" class="seg" class:active={mode === '3d'} onclick={() => setMode('3d')}>3D</button>
    </div>
    <button type="button" class="secondary" onclick={resetView} disabled={nodes.length === 0}>Reset view</button>
    <button type="button" class="secondary" onclick={() => void refresh()} disabled={loading || !graphEnabled}>
      {loading ? 'Loading…' : 'Refresh'}
    </button>
    <span class="counts tnum">{nodeCount} nodes · {edgeCount} edges</span>
  </header>

  <div class="canvas-wrap">
    {#if !graphEnabled}
      <p class="banner">
        Graph View is disabled. Turn on the code graph (Settings → Code Intelligence) to use it.
      </p>
    {:else if fetchError}
      <p class="banner err">
        Couldn't load the graph: {fetchError}
        <button type="button" class="secondary" onclick={() => void refresh()}>Retry</button>
      </p>
    {:else if loading}
      <p class="banner">Loading graph…</p>
    {:else if nodeCount === 0}
      <p class="banner">No indexed graph yet. Build the code graph from the Code Intelligence tab first.</p>
    {:else}
      <canvas
        bind:this={canvasEl}
        class="graph-canvas"
        onmousedown={onCanvasMouseDown}
        onmousemove={onCanvasMouseMove}
        onmouseleave={onCanvasMouseLeave}
        onwheel={onWheel}
        oncontextmenu={(e) => e.preventDefault()}
      ></canvas>

      {#if hoveredNode}
        <div class="tooltip" style="left:{hoverPos.x + 14}px; top:{hoverPos.y + 14}px;">
          <strong>{hoveredNode.label}</strong>
          <div>{hoveredNode.file}</div>
          <div>{hoveredNode.kind} · degree {hoveredNode.degree}</div>
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
              <h4>Node size / shape</h4>
              <div class="legend-row">size = call/reference degree</div>
              <div class="legend-row"><span class="swatch shape-circle"></span>symbol</div>
              <div class="legend-row"><span class="swatch shape-square"></span>file</div>
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
            {#if sawCloudPulse || sawLocalPulse}
              <div class="legend-section">
                <h4>Live activity pulse</h4>
                {#if sawCloudPulse}
                  <div class="legend-row"><span class="swatch" style="background:{PULSE_CLOUD}"></span>cloud agent (Claude / OpenCode)</div>
                {/if}
                {#if sawLocalPulse}
                  <div class="legend-row"><span class="swatch" style="background:{PULSE_LOCAL}"></span>local offload worker</div>
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
       same convention as WorkbenchView/CodeIntelligenceView/OffloadServerView. */
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
  .toggle-group {
    display: flex;
    gap: 2px;
    border: 1px solid var(--border, #444);
    border-radius: 6px;
    overflow: hidden;
  }
  .seg {
    padding: 4px 10px;
    border: none;
    background: transparent;
    color: var(--text, #ddd);
    font-size: 12px;
    cursor: pointer;
    opacity: 0.75;
  }
  .seg.active {
    background: var(--accent, #3b6ea5);
    color: #fff;
    opacity: 1;
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
    margin: 16px;
    padding: 8px 10px;
    border-radius: 6px;
    font-size: 12px;
    border: 1px solid var(--border, #444);
    background: rgba(255, 255, 255, 0.04);
    opacity: 0.9;
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
  .swatch.shape-circle {
    border-radius: 50%;
    background: var(--text, #ddd);
  }
  .swatch.shape-square {
    border-radius: 2px;
    background: var(--text, #ddd);
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
