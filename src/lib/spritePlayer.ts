// Manifest-driven sprite animation player for the `sprite` avatar variant.
//
// A faithful TypeScript port of Clawdmeter's `src/sprite_player.py`: it reads a
// `manifest.json` describing pixel-art animations (each a list of frames with a
// per-frame `hold_ms`), plays the active animation looping on a canvas, and
// rotates between several animations in the current rotation list every
// `ROTATE_INTERVAL_MS`. State selection (which rotation list is active) lives in
// the caller — this class only knows animation names.
//
// Two fidelity details carried over from the Qt original:
//   1. Per-animation *shared square alpha bbox*: the visible (non-transparent)
//      pixels across all frames of an animation are cropped to one common square
//      box, so the sprite fills the widget instead of floating in a transparent
//      margin and doesn't jitter between frames.
//   2. Nearest-neighbor scaling (`imageSmoothingEnabled = false`) so 20×20 art
//      blown up to the avatar box stays crisp pixel art rather than going blurry.

interface ManifestFrame {
  file: string;
  hold_ms: number;
}

interface ManifestAnim {
  slug: string;
  category: string;
  frames: ManifestFrame[];
}

/// Maps one avatar state to the animations that play for it. When more than
/// one is listed the player rotates between them (see ROTATE_INTERVAL_MS).
/// `state` matches the app's AvatarState values ("Idle", "Speaking", …).
interface SpriteGroup {
  state: string;
  animations: string[];
}

interface Manifest {
  tile?: number;
  animations: Record<string, ManifestAnim>;
  groups?: SpriteGroup[];
}

interface Crop {
  x: number;
  y: number;
  w: number;
  h: number;
}

interface LoadedAnim {
  images: HTMLImageElement[];
  holds: number[];
  crop: Crop;
}

/// How long to dwell on one animation before rotating to the next in the active
/// list. Matches Clawdmeter's `ROTATE_INTERVAL_MS`.
const ROTATE_INTERVAL_MS = 20_000;

export class SpritePlayer {
  private canvas: HTMLCanvasElement;
  private ctx: CanvasRenderingContext2D;
  /// Offscreen 1-tile canvas reused for alpha-bbox pixel inspection.
  private probe: HTMLCanvasElement;
  private probeCtx: CanvasRenderingContext2D;

  private base = '';
  private tile = 20;
  private anims: Record<string, ManifestAnim> = {};
  /// State -> animation-name list, parsed from the manifest's `groups`. This is
  /// where per-set behaviour lives now (replacing the old hardcoded
  /// SPRITE_STATE_ANIMS); the caller resolves which list to play per state and
  /// picks a fallback when a set defines no group for a state.
  private groups: Record<string, string[]> = {};
  private cache = new Map<string, LoadedAnim>();

  private activeKey = '';
  private activeList: string[] = [];
  private rotationIdx = 0;

  private curName = '';
  private curAnim: LoadedAnim | null = null;
  private frameIdx = 0;

  private frameTimer: ReturnType<typeof setTimeout> | null = null;
  private rotateTimer: ReturnType<typeof setInterval> | null = null;
  /// Bumped on every animation switch so a slow async frame-load that resolves
  /// after the target changed is discarded instead of clobbering the display.
  private gen = 0;
  private destroyed = false;

  constructor(canvas: HTMLCanvasElement) {
    this.canvas = canvas;
    const ctx = canvas.getContext('2d');
    if (!ctx) throw new Error('SpritePlayer: 2D canvas context unavailable');
    this.ctx = ctx;
    this.probe = document.createElement('canvas');
    const pctx = this.probe.getContext('2d', { willReadFrequently: true });
    if (!pctx) throw new Error('SpritePlayer: 2D probe context unavailable');
    this.probeCtx = pctx;
  }

