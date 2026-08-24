// The renderer's first tests (#130), against a RECORDING 2D context: a stub
// that logs every call and property write instead of drawing. That is enough
// for the questions worth asking about this file, all of which are about WHAT
// it decides to draw rather than how it looks:
//
//   - which nodes and edges a selection or a directory focus hides;
//   - that culled geometry is never stroked;
//   - that the ambient field is batched by (kind, confidence) rather than
//     stroked per edge — the fix that stopped a large snapshot pinning the
//     webview thread, and the kind of thing a later "simplification" undoes.
//
// A pixel comparison would need a real canvas and would fail on every
// deliberate colour change; this fails only when a rule changes.

import { describe, expect, test } from 'vitest';

import {
  currentEgoIds,
  dashFor,
  drawDirLayer,
  drawEdges,
  drawNodes,
  drawScene,
  EDGE_CONTAINS,
  EDGE_OTHER,
  edgeColor,
  hashStr,
  subsystemColor,
  SUBSYSTEM_PALETTE,
  visibleDirNames,
  visibleNodeIds,
  type Scene,
} from './render';
import {
  defaultTuning,
  dirOf,
  emptyWorld,
  type DirCluster,
  type DirEdge,
  type SimEdge,
  type SimNode,
} from './sim';

// ── the recording context ────────────────────────────────────────────────
type Op = { op: string; args: unknown[] };

/// Records the calls and the style writes, in order. Only the members
/// `render.ts` actually touches are implemented — anything it grows will fail
/// loudly here rather than silently no-op.
function recorder(): { ctx: CanvasRenderingContext2D; ops: Op[] } {
  const ops: Op[] = [];
  const call = (op: string) => (...args: unknown[]) => { ops.push({ op, args }); };
  const target: Record<string, unknown> = {
    beginPath: call('beginPath'),
    moveTo: call('moveTo'),
    lineTo: call('lineTo'),
    arc: call('arc'),
    fill: call('fill'),
    stroke: call('stroke'),
    fillText: call('fillText'),
    setLineDash: call('setLineDash'),
    save: call('save'),
    restore: call('restore'),
    clearRect: call('clearRect'),
  };
  const ctx = new Proxy(target, {
    get: (t, k) => t[k as string],
    set: (t, k, v) => {
      ops.push({ op: `set:${String(k)}`, args: [v] });
      t[k as string] = v;
      return true;
    },
  }) as unknown as CanvasRenderingContext2D;
  return { ctx, ops };
}

// ── fixtures ─────────────────────────────────────────────────────────────
function node(id: string, file: string, over: Partial<SimNode> = {}): SimNode {
  return {
    id, file, degree: 2, subsystem: '',
    x: 0, y: 0, z: 0,
    vx: 0, vy: 0, vz: 0,
    fx: 0, fy: 0, fz: 0,
    r: 3,
    sx: 100, sy: 100, sr: 3, sz: 500,
    pulseUntil: 0, pulseColor: null,
    dir: dirOf(file),
    ...over,
  } as SimNode;
}

function edge(src: SimNode, dst: SimNode, over: Partial<SimEdge> = {}): SimEdge {
  return {
    src, dst,
    kind: 'call', confidence: 'extracted',
    highlightUntil: 0, highlightColor: null,
    intra: src.dir === dst.dir,
    drawn: true,
    ...over,
  };
}

function cluster(name: string, members: SimNode[]): DirCluster {
  return {
    name, members, leash: 20,
    x: 0, y: 0, z: 0,
    vx: 0, vy: 0, vz: 0, fx: 0, fy: 0, fz: 0,
    sx: 50, sy: 50, sz: 400, discR: 0,
  };
}

