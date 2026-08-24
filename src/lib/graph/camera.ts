// The Graph view's camera and perspective projection (#130, F5) — lifted
// verbatim out of `GraphView.svelte`'s `── Camera / projection ──` block.
//
// An orbit camera: `theta`/`phi` are the eye's angles about `target`, `dist`
// its distance, `focal` the perspective focal length in pixels. `project`
// turns the world into screen coordinates for one frame.
//
// SAME NON-REACTIVE CONTRACT AS `sim.ts`: `project` writes `sx`/`sy`/`sr`/`sz`
// on every node and `sx`/`sy`/`sz` on every cluster, in place, every frame.
// Those objects are plain, never `$state`, and this file allocates exactly one
// object per frame — the camera basis — which is what the old in-component
// version did too.

import { clamp, type SimWorld } from './sim';

/// Nearest renderable camera distance, and the base of the farthest.
export const MIN_DIST_3D = 80;
export const MAX_DIST_3D = 8000;
/// Near plane, in camera-space depth units. Anything closer is culled.
export const NEAR_3D = 8;

/// The cluster-spacing knob stretches the layout roughly linearly, so the
/// distance ceiling stretches with it — otherwise a spaced-out graph could not
/// be framed in one view.
export function maxDist3d(clusterSpacing: number): number {
  return MAX_DIST_3D * Math.max(1, clusterSpacing);
}

/// Where the eye is and what it is looking at, plus the canvas size the
/// projection maps into. One of these per view, mutated in place by the drag,
/// wheel and fit handlers.
export interface Camera {
  theta: number;
  phi: number;
  dist: number;
  focal: number;
  targetX: number;
  targetY: number;
  targetZ: number;
  viewW: number;
  viewH: number;
}

/// The camera the view opens on (`fitView` immediately overrides most of it).
export function defaultCamera(): Camera {
  return {
    theta: 0.6,
    phi: 0.35,
    dist: 600,
    focal: 480,
    targetX: 0,
    targetY: 0,
    targetZ: 0,
    viewW: 0,
    viewH: 0,
  };
}

/// Eye position plus the three orthonormal camera axes, in world space.
export interface CameraBasis {
  ex: number; ey: number; ez: number;
  xx: number; xy: number; xz: number;
  yx: number; yy: number; yz: number;
  zx: number; zy: number; zz: number;
}

export function normalize3(x: number, y: number, z: number): [number, number, number] {
  const len = Math.hypot(x, y, z) || 1;
  return [x / len, y / len, z / len];
}

export function cross3(
  ax: number, ay: number, az: number,
  bx: number, by: number, bz: number,
): [number, number, number] {
  return [ay * bz - az * by, az * bx - ax * bz, ax * by - ay * bx];
}

export function computeCameraBasis(cam: Camera): CameraBasis {
  const ex = cam.targetX + cam.dist * Math.cos(cam.phi) * Math.sin(cam.theta);
  const ey = cam.targetY + cam.dist * Math.sin(cam.phi);
  const ez = cam.targetZ + cam.dist * Math.cos(cam.phi) * Math.cos(cam.theta);
  const [zx, zy, zz] = normalize3(ex - cam.targetX, ey - cam.targetY, ez - cam.targetZ);
  let [xx, xy, xz] = cross3(0, 1, 0, zx, zy, zz);
  const rl = Math.hypot(xx, xy, xz) || 1;
  xx /= rl; xy /= rl; xz /= rl;
  const [yx, yy, yz] = cross3(zx, zy, zz, xx, xy, xz);
  return { ex, ey, ez, xx, xy, xz, yx, yy, yz, zx, zy, zz };
}

/// Project every node and cluster into screen space, in place.
///
/// `nodeScale` is the node-size knob: the screen-radius ceiling grows with it
/// so scaling up isn't silently flattened for close-by nodes.
export function project(cam: Camera, w: SimWorld, nodeScale: number): void {
  const b = computeCameraBasis(cam);
  for (const n of w.nodes) {
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
    const scale = cam.focal / fz;
    n.sx = cam.viewW / 2 + rx * scale;
    n.sy = cam.viewH / 2 - ry * scale;
    n.sr = clamp(n.r * scale, 1, 16 * Math.max(1, nodeScale));
    n.sz = fz;
  }
  for (const c of w.clusters) {
    const dx = c.x - b.ex, dy = c.y - b.ey, dz = c.z - b.ez;
    const rx = dx * b.xx + dy * b.xy + dz * b.xz;
    const ry = dx * b.yx + dy * b.yy + dz * b.yz;
    const fz = -(dx * b.zx + dy * b.zy + dz * b.zz);
    if (fz < NEAR_3D) {
      c.sx = -1e6; c.sy = -1e6; c.sz = -1; c.discR = 0;
      continue;
    }
    const scale = cam.focal / fz;
    c.sx = cam.viewW / 2 + rx * scale;
    c.sy = cam.viewH / 2 - ry * scale;
    c.sz = fz;
  }
}
