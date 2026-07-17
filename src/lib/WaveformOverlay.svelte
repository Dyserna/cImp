<script lang="ts">
  import { onMount, onDestroy } from 'svelte';
  import {
    latestSamples,
    onSamples,
    startAmplitudeListener,
    startMicAmplitudeListener,
  } from './audioStream';
  import { avatarVisible } from './avatarState';
  import { avatar as avatarSettings, waveform as waveformSettings } from './settings/store';

  // oxlint-disable-next-line no-unassigned-vars -- assigned via bind:this
  let canvasEl: HTMLCanvasElement;
  let ctx: CanvasRenderingContext2D | null = null;
  let animationId = 0;
  let dpr = 1;

  const BUFFER_SIZE = 1024;
  const scrollBuffer = new Float32Array(BUFFER_SIZE);

  // Samples consumed per frame, and the number of all-zero frames after which
  // the scroll buffer is guaranteed fully flat — at which point there is
  // nothing left to animate. BUFFER_SIZE / SAMPLES_PER_FRAME frames push the
  // last real sample off the end; +1 guarantees the final flat frame is drawn.
  const SAMPLES_PER_FRAME = 16;
  const SETTLE_FRAMES = BUFFER_SIZE / SAMPLES_PER_FRAME + 1;

  // Idle-out state. While audio is flowing the render loop runs at display
  // rate; once the buffer drains to a flat line and no fresh packets arrive
  // for SETTLE_FRAMES, the loop stops entirely so the WebView compositor goes
  // quiet (idle GPU drops to ~0). `wake()` — fired by the amplitude listener
  // via onSamples — restarts it the instant audio resumes.
  let running = false;
  let idleFrames = 0;

  // Mirror the visibility store into a plain local so the render loop can
  // read it synchronously without per-frame `get(store)` calls. The
  // waveform follows the avatar's hide/show toggle but no longer gates on
  // Speaking — when there's no audio, fresh packets stop arriving and the
  // visualizer scrolls in zeros, settling to a flat line.
  let avatarOn = $state(true);
  const unsubVisible = avatarVisible.subscribe((v) => (avatarOn = v));
  let lastSeenSeq = 0;

  // Mirror waveform settings into local state so the render loop reads them
  // synchronously. Updates fire a redraw on the next animation frame. An
  // empty `waveColor` is the "follow active UI theme" sentinel — resolved
  // per-frame from the `--waveform-color` CSS variable.
  let waveColor = '';
  let waveLineWidth = 2;
  let waveGlow = 0.6;
  let waveOpacity = 0.85;
  // Independent show/hide for the waveform (Settings → Waveform → Visible).
  // Reactive so the container's `hidden` class updates live on toggle.
  let waveVisible = $state(true);
  const unsubWave = waveformSettings.subscribe((w) => {
    waveVisible = w.visible;
    waveColor = w.color;
    waveLineWidth = w.line_width;
    waveGlow = w.glow_intensity;
    waveOpacity = w.opacity;
    // Reflect an appearance tweak immediately even while the loop is parked.
    requestStaticRedraw();
  });

  function effectiveWaveColor(): string {
    if (waveColor) return waveColor;
    if (!canvasEl) return '#bb55ff';
    const v = getComputedStyle(canvasEl).getPropertyValue('--waveform-color').trim();
    return v || '#bb55ff';
  }

  // Layout values mirror the avatar's so the waveform sits inside the
  // avatar's image area regardless of resize/reposition.
  let avatarWidthPx = $state(240);
  let avatarHeightPx = $state(240);
  let avatarMarginXPx = $state(16);
  let avatarMarginYPx = $state(16);
  let avatarPositionClass = $state<string>('top-right');
  const unsubAvatar = avatarSettings.subscribe((a) => {
    avatarWidthPx = a.size.width_px;
    avatarHeightPx = a.size.height_px;
    avatarMarginXPx = a.margin.x_px;
    avatarMarginYPx = a.margin.y_px;
    avatarPositionClass = a.position;
  });

  const positionStyles = $derived(
    [
      `--avatar-width: ${avatarWidthPx}px`,
      `--avatar-height: ${avatarHeightPx}px`,
      `--avatar-margin-x: ${avatarMarginXPx}px`,
      `--avatar-margin-y: ${avatarMarginYPx}px`,
    ].join(';'),
  );

  let unsubSamples: (() => void) | null = null;

  onMount(() => {
    ctx = canvasEl.getContext('2d');
    resizeCanvas();
    window.addEventListener('resize', resizeCanvas);
    startAmplitudeListener();
    // V6-01: mic capture amplitude feeds the same waveform while recording.
    startMicAmplitudeListener();
    // Restart the render loop the instant a fresh packet lands. While silent
    // no packets arrive, so the loop settles to flat and stops on its own.
    unsubSamples = onSamples(wake);
    wake();
  });

  onDestroy(() => {
    cancelAnimationFrame(animationId);
    running = false;
    unsubSamples?.();
    window.removeEventListener('resize', resizeCanvas);
    unsubVisible();
    unsubWave();
    unsubAvatar();
  });

  function resizeCanvas() {
    if (!ctx) return;
    dpr = window.devicePixelRatio || 1;
    const rect = canvasEl.getBoundingClientRect();
    canvasEl.width = Math.max(1, Math.floor(rect.width * dpr));
    canvasEl.height = Math.max(1, Math.floor(rect.height * dpr));
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    // Resizing wipes the backing store; repaint now in case we're idle and the
    // loop is parked (otherwise the canvas would stay blank until next audio).
    requestStaticRedraw();
  }

  /// Start (or no-op if already running) the display-rate render loop.
  function wake() {
    if (running) return;
    running = true;
    idleFrames = 0;
    animationId = requestAnimationFrame(render);
  }

  /// Paint a single frame without starting the loop — used to reflect a
  /// settings/size change while the loop is parked. When the loop is live the
  /// next frame already repaints, so this is a no-op then.
  function requestStaticRedraw() {
    if (running || !ctx) return;
    drawWaveform();
  }

  function render() {
    if (ctx) {
      const fresh = latestSamples.seq !== lastSeenSeq;
      lastSeenSeq = latestSamples.seq;

      if (fresh && latestSamples.current.length > 0) {
        idleFrames = 0;
        const newSamples = latestSamples.current;
        const step = Math.max(1, Math.floor(newSamples.length / SAMPLES_PER_FRAME));
        for (let i = 0; i < SAMPLES_PER_FRAME; i++) {
          const idx = i * step;
          if (idx < newSamples.length) {
            scrollBuffer.copyWithin(0, 1);
            scrollBuffer[scrollBuffer.length - 1] = newSamples[idx];
          }
        }
      } else {
        idleFrames++;
        for (let i = 0; i < SAMPLES_PER_FRAME; i++) {
          scrollBuffer.copyWithin(0, 1);
          scrollBuffer[scrollBuffer.length - 1] = 0;
        }
      }
      drawWaveform();
    }

    // Once the buffer has fully drained to zero and no fresh packets are
    // arriving, park the loop. Nothing repaints the canvas until `wake()`
    // fires, so the WebView compositor idles and GPU usage drops to ~0.
    if (idleFrames >= SETTLE_FRAMES) {
      running = false;
      return;
    }
    animationId = requestAnimationFrame(render);
  }

  function drawWaveform() {
    if (!ctx) return;
    const w = canvasEl.width / dpr;
    const h = canvasEl.height / dpr;
    ctx.clearRect(0, 0, w, h);

    const color = effectiveWaveColor();
    ctx.globalAlpha = waveOpacity;
    ctx.strokeStyle = color;
    ctx.lineWidth = waveLineWidth;
    ctx.shadowColor = color;
    ctx.shadowBlur = 12 * waveGlow;
    ctx.lineCap = 'round';
    ctx.lineJoin = 'round';

    ctx.beginPath();
    const centerY = h / 2;
    const amplitudeScale = (h / 2) * 0.9;
    for (let i = 0; i < scrollBuffer.length; i++) {
      const x = (i / scrollBuffer.length) * w;
      const y = centerY - scrollBuffer[i] * amplitudeScale;
      if (i === 0) ctx.moveTo(x, y);
      else ctx.lineTo(x, y);
    }
    ctx.stroke();
  }
