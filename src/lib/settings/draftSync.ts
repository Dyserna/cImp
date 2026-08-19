// Lost-update guard for the Settings window's local draft.
//
// # The defect this exists to close
//
// The Settings window keeps its own draft copy of `Settings` and edits it with
// `patch()`: clone the draft, mutate, assign, push the WHOLE struct to the
// backend (`settings_update` is a wholesale replace). It also subscribes to the
// shared settings store, which is fed by the asynchronous `settings-changed`
// broadcast, and that subscription used to replace the draft unconditionally.
//
// Those two facts together lose edits. A rapid burst —
//
//   patch(A) → patch(B) → the ECHO of A's own push arrives
//
// — replaces the draft with a state that does not contain B, so the next
// `patch(C)` clones the regressed draft and pushes it wholesale: B is now gone
// from the backend too, silently and permanently. Observed live (2026-08-19/20)
// as a machine-wide tool path, filled by one click, erased for good.
//
// # The rule
//
// A broadcast is only allowed to replace the draft while NO push of our own is
// in flight. `beginPush()` opens a window (`pushSeq`), the settle callback the
// caller must invoke when the push promise settles closes it (`ackSeq`), and
// while `pushSeq > ackSeq` the draft is authoritative — every broadcast that
// arrives is buffered instead of applied. On becoming idle the LAST buffered
// broadcast is adopted, so nothing is dropped: an external change made in the
// main window mid-burst still lands, and `applySettings`' own rollback-on-error
// (it re-`set`s the store to the pre-push state) still reaches the draft.
//
// While in flight, any broadcast is either our own echo (equal to, or older
// than, the draft — adopting it can only regress) or a genuine concurrent edit
// from another window (which our own full-snapshot push is going to clobber
// regardless — that is pre-existing, accepted behaviour, see the milestone
// notes). In both cases "the draft wins until the burst ends" is the correct
// resolution, and the buffered-then-adopted step keeps the idle end-state
// converged with the store.
//
// # Known residual (deliberate)
//
// If a stale echo is delivered AFTER the last push has settled, it arrives on
// the idle path and is applied like any other broadcast — the gate cannot tell
// a late echo from an external change without an identity on the broadcast,
// which the `settings-changed` payload does not carry. The burst window is what
// made the live loss reproducible (echo latency ≪ burst length); widening the
// fix to broadcast identity is a backend change and out of scope here.

/** Closes the window opened by {@link SettingsDraftSync.beginPush}. Idempotent. */
export type SettlePush = () => void;

/**
 * Applies a broadcast state to the draft. The Settings window passes
 * `(s) => { snapshot = structuredClone(s); }` — the same assignment the
 * subscription used to do unconditionally.
 */
export type AdoptDraft<S> = (state: S) => void;

export class SettingsDraftSync<S> {
  /** Pushes started. */
  #pushSeq = 0;
  /** Pushes whose promise has settled. */
  #ackSeq = 0;
  /**
   * The most recent broadcast suppressed because a push was in flight, wrapped
   * so that a legitimately `undefined`/`null` state is still distinguishable
   * from "nothing buffered".
   */
  #buffered: { state: S } | null = null;
  readonly #adopt: AdoptDraft<S>;

  constructor(adopt: AdoptDraft<S>) {
    this.#adopt = adopt;
  }

  /** True while at least one push has been started and not yet settled. */
  get inFlight(): boolean {
    return this.#pushSeq > this.#ackSeq;
  }

  /** How many pushes are outstanding. Diagnostics and tests only. */
  get outstanding(): number {
    return this.#pushSeq - this.#ackSeq;
  }

  /**
   * Register a push that is about to go out. Call the returned function when
   * the push promise settles — in EITHER direction: a rejected push still has
   * to close its window, otherwise the gate wedges shut and the window stops
   * accepting cross-window updates for the rest of its life.
   *
   * Extra calls to the returned function are ignored, so wiring it through both
   * `.then` and `.catch` (or `.finally`) is safe.
   */
  beginPush(): SettlePush {
    this.#pushSeq += 1;
    let settled = false;
    return () => {
      if (settled) return;
      settled = true;
      this.#ackSeq += 1;
      if (!this.inFlight) this.#flush();
    };
  }

  /**
   * A `settings-changed` broadcast arrived (via the shared store subscription).
   * Adopts it into the draft when idle; buffers it when a push is in flight.
   *
   * Returns whether the state was adopted right away — the component ignores
   * this; tests read it.
   */
  broadcast(state: S): boolean {
    if (this.inFlight) {
      this.#buffered = { state };
      return false;
    }
    this.#buffered = null;
    this.#adopt(state);
    return true;
  }

  #flush(): void {
    const pending = this.#buffered;
    this.#buffered = null;
    if (pending) this.#adopt(pending.state);
  }
}

/** Convenience constructor, matching the `newX()` style of this directory. */
export function createDraftSync<S>(adopt: AdoptDraft<S>): SettingsDraftSync<S> {
  return new SettingsDraftSync(adopt);
}