/// Two directories: `a` holds a1/a2 with an internal edge, `b` holds b1, and
/// one cross-directory edge joins a1 to b1.
function fixture(): { scene: Scene; a1: SimNode; a2: SimNode; b1: SimNode } {
  const a1 = node('file:a/1.ts', 'a/1.ts', { sx: 10, sy: 10 });
  const a2 = node('file:a/2.ts', 'a/2.ts', { sx: 20, sy: 20 });
  const b1 = node('file:b/1.ts', 'b/1.ts', { sx: 200, sy: 200 });
  const intra = edge(a1, a2);
  const cross = edge(a1, b1);
  const w = emptyWorld();
  w.nodes = [a1, a2, b1];
  w.edges = [intra, cross];
  const ca = cluster('a', [a1, a2]);
  const cb = cluster('b', [b1]);
  cb.sx = 220; cb.sy = 220;
  w.clusters = [ca, cb];
  const de: DirEdge = { a: ca, b: cb, weight: 1, callW: 1, importW: 0 };
  w.dirEdges = [de];
  const scene: Scene = {
    world: w,
    nodeById: new Map(w.nodes.map((n) => [n.id, n])),
    edgesByNode: new Map([
      [a1.id, [intra, cross]],
      [a2.id, [intra]],
      [b1.id, [cross]],
    ]),
    clusterByDir: new Map([['a', ca], ['b', cb]]),
    selectedNodeId: null,
    selectedDir: null,
    hoveredNode: null,
    focusRingUntil: 0,
    edgeCallColor: '#4fb3ff',
    edgeImportColor: '#ff8a3d',
    tune: defaultTuning(),
  };
  return { scene, a1, a2, b1 };
}

const arcs = (ops: Op[]) => ops.filter((o) => o.op === 'arc');

describe('the visibility rules (shared with picking)', () => {
  test('with nothing selected, everything is visible', () => {
    const { scene } = fixture();
    expect(currentEgoIds(scene)).toBeNull();
    expect(visibleNodeIds(scene)).toBeNull();
    expect(visibleDirNames(scene)).toBeNull();
  });

  test('a node selection narrows to its 1-hop ego set, and to the dirs it spans', () => {
    const { scene, a1, a2, b1 } = fixture();
    scene.selectedNodeId = a1.id;
    expect([...visibleNodeIds(scene)!].sort()).toEqual([a1.id, a2.id, b1.id].sort());
    expect([...visibleDirNames(scene)!].sort()).toEqual(['a', 'b']);
  });

  test('an ego set follows over-quota edges too', () => {
    // Undrawn edges are list/highlight-only as AMBIENT lines, but they are
    // still connections: an ego view that dropped them would hide a real
    // neighbour from the connections panel and from picking.
    const { scene, a1, b1 } = fixture();
    scene.world.edges[1].drawn = false;
    scene.selectedNodeId = a1.id;
    expect(visibleNodeIds(scene)!.has(b1.id)).toBe(true);
  });

  test('a selection naming a node that is gone falls back to "show everything"', () => {
    // A refresh can drop the selected node. Resolving to an EMPTY ego set
    // instead of null would render a blank canvas with no way back.
    const { scene } = fixture();
    scene.selectedNodeId = 'file:vanished.ts';
    expect(currentEgoIds(scene)).toBeNull();
    expect(visibleNodeIds(scene)).toBeNull();
  });

  test('a directory focus narrows to that directory only', () => {
    const { scene, a1, a2 } = fixture();
    scene.selectedDir = 'a';
    expect([...visibleNodeIds(scene)!].sort()).toEqual([a1.id, a2.id].sort());
    expect([...visibleDirNames(scene)!]).toEqual(['a']);
  });

  test('a node selection wins over a directory focus', () => {
    const { scene, a2 } = fixture();
    scene.selectedNodeId = a2.id;
    scene.selectedDir = 'b';
    expect([...visibleNodeIds(scene)!].sort()).toEqual([a2.id, 'file:a/1.ts'].sort());
  });
});

