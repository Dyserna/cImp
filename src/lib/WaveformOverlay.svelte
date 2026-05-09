<script lang="ts">
  import { onMount, onDestroy } from 'svelte';
  import { latestSamples, startAmplitudeListener } from './audioStream';
  import { avatarVisible } from './avatarState';
  import { avatar as avatarSettings, waveform as waveformSettings } from './settings/store';

  let canvasEl: HTMLCanvasElement;
  let ctx: CanvasRenderingContext2D | null = null;
  let animationId = 0;
  let dpr = 1;

  const BUFFER_SIZE = 1024;
  const scrollBuffer = new Float32Array(BUFFER_SIZE);

  // Mirror the visibility store into a plain local so the render loop can
  // read it synchronously without per-frame `get(store)` calls. The
  // waveform follows the avatar's hide/show toggle but no longer gates on
  // Speaking — when there's no audio, fresh packets stop arriving and the
  // visualizer scrolls in zeros, settling to a flat line.
  let avatarOn = $state(true);
  const unsubVisible = avatarVisible.subscribe((v) => (avatarOn = v));
  let lastSeenSeq = 0;

  // Mirror waveform settings into local state so the render loop reads them
  // synchronously. Updates fire a redraw on the next animation frame.
  let waveColor = '#bb55ff';
  let waveLineWidth = 2;
  let waveGlow = 0.6;
  let waveOpacity = 0.85;
  const unsubWave = waveformSettings.subscribe((w) => {
    waveColor = w.color;
    waveLineWidth = w.line_width;
    waveGlow = w.glow_intensity;
    waveOpacity = w.opacity;
  });

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

  onMount(() => {
    ctx = canvasEl.getContext('2d');
    resizeCanvas();
    window.addEventListener('resize', resizeCanvas);
    startAmplitudeListener();
    animationId = requestAnimationFrame(render);
  });

  onDestroy(() => {
    cancelAnimationFrame(animationId);
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
  }

  function render() {
    if (ctx) {
      const samplesPerFrame = 16;
      const fresh = latestSamples.seq !== lastSeenSeq;
      lastSeenSeq = latestSamples.seq;

      if (fresh && latestSamples.current.length > 0) {
        const newSamples = latestSamples.current;
        const step = Math.max(1, Math.floor(newSamples.length / samplesPerFrame));
        for (let i = 0; i < samplesPerFrame; i++) {
          const idx = i * step;
          if (idx < newSamples.length) {
            scrollBuffer.copyWithin(0, 1);
            scrollBuffer[scrollBuffer.length - 1] = newSamples[idx];
          }
        }
      } else {
        for (let i = 0; i < samplesPerFrame; i++) {
          scrollBuffer.copyWithin(0, 1);
          scrollBuffer[scrollBuffer.length - 1] = 0;
        }
      }
      drawWaveform();
    }
    animationId = requestAnimationFrame(render);
  }

  function drawWaveform() {
    if (!ctx) return;
    const w = canvasEl.width / dpr;
    const h = canvasEl.height / dpr;
    ctx.clearRect(0, 0, w, h);

    ctx.globalAlpha = waveOpacity;
    ctx.strokeStyle = waveColor;
    ctx.lineWidth = waveLineWidth;
    ctx.shadowColor = waveColor;
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
  class:hidden={!avatarOn}
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
