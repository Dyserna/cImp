# Milestone 5: Visualizer

## Goal

Add the scrolling oscilloscope waveform overlay to the avatar area. The waveform is positioned within the avatar's overall footprint but rendered as a sibling element of the avatar overlay (not a child), so its opacity is independent of the avatar's global opacity. It sits in the bottom band of the avatar's area with padding so peaks have room to expand without clipping. It reacts to live TTS audio playback via the amplitude tap from Milestone 3 and is visible only during the Speaking state.

## Why This Milestone Now

The avatar overlay (Milestone 4) and the audio pipeline with amplitude tap (Milestone 3) are both in place. The visualizer combines them into the final visual element of the v1 design. Doing it after both of its dependencies are working means we're not debugging the audio pipeline or layout while also building Canvas rendering.

## Scope

### In Scope

- A waveform overlay component rendered within the avatar's footprint, positioned in the bottom band of the avatar area
- Sibling to the avatar overlay (not a child), so the avatar's `opacity` setting does not affect the waveform's rendering
- Scrolling oscilloscope style: time on X axis, amplitude on Y axis, scrolling left as new samples arrive
- Glow effect using Canvas `shadowBlur` or layered strokes
- Independent opacity setting for the waveform (configurable in Milestone 6, hardcoded in this milestone)
- Color, line width, glow intensity, and opacity hardcoded for this milestone (settings UI is Milestone 6)
- Visualizer is only visible during the `Speaking` state; in other states it is hidden
- Backend amplitude streaming: a tokio task pulls samples from the `AmplitudeTap` at ~60Hz and emits them to the frontend via Tauri events
- Frontend Canvas rendering at the browser's animation frame rate (typically 60Hz), driven by `requestAnimationFrame`
- Smooth, jank-free animation under normal operation
- Waveform is hidden when the avatar itself is hidden (via the toggle button)
- Waveform's position and dimensions track the avatar's position and dimensions (so when settings later change the avatar's position or size, the waveform follows)

### Out of Scope

- Configurable waveform parameters via settings (Milestone 6)
- Alternate visualizer styles (spectrum analyzer, radial, etc.)
- Idle/ambient waveform animation when not speaking
- Performance profiling beyond verifying it doesn't drop frames noticeably

## Acceptance Criteria

1. When the avatar is in the Speaking state and visible, a scrolling waveform appears in the bottom band of the avatar's area, animating smoothly with the audio
2. The waveform reacts to the actual audio being played — amplitudes correlate with what is heard
3. The waveform scrolls smoothly left as new samples arrive
4. Amplitude peaks have visible glow effect; the waveform feels "alive" rather than flat
5. There is visible padding below the waveform; peaks expanding downward do not touch the bottom edge of the avatar area
6. When the avatar transitions out of Speaking (to Thinking, Idle, or Error), the waveform fades out or disappears cleanly within ~200ms
7. When transitioning back into Speaking, the waveform appears cleanly (not abruptly mid-animation)
8. **Independent opacity**: the avatar's overall opacity setting (default 80% from Milestone 4) does NOT affect the waveform. The waveform renders at its own configured opacity (default 85%) regardless of how transparent the avatar is. Setting the avatar to 30% opacity does not dim the waveform.
9. When the avatar is hidden (toggle button clicked), the waveform is also hidden
10. Animation is smooth at 60Hz under normal load; no perceptible jank during typical usage
11. Works on both Windows (WebView2) and Linux (WebKitGTK)

## Implementation Approach

### Backend: Amplitude Streaming

Add a task that pulls amplitude data from the audio output module and streams it to the frontend.

```
src-tauri/src/
  audio/
    streaming.rs    # amplitude streaming task
```

- The `AudioOutput` from Milestone 3 already exposes an `AmplitudeTap`
- A background task running at ~60Hz reads samples and emits them via Tauri event
- The task only emits when audio is playing; when the queue is empty, skip emissions to avoid pointless IPC

```
let amplitude_tap = audio_output.amplitude_tap();
let app_handle = app.handle();
tokio::spawn(async move {
    let mut interval = tokio::time::interval(Duration::from_millis(16));
    loop {
        interval.tick().await;
        if audio_output.is_playing() {
            let samples = amplitude_tap.recent_samples(1024);
            let _ = app_handle.emit("audio-amplitude", samples);
        }
    }
});
```

Tauri event payload: `Vec<f32>` works directly. If serialization is inefficient, switch to `Vec<i16>` or downsample first.

### Frontend: Visualizer Component

```
src/lib/
  WaveformOverlay.svelte
  audioStream.ts       # ref for latest amplitude data
```

#### `audioStream.ts`

For high-frequency reads from the render loop, expose latest samples via a mutable ref rather than a Svelte store:

```typescript
import { listen } from '@tauri-apps/api/event';

export const latestSamples = { current: new Float32Array(0) };

listen<number[]>('audio-amplitude', (event) => {
    latestSamples.current = new Float32Array(event.payload);
});
```

#### `WaveformOverlay.svelte`

The waveform is a separate component placed alongside the avatar overlay in `App.svelte`. It uses the same position/size config as the avatar (so it tracks the avatar's footprint) but is rendered as a sibling, not a child. This means the avatar's opacity doesn't affect it.

```svelte
<script lang="ts">
  import { onMount, onDestroy } from 'svelte';
  import { latestSamples } from './audioStream';
  import { avatarState, avatarVisible } from './avatarState';
  import { avatarConfig } from './avatarConfig';

  let canvasEl: HTMLCanvasElement;
  let ctx: CanvasRenderingContext2D;
  let animationId: number;
  let scrollBuffer: Float32Array;

  // Hardcoded for this milestone
  const config = {
    color: '#00ff88',
    lineWidth: 2,
    glowIntensity: 0.6,
    opacity: 0.85,
    bufferSize: 1024,
  };

  $: visible = $avatarVisible && $avatarState === 'Speaking';

  onMount(() => {
    ctx = canvasEl.getContext('2d')!;
    scrollBuffer = new Float32Array(config.bufferSize);
    resizeCanvas();
    window.addEventListener('resize', resizeCanvas);
    animationId = requestAnimationFrame(render);
  });

  onDestroy(() => {
    cancelAnimationFrame(animationId);
    window.removeEventListener('resize', resizeCanvas);
  });

  function resizeCanvas() {
    const dpr = window.devicePixelRatio || 1;
    const rect = canvasEl.getBoundingClientRect();
    canvasEl.width = rect.width * dpr;
    canvasEl.height = rect.height * dpr;
    ctx.scale(dpr, dpr);
  }

  function render() {
    if (visible) {
      const newSamples = latestSamples.current;
      if (newSamples.length > 0) {
        const samplesPerFrame = 16;
        const step = Math.max(1, Math.floor(newSamples.length / samplesPerFrame));
        for (let i = 0; i < samplesPerFrame; i++) {
          const idx = i * step;
          if (idx < newSamples.length) {
            scrollBuffer.copyWithin(0, 1);
            scrollBuffer[scrollBuffer.length - 1] = newSamples[idx];
          }
        }
      }
      drawWaveform();
    } else {
      ctx.clearRect(0, 0, canvasEl.width, canvasEl.height);
    }
    animationId = requestAnimationFrame(render);
  }

  function drawWaveform() {
    const w = canvasEl.width / (window.devicePixelRatio || 1);
    const h = canvasEl.height / (window.devicePixelRatio || 1);
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
    const amplitudeScale = h / 2 * 0.9;
    for (let i = 0; i < scrollBuffer.length; i++) {
      const x = (i / scrollBuffer.length) * w;
      const y = centerY - scrollBuffer[i] * amplitudeScale;
      if (i === 0) ctx.moveTo(x, y);
      else ctx.lineTo(x, y);
    }
    ctx.stroke();
  }

  // Position styling — must mirror the avatar's position and size
  $: positionClass = avatarConfig.layout.position;
  $: positionStyles = computePositionStyles(avatarConfig.layout);

  function computePositionStyles(layout: typeof avatarConfig.layout): string {
    return [
      `--avatar-width: ${layout.widthPx}px`,
      `--avatar-height: ${layout.heightPx}px`,
      `--avatar-margin: ${layout.marginPx}px`,
    ].join(';');
  }
</script>

<div class="waveform-container {positionClass}" style={positionStyles} class:hidden={!visible}>
  <canvas bind:this={canvasEl} class="waveform-canvas"></canvas>
</div>

<style>
  .waveform-container {
    position: absolute;
    width: var(--avatar-width);
    height: var(--avatar-height);
    pointer-events: none;
    transition: opacity 200ms ease;
  }
  .waveform-container.top-right {
    top: var(--avatar-margin);
    right: calc(var(--avatar-margin) + 16px); /* offset for toggle button width */
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
  .waveform-canvas {
    position: absolute;
    /* Bottom band of the avatar area: roughly bottom third with padding below */
    left: 5%;
    right: 5%;
    bottom: 12%;
    height: 25%;
  }
</style>
```

#### Mounting in App.svelte

```svelte
<script lang="ts">
  import Terminal from './lib/Terminal.svelte';
  import AvatarOverlay from './lib/AvatarOverlay.svelte';
  import WaveformOverlay from './lib/WaveformOverlay.svelte';
</script>

<main>
  <Terminal />
  <AvatarOverlay />
  <WaveformOverlay />
</main>
```

`AvatarOverlay` and `WaveformOverlay` are siblings, not parent/child. This is the architectural reason the waveform's opacity is independent of the avatar's: they don't share a CSS opacity stack.

### Glow Effect

Use Canvas `shadowBlur`, which is GPU-accelerated and produces good results. If it ever looks insufficient, fall back to layered strokes.

### Performance Considerations

The waveform renders at 60Hz with a few hundred line segments per frame — trivial work. No optimization should be needed. If profiling later shows issues, reduce sample count or lower IPC rate.

## Validation Steps

1. **Speaking state visualization**: trigger a TTS response. Verify the waveform appears in the bottom band of the avatar area, animating with the audio
2. **Audio correlation**: speak short and long, soft and loud TTS content. Verify the waveform's amplitudes correlate with what's heard
3. **Avatar visibility behind waveform**: confirm the avatar image is visible behind the waveform (waveform is semi-transparent) and the waveform itself is clearly distinguishable
4. **Position and padding**: verify the waveform sits in roughly the bottom third of the avatar area and has visible padding below; high-amplitude peaks expand into the padding without touching the avatar edge
5. **State transitions**: when avatar transitions out of Speaking, verify the waveform fades or hides cleanly within ~200ms. Transitioning back to Speaking shows the waveform cleanly without stale state
6. **Resize behavior**: not applicable in this milestone (avatar size is hardcoded). In Milestone 6, resizing the avatar should also resize the waveform; that test belongs to Milestone 6
7. **Glow effect**: verify peaks have a visible glow halo, not just a flat line
8. **Long playback**: play a long TTS response (multiple paragraphs); verify no jank, no frame drops, no memory growth
9. **Independent opacity test**: in Milestone 6 the avatar's opacity becomes settable. For this milestone, verify by code inspection that the waveform is a sibling of the avatar overlay (not a child) and that the avatar's CSS opacity does not propagate to the waveform's container or canvas. A useful manual test: temporarily reduce `avatarConfig.layout.opacity` in code to 0.3 and verify the waveform still renders at its full configured opacity.
10. **Avatar hidden hides waveform**: click the avatar's toggle button to hide the avatar; verify the waveform is also hidden during Speaking. Show the avatar again; the waveform reappears during Speaking.
11. **Cross-platform**: validate on the second platform; Canvas rendering performance should be similar but verify

## Known Risks and Mitigation

- **IPC rate vs animation rate mismatch**: backend at 60Hz, frontend at display refresh rate (often 60Hz, sometimes higher). The ring-buffer approach handles this implicitly — frames render with whatever's in the buffer.
- **High-DPI rendering**: needs `devicePixelRatio` handling on Canvas. Implemented above.
- **WebKitGTK Canvas performance**: usually fine; if the visualizer is janky on Linux, consider OffscreenCanvas or reduced redraw complexity.
- **Sample range**: PCM samples typically in [-1, 1]. Verify and clamp if needed.
- **Visualizer hides important avatar regions**: the bottom-third placement is fixed for v1. If a specific avatar's bottom region is visually critical, the user can configure a different avatar position (Milestone 6) — placing the avatar at top vs bottom doesn't change where the waveform sits within the avatar, but the user can choose what avatar art to use accordingly.
- **Position offset for toggle button**: the waveform container subtracts the toggle button's width from the side margin so it aligns with the avatar's *image* area, not the avatar+toggle combined area. This keeps the waveform centered within the visible avatar.
- **Inheriting parent opacity by accident**: if a future refactor nests the waveform under the avatar overlay, the independent-opacity property breaks. The acceptance criteria call this out explicitly. Add a code comment near the waveform mounting site reminding future contributors of this constraint.

## What "Done" Looks Like

The avatar is now visually alive. When Claude speaks, the avatar shows the Speaking state image (at the configured opacity) with a glowing waveform pulsing across its lower portion (at its own independent opacity), reactive to the actual audio. When silence returns, the waveform fades and the avatar reverts to its idle state. The waveform follows the avatar's visibility — hidden when the avatar is hidden — but its opacity is independent of the avatar's transparency.

---

## Next Milestone

Milestone 6: Settings. Brings the settings window to life, exposing all the live-updatable controls for TTS, avatar (including position, size, opacity, visibility persistence), waveform, terminal display, behavior, and shortcuts. Wires every previously-hardcoded value to the settings store.