describe('drawNodes', () => {
  test('every visible node gets a disc', () => {
    const { scene } = fixture();
    const { ctx, ops } = recorder();
    drawNodes(ctx, scene, 0);
    expect(arcs(ops)).toHaveLength(3);
  });

  test('a culled node (sz < 0) is not drawn at all', () => {
    const { scene, b1 } = fixture();
    b1.sz = -1;
    const { ctx, ops } = recorder();
    drawNodes(ctx, scene, 0);
    expect(arcs(ops)).toHaveLength(2);
  });

  test('with a selection, only the ego set draws', () => {
    const { scene, a2 } = fixture();
    scene.selectedNodeId = a2.id;
    const { ctx, ops } = recorder();
    drawNodes(ctx, scene, 0);
    // a2 and its one neighbour a1 — plus a2's own selection ring.
    expect(arcs(ops)).toHaveLength(3);
  });

  test('a live pulse adds a ring, and an expired one does not', () => {
    const { scene, a1 } = fixture();
    a1.pulseUntil = 1000;
    const live = recorder();
    drawNodes(live.ctx, scene, 500);
    const expired = recorder();
    drawNodes(expired.ctx, scene, 2000);
    expect(arcs(live.ops).length).toBe(arcs(expired.ops).length + 1);
  });

  test('the focus ring is drawn only while it lasts', () => {
    const { scene, a1 } = fixture();
    scene.selectedNodeId = a1.id;
    scene.focusRingUntil = 1000;
    const during = recorder();
    drawNodes(during.ctx, scene, 500);
    const after = recorder();
    drawNodes(after.ctx, scene, 2000);
    expect(arcs(during.ops).length).toBe(arcs(after.ops).length + 1);
  });

  test('nodes paint back to front', () => {
    // Sorted by descending camera depth, so the nearest disc lands on top.
    const { scene, a1, a2, b1 } = fixture();
    a1.sz = 100; a2.sz = 900; b1.sz = 500;
    const { ctx, ops } = recorder();
    drawNodes(ctx, scene, 0);
    expect(arcs(ops).map((o) => o.args[0])).toEqual([a2.sx, b1.sx, a1.sx]);
  });
});

describe('drawEdges', () => {
  const strokes = (ops: Op[]) => ops.filter((o) => o.op === 'stroke');

  test('the ambient field draws intra-directory edges, batched by style', () => {
    // One beginPath/stroke per (kind, confidence) group, NOT per edge. The
    // cross-directory edge is represented by the directory layer's aggregate
    // line instead, so it is not here.
    const { scene } = fixture();
    const { ctx, ops } = recorder();
    drawEdges(ctx, scene, 0);
    expect(strokes(ops)).toHaveLength(1);
    expect(ops.filter((o) => o.op === 'moveTo')).toHaveLength(1);
  });

  test('two edges of one style still cost one stroke; two styles cost two', () => {
    const { scene, a1, a2 } = fixture();
    const a3 = node('file:a/3.ts', 'a/3.ts');
    scene.world.nodes.push(a3);
    scene.world.edges.push(edge(a2, a3));
    const same = recorder();
    drawEdges(same.ctx, scene, 0);
    expect(strokes(same.ops)).toHaveLength(1);
    scene.world.edges.push(edge(a1, a3, { kind: 'import' }));
    const mixed = recorder();
    drawEdges(mixed.ctx, scene, 0);
    expect(strokes(mixed.ops)).toHaveLength(2);
  });

  test('over-quota edges are never ambient lines', () => {
    const { scene } = fixture();
    scene.world.edges[0].drawn = false;
    const { ctx, ops } = recorder();
    drawEdges(ctx, scene, 0);
    expect(strokes(ops)).toHaveLength(0);
  });

  test('an edge with a culled endpoint is skipped', () => {
    const { scene, a2 } = fixture();
    a2.sz = -1;
    const { ctx, ops } = recorder();
    drawEdges(ctx, scene, 0);
    expect(strokes(ops)).toHaveLength(0);
  });

  test('with a node selected the ambient field is gone and its own edges emphasise', () => {
    const { scene, a1 } = fixture();
    scene.selectedNodeId = a1.id;
    const { ctx, ops } = recorder();
    drawEdges(ctx, scene, 0);
    // Both of a1's edges, each stroked on its own (the intra one, and the
    // cross one routed through the two directory anchors).
    expect(strokes(ops)).toHaveLength(2);
    // The routed one adds the two anchor waypoints.
    expect(ops.filter((o) => o.op === 'lineTo')).toHaveLength(4);
  });

  test('a highlighted edge draws on top, in its own colour', () => {
    const { scene } = fixture();
    scene.world.edges[0].highlightUntil = 1000;
    scene.world.edges[0].highlightColor = '#ff0000';
    const { ctx, ops } = recorder();
    drawEdges(ctx, scene, 500);
    const styles = ops.filter((o) => o.op === 'set:strokeStyle').map((o) => o.args[0]);
    expect(styles).toContain('#ff0000');
  });

  test('under a directory focus, only that directory\'s internal edges draw', () => {
    const { scene } = fixture();
    scene.selectedDir = 'b'; // b holds one node and no internal edge
    const { ctx, ops } = recorder();
    drawEdges(ctx, scene, 0);
    expect(strokes(ops)).toHaveLength(0);
    scene.selectedDir = 'a';
    const inA = recorder();
    drawEdges(inA.ctx, scene, 0);
    expect(strokes(inA.ops)).toHaveLength(1);
  });

  test('the dash pattern is reset when the pass ends', () => {
    // A leaked dash would apply to whatever the NEXT frame's first stroke is.
    const { scene } = fixture();
    const { ctx, ops } = recorder();
    drawEdges(ctx, scene, 0);
    const dashes = ops.filter((o) => o.op === 'setLineDash');
    expect(dashes.at(-1)!.args[0]).toEqual([]);
  });
});

