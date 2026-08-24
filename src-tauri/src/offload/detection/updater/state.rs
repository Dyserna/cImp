//! The per-root [`State`] cache and the run lock, which are the two pieces of
//! process-wide state the updater owns. Everything else in this module tree is
//! a function of a `State` and a [`Layout`].

use super::*;

// ── Cached state ───────────────────────────────────────────────────────────
//
// The state file is read once per root and kept in memory. The Settings poller
// and the Advisor's signal assembly both read it every couple of seconds;
// re-parsing a JSON file on each of those would be disk churn for a value only
// this module ever writes.

pub(super) fn cache() -> &'static Mutex<HashMap<PathBuf, State>> {
    static C: OnceLock<Mutex<HashMap<PathBuf, State>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The process-wide **run lock**: at most one check/apply/revert at a time.
///
/// `staging/<component>/` is a single fixed path that [`run_component`] wipes
/// on the way in and on the way out, and `previous/<component>/<version>/` is
/// wiped by both [`activate`] and [`revert_inner`]. Two overlapping runs — a
/// scheduler tick and a Settings click is the realistic pair — would therefore
/// have one deleting what the other had just written. Async rather than a
/// `std::sync::Mutex` because [`run`] holds it across the download `await`.
pub(super) fn run_lock() -> &'static tokio::sync::Mutex<()> {
    static L: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    L.get_or_init(|| tokio::sync::Mutex::new(()))
}

/// The state under `root`, hydrating from disk on first use.
pub fn state_at(root: &Path) -> State {
    let mut g = cache().lock().unwrap_or_else(PoisonError::into_inner);
    g.entry(root.to_path_buf())
        .or_insert_with(|| store::load_state(root))
        .clone()
}

/// The state for the real layout — what Settings and the Advisor read.
pub fn state() -> State {
    match store::state_dir() {
        Some(root) => state_at(&root),
        None => State::default(),
    }
}

/// Mutate the state under `root` and persist it. A failed write is logged and
/// the in-memory copy still updates: losing the *record* of an update must not
/// make the update itself look like it never happened.
pub(super) fn update_state_at(root: &Path, f: impl FnOnce(&mut State)) {
    let mut next = state_at(root);
    f(&mut next);
    if let Err(e) = store::save_state(root, &next) {
        warn!(target: "offload", error = %e, "detection updater: could not persist state");
    }
    cache()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(root.to_path_buf(), next);
}
