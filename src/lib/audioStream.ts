import { listenEvent, AUDIO_AMPLITUDE, MIC_AMPLITUDE } from './events';

// Mutable ref rather than a Svelte store: the visualizer reads this from
// requestAnimationFrame at display rate, and stores would force a reactive
// dependency chain we don't want firing at 60 Hz. `seq` lets the consumer
// distinguish a fresh packet from a re-read of the previous one — without
// it, the visualizer can't tell silence from "no new event yet" and keeps
// scrolling the last packet through the buffer forever after audio ends.
export const latestSamples: { current: Float32Array; seq: number } = {
  current: new Float32Array(0),
  seq: 0,
};

// Consumers (the waveform visualizer) register here to be woken the instant a
// fresh amplitude packet arrives. This lets the visualizer stop its
// requestAnimationFrame loop while silent and restart only when audio resumes,
// instead of repainting the canvas ~60×/s forever — a flat-line repaint that
// otherwise keeps the WebView's GPU compositor busy at idle. Plain callback set
// rather than a store, for the same 60 Hz-avoidance reason as `latestSamples`.
const sampleListeners = new Set<() => void>();

/// Register a callback fired after every fresh amplitude packet (TTS or mic).
/// Returns an unsubscribe; call it on component teardown.
export function onSamples(cb: () => void): () => void {
  sampleListeners.add(cb);
  return () => {
    sampleListeners.delete(cb);
  };
}

function notifySamples(): void {
  for (const cb of sampleListeners) cb();
}

let started = false;

/// Attach the backend amplitude listener exactly once for the lifetime of
/// the page. Deliberately does NOT return an unlisten — the listener is a
/// process-lifetime singleton, and earlier we returned a cached UnlistenFn
/// which the first onDestroy (e.g. an HMR remount) would tear down,
/// silently breaking every subsequent component instance.
export function startAmplitudeListener(): void {
  if (started) return;
  started = true;
  listenEvent(AUDIO_AMPLITUDE, (event) => {
    latestSamples.current = new Float32Array(event.payload);
    latestSamples.seq++;
    notifySamples();
  }).catch((e) => {
    // Surface the failure (the waveform would otherwise just stay flat) and
    // allow a later retry instead of latching `started` on a failed attempt.
    started = false;
    console.error('audio-amplitude listen failed', e);
  });
}

let micStarted = false;

/// V6-01: feed mic capture amplitude into the SAME visualizer buffer while
/// recording, so the existing avatar waveform reflects the user's voice
/// (reusing the component rather than building a second one). The backend
/// only emits `mic-amplitude` while recording, and TTS playback (the
/// `audio-amplitude` source) is silent then, so the two never fight over the
/// buffer. Process-lifetime singleton, like `startAmplitudeListener`.
export function startMicAmplitudeListener(): void {
  if (micStarted) return;
  micStarted = true;
  listenEvent(MIC_AMPLITUDE, (event) => {
    latestSamples.current = new Float32Array(event.payload);
    latestSamples.seq++;
    notifySamples();
  }).catch((e) => {
    micStarted = false;
    console.error('mic-amplitude listen failed', e);
  });
}
