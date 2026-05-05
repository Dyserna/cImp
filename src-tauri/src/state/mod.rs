//! Avatar state machine — per-tab edition.
//!
//! Each tab owns an independent [`TabState`] with its own avatar state and
//! activity bookkeeping; the manager keeps a `HashMap<TabId, TabState>` and
//! an `active: TabId` pointer so the frontend can know which tab's avatar is
//! currently displayed. The transition logic itself is unchanged from v1 —
//! it runs per-tab, gated by the `tab` field on each [`StateSignal`].

mod manager;

pub use manager::{spawn_state_manager, StateSignal, TabId};