</script>

<!--
  CRITICAL: this component MUST remain a sibling of <AvatarOverlay/>, never
  nested inside it. The avatar's CSS opacity sits on .avatar-overlay; if the
  waveform gets reparented under it, that opacity inherits and the
  independent-opacity acceptance criterion breaks. Keep it alongside the
  avatar in App.svelte.
-->
<div
  class="waveform-container {avatarPositionClass}"
  style={positionStyles}
  class:hidden={!avatarOn || !waveVisible}
>
  <div class="waveform-band">
    <canvas bind:this={canvasEl} class="waveform-canvas"></canvas>
  </div>
</div>

<style>
  .waveform-container {
    position: absolute;
    width: var(--avatar-width);
    height: var(--avatar-height);
    pointer-events: none;
    transition: opacity 200ms ease;
    z-index: 11;
  }
  /* Mirrors AvatarOverlay's positioning exactly so the waveform sits
     centred over the avatar's image area. Top-* positions add the same
     32px tab-bar clearance the avatar uses. */
  .waveform-container.top-right {
    top: calc(var(--avatar-margin-y) + 32px);
    right: var(--avatar-margin-x);
  }
  .waveform-container.top-left {
    top: calc(var(--avatar-margin-y) + 32px);
    left: var(--avatar-margin-x);
  }
  .waveform-container.bottom-right {
    bottom: var(--avatar-margin-y);
    right: var(--avatar-margin-x);
  }
  .waveform-container.bottom-left {
    bottom: var(--avatar-margin-y);
    left: var(--avatar-margin-x);
  }
  .hidden {
    opacity: 0;
  }
  .waveform-band {
    /* Sizes/positions the bottom band; the canvas inside fills it. We
       can't put left/right/bottom directly on the canvas because <canvas>
       is a CSS replaced element — its layout uses the intrinsic
       (HTML width/height attribute) size, which we set to backing-store
       pixels (rect.width * dpr). On displays with dpr > 1 that overflows
       the parent and the visible wave gets clipped to the left half.
       Wrapping in a plain <div> sidesteps the replaced-element rules. */
    position: absolute;
    left: 0;
    right: 0;
    bottom: calc(12% - 30px);
    height: 25%;
  }
  .waveform-canvas {
    display: block;
    width: 100%;
    height: 100%;
  }
</style>
