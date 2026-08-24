// The orbit camera's first tests (#130). The projection is the one piece of
// this view where a sign error is invisible in review and obvious on screen —
// and until now the only way to see it was to open the tab.

import { describe, expect, test } from 'vitest';

import {
  computeCameraBasis,
  cross3,
  defaultCamera,
  MAX_DIST_3D,
  maxDist3d,
  NEAR_3D,
  normalize3,
  project,
  type Camera,
} from './camera';
import { dirOf, emptyWorld, nodeRadius, type DirCluster, type SimNode, type SimWorld } from './sim';

function node(id: string, x: number, y: number, z: number): SimNode {
  return {
    id, file: `x/${id}.ts`, degree: 1, subsystem: '',
    x, y, z,
    vx: 0, vy: 0, vz: 0,
    fx: 0, fy: 0, fz: 0,
    r: nodeRadius(1, 1),
    sx: 0, sy: 0, sr: 0, sz: 0,
    pulseUntil: 0, pulseColor: null,
    dir: dirOf(`x/${id}.ts`),
  } as SimNode;
}

function cluster(name: string, x: number, y: number, z: number): DirCluster {
  return {
    name, members: [], leash: 20,
    x, y, z,
    vx: 0, vy: 0, vz: 0, fx: 0, fy: 0, fz: 0,
    sx: 0, sy: 0, sz: 0, discR: 0,
  };
}

function scene(nodes: SimNode[], clusters: DirCluster[] = []): SimWorld {
  const w = emptyWorld();
  w.nodes = nodes;
  w.clusters = clusters;
  return w;
}

function view(over: Partial<Camera> = {}): Camera {
  return { ...defaultCamera(), viewW: 800, viewH: 600, ...over };
}

describe('computeCameraBasis', () => {
  test('the three axes are unit length and mutually perpendicular', () => {
    // Not decoration: `project` dots world offsets against these, so a
    // non-orthonormal basis skews and scales the whole picture.
    const b = computeCameraBasis(view({ theta: 0.9, phi: -0.4, dist: 700 }));
    const dot = (
      a: [number, number, number],
      c: [number, number, number],
    ) => a[0] * c[0] + a[1] * c[1] + a[2] * c[2];
    const X: [number, number, number] = [b.xx, b.xy, b.xz];
    const Y: [number, number, number] = [b.yx, b.yy, b.yz];
    const Z: [number, number, number] = [b.zx, b.zy, b.zz];
    for (const v of [X, Y, Z]) expect(Math.hypot(...v)).toBeCloseTo(1, 10);
    expect(dot(X, Y)).toBeCloseTo(0, 10);
    expect(dot(X, Z)).toBeCloseTo(0, 10);
    expect(dot(Y, Z)).toBeCloseTo(0, 10);
  });

  test('the eye sits `dist` away from the target, whatever the angles', () => {
    for (const [theta, phi] of [[0, 0], [1.2, 0.6], [-2.5, -1.4]]) {
      const cam = view({ theta, phi, dist: 421, targetX: 5, targetY: -7, targetZ: 11 });
      const b = computeCameraBasis(cam);
      expect(Math.hypot(b.ex - cam.targetX, b.ey - cam.targetY, b.ez - cam.targetZ)).toBeCloseTo(
        cam.dist,
        8,
      );
    }
  });
});

