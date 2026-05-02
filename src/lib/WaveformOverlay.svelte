<script lang="ts">
  import { onMount, onDestroy } from 'svelte';
  import { latestSamples, startAmplitudeListener } from './audioStream';
  import { avatarVisible } from './avatarState';
  import { avatarConfig } from './avatarConfig';

  let canvasEl: HTMLCanvasElement;
  let ctx: CanvasRenderingContext2D | null = null;
  let animationId = 0;
  let dpr = 1;

  // Hardcoded in M5; Milestone 6 wires these to the settings store.
  const config = {
    color: '#bb55ff',
    lineWidth: 2,
    glowIntensity: 0.6,
    opacity: 0.85,
    bufferSize: 1024,
  };

  const scrollBuffer = new Float32Array(config.bufferSize);

  // Mirror the visibility store into a plain local so the render loop can
  // read it synchronously without per-frame `get(store)` calls. The
  // waveform follows the avatar's hide/show toggle but no longer gates on
  // Speaking — when there's no audio, fresh packets stop arriving and the
  // visualizer scrolls in zeros, settling to a flat line.
  let avatarOn = true;
  const unsubVisible = avatarVisible.subscribe((v) => (avatarOn = v));
  let lastSeenSeq = 0;

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
        // Take a thin slice from the freshest packet. The backend ring is
        // wider than one frame's worth of samples, so older values age
        // out naturally as we scroll left.
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
        // No new packet this frame → audio is silent. Scroll in zeros so
        // the waveform settles to a flat line instead of looping the last
        // packet's samples through the buffer indefinitely.
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

    ctx.globalAlpha = config.opacity;
    ctx.strokeStyle = config.color;
    ctx.lineWidth = config.lineWidth;
    ctx.shadowColor = config.color;
    ctx.shadowBlur = 12 * config.glowIntensity;
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

  // Layout mirrors AvatarOverlay so the waveform sits inside the avatar's
  // image area. The toggle button lives outside .avatar-overlay so we
  // offset by 16 px on the toggle side to stay aligned with the image,
  // not the image+toggle combined footprint.
  const positionStyles = (() => {
    const { widthPx, heightPx, marginPx } = avatarConfig.layout;
    return [
      `--avatar-width: ${widthPx}px`,
      `--avatar-height: ${heightPx}px`,
      `--avatar-margin: ${marginPx}px`,
    ].join(';');
  })();

  const positionClass = avatarConfig.layout.position;
</script>

<!--
  CRITICAL: this component MUST remain a sibling of <AvatarOverlay/>, never
  nested inside it. The avatar's CSS opacity sits on .avatar-overlay; if the
  waveform gets reparented under it, that opacity inherits and the
  independent-opacity acceptance criterion breaks. Keep it alongside the
  avatar in App.svelte.
-->
<div
  class="waveform-container {positionClass}"
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
  .waveform-container.top-right {
    top: var(--avatar-margin);
    right: calc(var(--avatar-margin) + 16px);
  }
  .waveform-container.top-left {
    top: var(--avatar-margin);
    left: calc(var(--avatar-margin) + 16px);
  }
  .waveform-container.bottom-right {
    bottom: var(--avatar-margin);
    right: calc(var(--avatar-margin) + 16px);
  }
  .waveform-container.bottom-left {
    bottom: var(--avatar-margin);
    left: calc(var(--avatar-margin) + 16px);
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