  /// Load a manifest from `manifestUrl` whose frame `file` paths are relative to
  /// `baseUrl`. Resets all playback state and caches. Returns the available
  /// animation names. Throws on fetch / parse failure (caller decides fallback).
  async load(manifestUrl: string, baseUrl: string): Promise<string[]> {
    const res = await fetch(manifestUrl, { cache: 'no-store' });
    if (!res.ok) throw new Error(`sprite manifest ${manifestUrl}: HTTP ${res.status}`);
    const manifest = (await res.json()) as Manifest;
    if (this.destroyed) return [];
    this.base = baseUrl.replace(/\/+$/, '');
    this.tile = manifest.tile && manifest.tile > 0 ? manifest.tile : 20;
    this.anims = manifest.animations ?? {};
    this.groups = {};
    for (const g of manifest.groups ?? []) {
      if (g && typeof g.state === 'string' && Array.isArray(g.animations)) {
        this.groups[g.state] = g.animations;
      }
    }
    this.cache.clear();
    this.activeKey = '';
    this.activeList = [];
    this.curName = '';
    this.curAnim = null;
    this.clearTimers();
    return Object.keys(this.anims);
  }

  /// Names present in the loaded manifest, in declaration order.
  animNames(): string[] {
    return Object.keys(this.anims);
  }

  /// Animation-name list the manifest's `groups` assigns to avatar `state`, or
  /// `[]` if the set defines no group for it. Returned raw — `setAnims` filters
  /// to names actually present. The caller decides the fallback for `[]`.
  groupFor(state: string): string[] {
    return this.groups[state] ?? [];
  }

  /// Switch the active rotation list. `key` is a caller-supplied identity for
  /// the list (e.g. the avatar state name); when it is unchanged the call is a
  /// no-op so the current animation keeps playing without restarting.
  ///
  /// `names` is filtered to those present in the manifest; if none survive it
  /// falls back to every available animation, guaranteeing motion for any
  /// conformant set.
  setAnims(key: string, names: string[]): void {
    if (this.destroyed || Object.keys(this.anims).length === 0) return;
    if (key === this.activeKey) return;
    this.activeKey = key;

    let list = names.filter((n) => n in this.anims);
    if (list.length === 0) list = Object.keys(this.anims);
    this.activeList = list;
    this.rotationIdx = 0;

    this.clearRotateTimer();
    void this.startAnim(list[0]);
    if (list.length > 1) {
      this.rotateTimer = setInterval(() => this.rotate(), ROTATE_INTERVAL_MS);
    }
  }

  /// Redraw the current frame at the canvas's current backing size. Call after
  /// the caller resizes `canvas.width`/`canvas.height` (e.g. on a size setting
  /// change) so the sprite re-fits without waiting for the next frame tick.
  redraw(): void {
    if (this.curAnim) this.showFrame();
  }

  destroy(): void {
    this.destroyed = true;
    this.clearTimers();
  }

  // --- internals ----------------------------------------------------------

  private clearTimers(): void {
    if (this.frameTimer !== null) {
      clearTimeout(this.frameTimer);
      this.frameTimer = null;
    }
    this.clearRotateTimer();
  }

  private clearRotateTimer(): void {
    if (this.rotateTimer !== null) {
      clearInterval(this.rotateTimer);
      this.rotateTimer = null;
    }
  }

  private rotate(): void {
    if (this.activeList.length <= 1) return;
    this.rotationIdx = (this.rotationIdx + 1) % this.activeList.length;
    void this.startAnim(this.activeList[this.rotationIdx]);
  }

  private async startAnim(name: string): Promise<void> {
    this.curName = name;
    const gen = ++this.gen;
    // Stop the outgoing animation's frame timer immediately. Otherwise a timer
    // scheduled by the previous animation's showFrame can fire advanceFrame()
    // during the loadAnim() await below — painting one or more extra frames of
    // the old animation before the new one lands, defeating the gen guard.
    if (this.frameTimer !== null) {
      clearTimeout(this.frameTimer);
      this.frameTimer = null;
    }
    let loaded: LoadedAnim;
    try {
      loaded = await this.loadAnim(name);
    } catch {
      return; // bad frames — leave whatever is on screen
    }
    // A newer switch happened (rotation, state change, reload) while we were
    // decoding frames: discard this stale result.
    if (this.destroyed || gen !== this.gen || this.curName !== name) return;
    if (loaded.images.length === 0) return;
    this.curAnim = loaded;
    this.frameIdx = 0;
    this.showFrame();
  }