describe('project', () => {
  test('a node at the camera target lands dead centre', () => {
    const cam = view({ targetX: 40, targetY: -12, targetZ: 3 });
    const n = node('a', cam.targetX, cam.targetY, cam.targetZ);
    project(cam, scene([n]), 1);
    expect(n.sx).toBeCloseTo(cam.viewW / 2, 6);
    expect(n.sy).toBeCloseTo(cam.viewH / 2, 6);
    // Depth is the distance along the view axis — the camera distance, here.
    expect(n.sz).toBeCloseTo(cam.dist, 6);
  });

  test('screen y is inverted: higher in the world is higher on the screen', () => {
    // The one sign that cannot be checked by reading `project` alone, and the
    // one that turns the whole graph upside down when it flips.
    const cam = view({ theta: 0, phi: 0 });
    const up = node('up', 0, 100, 0);
    const down = node('down', 0, -100, 0);
    project(cam, scene([up, down]), 1);
    expect(up.sy).toBeLessThan(cam.viewH / 2);
    expect(down.sy).toBeGreaterThan(cam.viewH / 2);
  });

  test('a nearer node projects bigger, and a farther one smaller', () => {
    const cam = view({ theta: 0, phi: 0 });
    // theta=phi=0 puts the eye on +z looking back at the origin.
    const near = node('near', 0, 0, 300);
    const far = node('far', 0, 0, -300);
    project(cam, scene([near, far]), 1);
    expect(near.sz).toBeLessThan(far.sz);
    expect(near.sr).toBeGreaterThan(far.sr);
  });

  test('anything at or behind the near plane is culled, not clamped', () => {
    // Clamping smeared such a node across the screen and drew distorted edges
    // to it — the comment in `project` records that. `sz < 0` is how every
    // reader downstream (renderer, both pickers) knows to skip it.
    const cam = view({ theta: 0, phi: 0, dist: 100 });
    const behind = node('behind', 0, 0, 100 - NEAR_3D / 2);
    project(cam, scene([behind]), 1);
    expect(behind.sz).toBe(-1);
    expect(behind.sr).toBe(0);
    expect(behind.sx).toBeLessThan(-1000);
  });

  test('the node-scale knob lifts the screen-radius ceiling with it', () => {
    // Otherwise scaling nodes up is silently flattened for close-by nodes.
    const cam = view({ theta: 0, phi: 0, dist: 20 });
    const big = node('big', 0, 0, 0);
    big.r = 400;
    project(cam, scene([big]), 1);
    const at1 = big.sr;
    project(cam, scene([big]), 4);
    expect(at1).toBeCloseTo(16, 6);
    expect(big.sr).toBeCloseTo(64, 6);
  });

  test('clusters project too, and a culled one loses its disc radius', () => {
    const cam = view({ theta: 0, phi: 0, dist: 100 });
    const visible = cluster('a', 0, 0, 0);
    const behind = cluster('b', 0, 0, 100 - NEAR_3D / 2);
    behind.discR = 42;
    project(cam, scene([], [visible, behind]), 1);
    expect(visible.sx).toBeCloseTo(cam.viewW / 2, 6);
    expect(behind.sz).toBe(-1);
    expect(behind.discR).toBe(0);
  });

  test('it writes into the same node objects — no copies for the renderer', () => {
    // The renderer reads `sx`/`sy`/`sr`/`sz` off the very objects the sim
    // moves. A projection that returned new objects would render last frame.
    const n = node('a', 10, 20, 30);
    const w = scene([n]);
    project(view(), w, 1);
    expect(w.nodes[0]).toBe(n);
  });
});

describe('maxDist3d', () => {
  test('a neutral or shrunk cluster spacing keeps the base ceiling', () => {
    expect(maxDist3d(1)).toBe(MAX_DIST_3D);
    expect(maxDist3d(0.2)).toBe(MAX_DIST_3D);
  });

  test('a stretched layout gets a proportionally higher ceiling', () => {
    // Without this, a graph spaced out by the 50× knob could not be framed.
    expect(maxDist3d(50)).toBe(MAX_DIST_3D * 50);
  });
});

describe('the vector helpers', () => {
  test('normalize3 returns a unit vector, and survives the zero vector', () => {
    const [x, y, z] = normalize3(3, 4, 0);
    expect(Math.hypot(x, y, z)).toBeCloseTo(1, 10);
    expect(normalize3(0, 0, 0)).toEqual([0, 0, 0]);
  });

  test('cross3 follows the right-hand rule', () => {
    expect(cross3(1, 0, 0, 0, 1, 0)).toEqual([0, 0, 1]);
  });
});
