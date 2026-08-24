// The force simulation's first tests. It ran in `GraphView.svelte` from V15
// until #130 and was never covered by anything: a layout bug could only be
// found by looking at the canvas, and every one recorded in the comments
// (`applySpring`'s degree normalisation, the halved sampled-repulsion weight,
// the alpha floor) was found exactly that way.
//
// What is worth pinning here is the CONVERGENCE CONTRACT, because that is what
// the component's frame loop trusts: `simulate` returns false when the layout
// has settled, and it must do so — a sim that never says "done" pins the
// webview thread until `SIM_MAX_MS` every single time.

import { describe, expect, test } from 'vitest';

import {
  ALPHA_MIN,
  applyRepulsion,
  applySpring,
  clamp,
  defaultTuning,
  dirOf,
  emptyWorld,
  initPosition,
  leashFor,
  MAX_REPEL_FULL,
  nodeRadius,
  simulate,
  type DirCluster,
  type SimEdge,
  type SimNode,
  type SimWorld,
} from './sim';

/// A tiny deterministic PRNG (mulberry32). The sim takes `rand` only so a test
/// can do this — the app always passes `Math.random`.
function seeded(seed: number): () => number {
  let a = seed >>> 0;
  return () => {
    a = (a + 0x6d2b79f5) >>> 0;
    let t = Math.imul(a ^ (a >>> 15), 1 | a);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

function node(id: string, file: string, degree: number, x: number, y: number, z: number): SimNode {
  return {
    id,
    file,
    degree,
    subsystem: '',
    x, y, z,
    vx: 0, vy: 0, vz: 0,
    fx: 0, fy: 0, fz: 0,
    r: nodeRadius(degree, 1),
    sx: 0, sy: 0, sr: 0, sz: 0,
    pulseUntil: 0,
    pulseColor: null,
    dir: dirOf(file),
  } as SimNode;
}

function edge(src: SimNode, dst: SimNode): SimEdge {
  return {
    src,
    dst,
    kind: 'call',
    confidence: 'extracted',
    highlightUntil: 0,
    highlightColor: null,
    intra: src.dir === dst.dir,
    drawn: true,
  };
}

/// The same shape `GraphView.buildSim` produces: nodes seeded on the Fibonacci
/// spiral, one directory cluster per `dirOf`, intra-directory edges.
function world(files: string[], links: [number, number][], seed = 7): SimWorld {
  const rand = seeded(seed);
  const w = emptyWorld();
  w.nodes = files.map((f, i) => {
    const p = initPosition(i, files.length, rand);
    return node(`file:${f}`, f, 2, p.x, p.y, p.z);
  });
  w.edges = links.map(([a, b]) => edge(w.nodes[a], w.nodes[b]));
  const byDir = new Map<string, SimNode[]>();
  for (const n of w.nodes) {
    const m = byDir.get(n.dir);
    if (m) m.push(n);
    else byDir.set(n.dir, [n]);
  }
  for (const [dir, members] of byDir) {
    let x = 0, y = 0, z = 0;
    for (const m of members) { x += m.x; y += m.y; z += m.z; }
    const c: DirCluster = {
      name: dir,
      members,
      leash: leashFor(members.length, 1),
      x: x / members.length, y: y / members.length, z: z / members.length,
      vx: 0, vy: 0, vz: 0, fx: 0, fy: 0, fz: 0,
      sx: 0, sy: 0, sz: 0, discR: 0,
    };
    w.clusters.push(c);
  }
  w.alpha = 1;
  return w;
}

/// Step until `simulate` reports settled, or give up. Returns the frame count;
/// `null` means it never settled, which is the failure the app cares about.
function settle(w: SimWorld, maxFrames = 2000, seed = 11): number | null {
  const tune = defaultTuning();
  const rand = seeded(seed);
  for (let i = 1; i <= maxFrames; i++) {
    if (!simulate(w, tune, 16.67, rand)) return i;
  }
  return null;
}

const GRID = ['a/one.ts', 'a/two.ts', 'a/three.ts', 'b/four.ts', 'b/five.ts', 'c/six.ts'];
const LINKS: [number, number][] = [[0, 1], [1, 2], [3, 4]];

describe('simulate — the convergence contract', () => {
  test('a layout settles, and says so', () => {
    const w = world(GRID, LINKS);
    const frames = settle(w);
    expect(frames, 'the sim never reported settled — the frame loop would run to SIM_MAX_MS')
      .not.toBeNull();
    // Two independent gates settle a layout: the cooling floor (alpha decays
    // 2%/frame from 1, so it is under ALPHA_MIN inside ~200 frames whatever
    // else happens) and the kinetic-energy floor. A six-file graph reaches
    // equilibrium long before it cools, so here it is the ENERGY gate that
    // fires — see the sampled-repulsion case below for the other one.
    expect(frames!).toBeLessThan(400);
    expect(w.alpha, 'this size settles on energy, not on cooling').toBeGreaterThan(ALPHA_MIN);
  });

  test('a graph big enough for sampled repulsion also settles', () => {
    // Above MAX_REPEL_FULL the sim stops computing every pair and samples
    // partners at random instead. That noise is exactly what the cooling
    // schedule was added for, so this is the path most at risk of running
    // forever — the frame budget is why it must not.
    const files = Array.from({ length: MAX_REPEL_FULL + 100 }, (_, i) => `d${i % 20}/f${i}.ts`);
    const w = world(files, [], 21);
    const frames = settle(w, 4000, 22);
    expect(frames, 'a sampled-repulsion layout never settled').not.toBeNull();
    expect(frames!).toBeLessThan(400);
  });

  test('the cooling floor stops a layout that is still moving fast', () => {
    // The gate `ALPHA_DECAY`'s comment exists for: sampled repulsion can keep
    // kinetic energy above IDLE_KE_THRESHOLD indefinitely, so energy alone is
    // not allowed to be the only way out. Cooled below ALPHA_MIN, a world with
    // every node flying still reports settled.
    const w = world(GRID, LINKS);
    for (const n of w.nodes) { n.vx = 30; n.vy = 30; n.vz = 30; }
    w.alpha = ALPHA_MIN;
    expect(simulate(w, defaultTuning(), 16.67, seeded(1))).toBe(false);
  });

  test('the same seed settles to the same layout', () => {
    // Determinism is what makes a layout regression reviewable at all: the
    // only nondeterminism in the whole engine is the `rand` these two share.
    const a = world(GRID, LINKS, 3);
    const b = world(GRID, LINKS, 3);
    settle(a, 2000, 5);
    settle(b, 2000, 5);
    expect(a.nodes.map((n) => [n.x, n.y, n.z])).toEqual(b.nodes.map((n) => [n.x, n.y, n.z]));
  });

  test('an empty world is settled, not busy', () => {
    expect(simulate(emptyWorld(), defaultTuning(), 16.67)).toBe(false);
  });

  test('a settled layout is finite everywhere', () => {
    // A NaN anywhere (a zero-distance divide, an unclamped force) propagates
    // through every subsequent frame and paints nothing at all.
    const w = world(GRID, LINKS);
    settle(w);
    for (const n of w.nodes) {
      for (const v of [n.x, n.y, n.z, n.vx, n.vy, n.vz]) expect(Number.isFinite(v)).toBe(true);
    }
    for (const c of w.clusters) {
      for (const v of [c.x, c.y, c.z]) expect(Number.isFinite(v)).toBe(true);
    }
  });

  test('files in one directory end up nearer each other than to another directory', () => {
    // The whole point of the directory-cluster layout. Not a tight bound —
    // just the property a reader looks at the picture to check.
    const w = world(GRID, LINKS);
    settle(w);
    const dist = (a: SimNode, b: SimNode) => Math.hypot(a.x - b.x, a.y - b.y, a.z - b.z);
    const [a1, a2, , b1] = w.nodes;
    expect(dist(a1, a2)).toBeLessThan(dist(a1, b1));
  });

  test('it mutates the world in place — the caller keeps one object per view', () => {
    // The invariant the whole extraction rests on: the component holds ONE
    // `SimWorld` and hands it over by reference every frame. If `simulate`
    // ever returned or swapped in fresh arrays, the component's `scene` (and
    // its `nodeById` index) would be pointing at last frame's data.
    const w = world(GRID, LINKS);
    const nodesRef = w.nodes;
    const node0 = w.nodes[0];
    const clustersRef = w.clusters;
    const before = { x: node0.x, y: node0.y, z: node0.z };
    simulate(w, defaultTuning(), 16.67, seeded(1));
    expect(w.nodes).toBe(nodesRef);
    expect(w.nodes[0]).toBe(node0);
    expect(w.clusters).toBe(clustersRef);
    expect([node0.x, node0.y, node0.z]).not.toEqual([before.x, before.y, before.z]);
  });

  test('alpha decays every frame, so a re-heat is the only way back up', () => {
    const w = world(GRID, LINKS);
    const first = w.alpha;
    simulate(w, defaultTuning(), 16.67, seeded(1));
    const second = w.alpha;
    simulate(w, defaultTuning(), 16.67, seeded(1));
    expect(second).toBeLessThan(first);
    expect(w.alpha).toBeLessThan(second);
  });
});

describe('the force primitives', () => {
  test('repulsion pushes both endpoints apart, symmetrically', () => {
    const a = node('a', 'x/a.ts', 1, -10, 0, 0);
    const b = node('b', 'x/b.ts', 1, 10, 0, 0);
    applyRepulsion(a, b, 1, 1);
    expect(a.fx).toBeLessThan(0);
    expect(b.fx).toBeGreaterThan(0);
    expect(a.fx).toBeCloseTo(-b.fx, 10);
  });

  test('repulsion beyond the cutoff costs nothing', () => {
    const a = node('a', 'x/a.ts', 1, 0, 0, 0);
    const b = node('b', 'x/b.ts', 1, 5000, 0, 0);
    applyRepulsion(a, b, 1, 1);
    expect([a.fx, a.fy, a.fz, b.fx, b.fy, b.fz]).toEqual([0, 0, 0, 0, 0, 0]);
  });

  test('a stretched spring pulls its endpoints together', () => {
    const a = node('a', 'x/a.ts', 1, -500, 0, 0);
    const b = node('b', 'x/b.ts', 1, 500, 0, 0);
    applySpring(edge(a, b), 1);
    expect(a.fx).toBeGreaterThan(0); // a moves right, toward b
    expect(b.fx).toBeLessThan(0);
  });

  test('a hub takes less of the displacement than its leaf (degree bias)', () => {
    // The fix recorded in `applySpring`'s comment: without the bias a hub
    // accumulated the stiffness of every one of its springs, overshot, and
    // froze stranded outside the cluster.
    const hub = node('hub', 'x/hub.ts', 100, -500, 0, 0);
    const leaf = node('leaf', 'x/leaf.ts', 1, 500, 0, 0);
    applySpring(edge(hub, leaf), 1);
    expect(Math.abs(hub.fx)).toBeLessThan(Math.abs(leaf.fx));
  });
});

describe('the small pure helpers', () => {
  test('clamp holds the bounds', () => {
    expect(clamp(5, 0, 10)).toBe(5);
    expect(clamp(-5, 0, 10)).toBe(0);
    expect(clamp(50, 0, 10)).toBe(10);
  });

  test('dirOf splits a path, and a bare filename is the root', () => {
    expect(dirOf('src/lib/graph/sim.ts')).toBe('src/lib/graph');
    expect(dirOf('README.md')).toBe('(root)');
  });

  test('node radius is bounded and rises with degree, then scales', () => {
    expect(nodeRadius(0, 1)).toBeCloseTo(2, 10);
    expect(nodeRadius(10_000, 1)).toBeCloseTo(7, 10);
    expect(nodeRadius(8, 1)).toBeGreaterThan(nodeRadius(2, 1));
    expect(nodeRadius(8, 3)).toBeCloseTo(nodeRadius(8, 1) * 3, 10);
  });

  test('the directory leash grows with member count and scales', () => {
    expect(leashFor(1, 1)).toBeLessThan(leashFor(100, 1));
    expect(leashFor(9, 2)).toBeCloseTo(leashFor(9, 1) * 2, 10);
  });

  test('initPosition is a spiral, and only its z uses the RNG', () => {
    const a = initPosition(3, 50, () => 0.5);
    const b = initPosition(3, 50, () => 0.9);
    expect(a.x).toBe(b.x);
    expect(a.y).toBe(b.y);
    expect(a.z).not.toBe(b.z);
    expect(initPosition(0, 1, () => 0.5)).toEqual({ x: expect.any(Number), y: 0, z: 0 });
  });
});
