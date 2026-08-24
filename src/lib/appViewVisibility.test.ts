// `pollWhileVisible`'s contract (#130), and the rule it encodes.
//
// An app view stays MOUNTED for the app's lifetime once created (appViews.ts),
// so a periodic job in one keeps running while the tab is detached — burning
// IPC forever after the tab has been opened once. Every keep-alive poll has to
// idle off-screen. Five of them wrote that gate by hand; this is the one
// spelling, and these are the properties the five depend on.

import { afterEach, beforeEach, describe, expect, test, vi } from 'vitest';

import { pollWhileVisible, setAppViewVisible } from './appViewVisibility';
import { EVENTS_TAB_ID, WORKBENCH_TAB_ID } from './tabs/types';

beforeEach(() => {
  vi.useFakeTimers();
  setAppViewVisible(EVENTS_TAB_ID, false);
  setAppViewVisible(WORKBENCH_TAB_ID, false);
});
afterEach(() => {
  vi.useRealTimers();
});

describe('pollWhileVisible', () => {
  test('a hidden view is never ticked, however long it waits', () => {
    // THE point of the helper. Nothing else in the app notices a view that
    // keeps polling while detached — the IPC just quietly keeps happening.
    const tick = vi.fn();
    const stop = pollWhileVisible(EVENTS_TAB_ID, tick, 1000);
    vi.advanceTimersByTime(60_000);
    expect(tick).not.toHaveBeenCalled();
    stop();
  });

  test('a visible view ticks on the interval', () => {
    const tick = vi.fn();
    setAppViewVisible(EVENTS_TAB_ID, true);
    const stop = pollWhileVisible(EVENTS_TAB_ID, tick, 1000);
    vi.advanceTimersByTime(3000);
    expect(tick).toHaveBeenCalledTimes(3);
    stop();
  });

  test('it does NOT tick on start — the caller owns first-paint work', () => {
    // Every call site does its own initial fetch (in `onMount`, or via
    // `onAppViewShown`). A leading tick here would double every one of them.
    const tick = vi.fn();
    setAppViewVisible(EVENTS_TAB_ID, true);
    const stop = pollWhileVisible(EVENTS_TAB_ID, tick, 1000);
    expect(tick).not.toHaveBeenCalled();
    stop();
  });

  test('hiding a view stops the ticks; showing it again resumes them', () => {
    const tick = vi.fn();
    setAppViewVisible(EVENTS_TAB_ID, true);
    const stop = pollWhileVisible(EVENTS_TAB_ID, tick, 1000);
    vi.advanceTimersByTime(2000);
    expect(tick).toHaveBeenCalledTimes(2);
    setAppViewVisible(EVENTS_TAB_ID, false);
    vi.advanceTimersByTime(10_000);
    expect(tick).toHaveBeenCalledTimes(2);
    setAppViewVisible(EVENTS_TAB_ID, true);
    vi.advanceTimersByTime(1000);
    expect(tick).toHaveBeenCalledTimes(3);
    stop();
  });

  test('each view is gated on its OWN id', () => {
    const tick = vi.fn();
    setAppViewVisible(WORKBENCH_TAB_ID, true);
    const stop = pollWhileVisible(EVENTS_TAB_ID, tick, 1000);
    vi.advanceTimersByTime(5000);
    expect(tick).not.toHaveBeenCalled();
    stop();
  });

  test('skipWhen drops a tick without stopping the poll', () => {
    // TimelineView's `!actionBusy`: a latch action in flight must not race a
    // refresh, but the poll has to come back on its own afterwards.
    const tick = vi.fn();
    let busy = true;
    setAppViewVisible(WORKBENCH_TAB_ID, true);
    const stop = pollWhileVisible(WORKBENCH_TAB_ID, tick, 1000, { skipWhen: () => busy });
    vi.advanceTimersByTime(3000);
    expect(tick).not.toHaveBeenCalled();
    busy = false;
    vi.advanceTimersByTime(1000);
    expect(tick).toHaveBeenCalledTimes(1);
    stop();
  });

  test('skipWhen is asked only when the view is visible', () => {
    // It runs on the caller's state; a detached view should pay nothing.
    const skipWhen = vi.fn(() => false);
    const stop = pollWhileVisible(EVENTS_TAB_ID, () => {}, 1000, { skipWhen });
    vi.advanceTimersByTime(5000);
    expect(skipWhen).not.toHaveBeenCalled();
    stop();
  });

  test('the teardown really stops it', () => {
    const tick = vi.fn();
    setAppViewVisible(EVENTS_TAB_ID, true);
    const stop = pollWhileVisible(EVENTS_TAB_ID, tick, 1000);
    vi.advanceTimersByTime(1000);
    stop();
    vi.advanceTimersByTime(60_000);
    expect(tick).toHaveBeenCalledTimes(1);
  });

  test('stopping twice is harmless', () => {
    // Components tear down along more than one path (onDestroy, an effect's
    // return); a double stop must not throw.
    const stop = pollWhileVisible(EVENTS_TAB_ID, () => {}, 1000);
    stop();
    expect(() => stop()).not.toThrow();
  });
});