describe('drawDirLayer', () => {
  test('each cluster gets a disc sized to its farthest visible member', () => {
    const { scene } = fixture();
    const { ctx, ops } = recorder();
    drawDirLayer(ctx, scene);
    const [ca] = scene.world.clusters;
    // a1 at (10,10) is the farther of the two from the anchor at (50,50):
    // the disc reaches its centre plus its own screen radius, plus 6 of pad.
    expect(ca.discR).toBeCloseTo(Math.hypot(-40, -40) + 3 + 6, 6);
    expect(arcs(ops)).toHaveLength(2);
  });

  test('the aggregate directory edge draws only without a selection', () => {
    const { scene, a1 } = fixture();
    const ambient = recorder();
    drawDirLayer(ambient.ctx, scene);
    const ambientLines = ambient.ops.filter((o) => o.op === 'lineTo').length;
    expect(ambientLines).toBe(1);
    scene.selectedNodeId = a1.id;
    const selected = recorder();
    drawDirLayer(selected.ctx, scene);
    expect(selected.ops.filter((o) => o.op === 'lineTo')).toHaveLength(0);
  });

  test('a culled cluster loses its disc radius, so picking cannot hit it', () => {
    const { scene } = fixture();
    scene.world.clusters[1].discR = 99;
    scene.world.clusters[1].sz = -1;
    const { ctx } = recorder();
    drawDirLayer(ctx, scene);
    expect(scene.world.clusters[1].discR).toBe(0);
  });
});

describe('drawScene', () => {
  test('the layers stack back to front: discs, then edges, then nodes', () => {
    const { scene } = fixture();
    const { ctx, ops } = recorder();
    drawScene(ctx, scene, 0);
    // The directory labels are the last thing the disc layer draws, and the
    // node discs are `arc` calls with a radius of 3 (the node radius).
    const label = ops.findIndex((o) => o.op === 'fillText');
    const firstNode = ops.findIndex((o) => o.op === 'arc' && o.args[2] === 3);
    expect(label).toBeGreaterThan(-1);
    expect(firstNode).toBeGreaterThan(label);
  });
});

describe('the palette helpers', () => {
  test('a subsystem always gets the same colour, from the shipped palette', () => {
    expect(subsystemColor('graph')).toBe(subsystemColor('graph'));
    expect(SUBSYSTEM_PALETTE).toContain(subsystemColor('graph'));
    expect(SUBSYSTEM_PALETTE).toContain(subsystemColor('a totally different name'));
  });

  test('the hash is non-negative, so the palette index never goes off the end', () => {
    for (const s of ['', 'a', 'zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz', '💥']) {
      expect(hashStr(s)).toBeGreaterThanOrEqual(0);
    }
  });

  test('edge colour follows the live knobs for call/import and is fixed otherwise', () => {
    expect(edgeColor('call', '#111111', '#222222')).toBe('#111111');
    expect(edgeColor('import', '#111111', '#222222')).toBe('#222222');
    expect(edgeColor('contains', '#111111', '#222222')).toBe(EDGE_CONTAINS);
    expect(edgeColor('whatever', '#111111', '#222222')).toBe(EDGE_OTHER);
  });

  test('confidence carries a non-hue channel — each level dashes differently', () => {
    // Deliberate redundancy with colour, for colourblind readers.
    expect(dashFor('extracted')).toEqual([]);
    expect(dashFor('inferred')).not.toEqual(dashFor('ambiguous'));
    expect(dashFor('inferred')).not.toEqual([]);
  });
});
