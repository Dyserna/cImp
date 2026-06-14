// Speech-to-text (V6-01) frontend glue. Mirrors the backend `stt/` module:
// thin IPC wrappers for the five commands plus event listeners that drive the
// `sttState` store and append transcripts into the compose overlay.
//
// State transitions and transcripts arrive as Tauri events (`stt-state` /
// `stt-transcription`), not as command return values — the commands just post
// start/stop/cancel to the backend capture thread.

import { writable, get } from 'svelte/store';
import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { composeContent, composeOpen, openCompose } from './composeState';
import { showToast } from './toast';

export type SttState = 'idle' | 'recording' | 'transcribing' | 'error';

/// Live lifecycle state, updated from the `stt-state` event. Drives the
/// record button's visual state (idle / pulsing / spinner).
export const sttState = writable<SttState>('idle');

export const startRecording = (): Promise<void> => invoke('stt_start_recording');
export const stopRecording = (): Promise<void> => invoke('stt_stop_recording');
export const cancelRecording = (): Promise<void> => invoke('stt_cancel');
export const listSttModels = (): Promise<string[]> => invoke('stt_list_models');
export const listInputDevices = (): Promise<string[]> => invoke('stt_list_input_devices');

let inited = false;

/// Register the STT event listeners once at app startup (next to
/// `initSettings` / `installDispatcher` in App.svelte). Idempotent.
export function initStt(): void {
  if (inited) return;
  inited = true;

  void listen<{ state: SttState }>('stt-state', (e) => {
    const next = e.payload.state;
    sttState.set(next);
    if (next === 'error') {
      showToast('Speech-to-text error — check the model and microphone.');
    }
  });

  void listen<{ text: string }>('stt-transcription', (e) => {
    const text = (e.payload.text ?? '').trim();
    if (!text) {
      // Silence / too-short utterance — the backend emits an empty transcript.
      showToast("Didn't catch that.");
      return;
    }
    if (!get(composeOpen)) openCompose();
    const cur = get(composeContent);
    // Append with a single space so a second dictation doesn't clobber the
    // first (or any text the user already typed).
    composeContent.set(cur ? `${cur} ${text}` : text);
  });
}