  private async loadAnim(name: string): Promise<LoadedAnim> {
    const cached = this.cache.get(name);
    if (cached) return cached;

    const meta = this.anims[name];
    if (!meta) throw new Error(`sprite animation "${name}" not in manifest`);

    const images = await Promise.all(
      meta.frames.map((f) => this.loadImage(`${this.base}/${f.file}`)),
    );
    const holds = meta.frames.map((f) => Math.max(1, f.hold_ms | 0));
    const crop = this.squareAlphaBbox(images);
    const loaded: LoadedAnim = { images, holds, crop };
    this.cache.set(name, loaded);
    return loaded;
  }

  private loadImage(url: string): Promise<HTMLImageElement> {
    return new Promise((resolve, reject) => {
      const img = new Image();
      img.onload = () => resolve(img);
      img.onerror = () => reject(new Error(`sprite frame ${url} failed to load`));
      img.src = url;
    });
  }

  /// Union of every frame's non-transparent bounding box, expanded to a centered
  /// square and clamped to the tile. Mirrors `_square_alpha_bbox` in the Python
  /// original. Falls back to the full tile when a frame is fully transparent.
  private squareAlphaBbox(images: HTMLImageElement[]): Crop {
    const size = this.tile;
    this.probe.width = size;
    this.probe.height = size;

    let minX = size;
    let minY = size;
    let maxX = -1;
    let maxY = -1;

    for (const img of images) {
      this.probeCtx.clearRect(0, 0, size, size);
      this.probeCtx.drawImage(img, 0, 0, size, size);
      let data: Uint8ClampedArray;
      try {
        data = this.probeCtx.getImageData(0, 0, size, size).data;
      } catch {
        return { x: 0, y: 0, w: size, h: size }; // tainted/unreadable — use full tile
      }
      for (let y = 0; y < size; y++) {
        for (let x = 0; x < size; x++) {
          if (data[(y * size + x) * 4 + 3] > 0) {
            if (x < minX) minX = x;
            if (y < minY) minY = y;
            if (x > maxX) maxX = x;
            if (y > maxY) maxY = y;
          }
        }
      }
    }

    if (maxX < minX || maxY < minY) return { x: 0, y: 0, w: size, h: size };

    const w = maxX - minX + 1;
    const h = maxY - minY + 1;
    const side = Math.min(size, Math.max(w, h));
    // Center the square box over the content bbox, then clamp inside the tile.
    let x = Math.round(minX + w / 2 - side / 2);
    let y = Math.round(minY + h / 2 - side / 2);
    x = Math.max(0, Math.min(x, size - side));
    y = Math.max(0, Math.min(y, size - side));
    return { x, y, w: side, h: side };
  }

  private showFrame(): void {
    const anim = this.curAnim;
    if (!anim) return;
    const img = anim.images[this.frameIdx];
    const { crop } = anim;

    const cw = this.canvas.width;
    const ch = this.canvas.height;
    this.ctx.imageSmoothingEnabled = false;
    this.ctx.clearRect(0, 0, cw, ch);

    // KeepAspectRatio fit of the (square) crop into the canvas, centered —
    // matches Qt's `scaled(..., KeepAspectRatio)` behavior.
    const scale = Math.min(cw / crop.w, ch / crop.h);
    const dw = crop.w * scale;
    const dh = crop.h * scale;
    const dx = (cw - dw) / 2;
    const dy = (ch - dh) / 2;
    this.ctx.drawImage(img, crop.x, crop.y, crop.w, crop.h, dx, dy, dw, dh);

    const hold = anim.holds[this.frameIdx];
    if (this.frameTimer !== null) clearTimeout(this.frameTimer);
    this.frameTimer = setTimeout(() => this.advanceFrame(), hold);
  }

  private advanceFrame(): void {
    const anim = this.curAnim;
    if (!anim || anim.images.length === 0) return;
    this.frameIdx = (this.frameIdx + 1) % anim.images.length;
    this.showFrame();
  }
}
