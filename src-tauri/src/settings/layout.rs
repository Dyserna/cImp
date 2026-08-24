//! Hydration-time layout-tree integrity (V42 Phase B).
//!
//! The layout tree persisted in `settings.layout` describes a binary tree of
//! splits and panes over tab ids. Nothing on the write path validates it: the
//! frontend pushes whatever it holds, `save_layout` stores it verbatim, and the
//! file is hand-editable. Every launch therefore has to adapt the persisted
//! shape to the tab list that actually exists now — tabs deleted between
//! launches, tabs created since a preset was saved, a ratio a hand-edit set to
//! `5.0`, a pane emptied by a drop.
//!
//! That adaptation used to live in the frontend
//! (`src/lib/layout/persistence.ts`'s `validateAndRepairLayout`), which meant
//! the backend handed out a tree it knew was wrong and every consumer of
//! `settings.layout` outside the main window — `layout_focused_active_tab_id`
//! at startup, the post-repair save, a second reader added tomorrow — saw the
//! unrepaired shape. It lives here now, and the frontend receives a tree that
//! is already correct. The rules and their order are a direct port; the named
//! cases in `persistence.test.ts` came with them (see this module's tests).
//!
//! ## The rules, in the order they run
//!
//! 0. **Ratio sanity.** Every split's `ratio` is clamped into `[0.05, 0.95]`,
//!    the same band `setSplitRatio` enforces at runtime; a non-finite ratio
//!    resets to `0.5`.
//! 1. **Unknown-tab drop + tree-wide dedupe.** A tab id survives only if it is
//!    a live, non-hidden tab AND has not already been seen earlier in document
//!    order. A pane whose `active_tab_id` is no longer one of its own tabs
//!    re-points at its first.
//! 2. **Focus validation** — before orphan placement, so the orphans land in a
//!    pane that exists. An unknown `focused_pane_id` falls back to the leftmost
//!    leaf.
//! 3. **Orphan placement.** Live tabs that no pane holds are appended to the
//!    focused pane, in tab-list order. Appended to the FIRST pane matching the
//!    focused id only: a corrupt file can carry duplicate pane ids, and
//!    appending to every match would manufacture the cross-pane duplicates rule
//!    1 exists to remove.
//! 4. **Empty-pane collapse.** Non-root empty panes are closed one at a time
//!    (each close rebalances the tree, so ids gathered before it go stale),
//!    with focus re-resolved whenever the collapse removed the focused pane.
//! 5. **Empty-root rebuild.** A root that is itself an empty pane is
//!    repopulated from the live tab list. With rule 3 in front of it this can
//!    only fire when there are no live tabs at all — but nothing guarantees a
//!    non-empty tab list (`"tabs": []` is a hand-edit away), so it is handled
//!    rather than rendered as a blank app.
//!
//! ## Hidden tabs
//!
//! A UI-hidden tab is, by invariant, absent from the layout tree while still
//! present in `settings.tabs` (see `src/lib/tabs/visibility.ts`). Rules 1 and 3
//! read a tab list with the hidden ids already removed, which is what keeps
//! that invariant across a launch: a hidden tab is not an orphan to be placed,
//! and a pane that (contra the invariant — a file torn by a kill between the
//! ui-state write and the debounced layout save) still lists one loses it.
//!
//! This replaces the frontend's boot-time `stripHiddenTabsFromLayout()`, whose
//! net effect was the same. One divergence, in the torn-file case only: the
//! frontend removed the hidden tab with the runtime close op, which re-points a
//! pane's active tab at the tab to its LEFT; rule 1 re-points at the pane's
//! FIRST tab. Both land on a live tab of the same pane.
//!
//! ## What did NOT move
//!
//! Runtime mutation stays in the frontend — drag-and-drop, splitter drags, the
//! per-frame resize math, pane-id minting, focus-on-collapse. This module is
//! the hydration pass only. See `src/lib/layout/store.ts` for why the discrete
//! runtime ops are still there.

use std::collections::HashSet;

use super::schema::{LayoutNodePersisted, LayoutPersisted};

/// The ratio band. Mirrors `setSplitRatio`'s clamp in `layout/tree.ts` — a
/// pane narrower than this is invisible, and a ratio outside `[0, 1]` renders
/// one frame of negative flex before `Split.svelte`'s measured clamp catches
/// it.
const MIN_RATIO: f32 = 0.05;
const MAX_RATIO: f32 = 0.95;

/// The ratio a non-finite one resets to.
const DEFAULT_RATIO: f32 = 0.5;

/// Repair `layout` in place against the live tab list. `tab_ids` is
/// `settings.tabs` in order; `hidden` is the project's UI-hidden set (see the
/// module docs). Returns `true` when anything changed — the caller's cue to
/// persist the repaired shape rather than leave the broken one on disk.
pub fn repair(layout: &mut LayoutPersisted, tab_ids: &[&str], hidden: &HashSet<String>) -> bool {
    let repaired = repaired_layout(layout, tab_ids, hidden);
    if repaired == *layout {
        return false;
    }
    *layout = repaired;
    true
}

/// The pure half of [`repair`]: same rules, no mutation of the input.
fn repaired_layout(
    persisted: &LayoutPersisted,
    tab_ids: &[&str],
    hidden: &HashSet<String>,
) -> LayoutPersisted {
    // The tab list as the layout is allowed to see it: order preserved (orphan
    // placement is order-sensitive), hidden ids removed.
    let live: Vec<&str> = tab_ids
        .iter()
        .copied()
        .filter(|id| !hidden.contains(*id))
        .collect();
    let valid: HashSet<&str> = live.iter().copied().collect();

    // 0. Ratios first — nothing below reads them, but a later rule can clone a
    //    subtree, and cloning an unsanitized one would carry the bad value.
    let mut tree = persisted.tree.clone();
    sanitize_ratios(&mut tree);

    // 1. Drop unknown ids and dedupe tree-wide. `seen` ends up holding exactly
    //    the ids that survived, which is the "already placed" set rule 3 needs.
    let mut seen: HashSet<String> = HashSet::new();
    walk_panes_mut(&mut tree, &mut |_id, tab_ids, active| {
        tab_ids.retain(|t| valid.contains(t.as_str()) && seen.insert(t.clone()));
        let active_is_live = match active.as_deref() {
            Some(a) => tab_ids.iter().any(|t| t == a),
            None => false,
        };
        if !active_is_live {
            *active = tab_ids.first().cloned();
        }
    });

    // 2. Focus validation, BEFORE orphan placement.
    let mut focused = persisted.focused_pane_id.clone();
    if !has_pane(&tree, &focused) {
        focused = leftmost_pane_id(&tree);
    }

    // 3. Orphans -> the end of the focused pane.
    let orphans: Vec<String> = live
        .iter()
        .filter(|id| !seen.contains(**id))
        .map(|id| (*id).to_string())
        .collect();
    if !orphans.is_empty() {
        let mut placed = false;
        walk_panes_mut(&mut tree, &mut |id, tab_ids, active| {
            if placed || id != focused {
                return;
            }
            placed = true;
            tab_ids.extend(orphans.iter().cloned());
            if active.is_none() {
                *active = orphans.first().cloned();
            }
        });
    }

    // 4. Collapse non-root empty panes, one per pass: each close rebalances the
    //    tree, so an id list gathered up front would go stale mid-loop.
    while matches!(tree, LayoutNodePersisted::Split { .. }) {
        let Some(empty_id) = first_empty_pane_id(&tree) else {
            break;
        };
        let Some(collapsed) = close_pane(&tree, &empty_id) else {
            break;
        };
        tree = collapsed;
        if !has_pane(&tree, &focused) {
            focused = leftmost_pane_id(&tree);
        }
    }

    // 5. An empty root pane is repopulated from the live tab list. The pane
    //    KEEPS ITS ID rather than being replaced by a freshly-minted one the
    //    way the frontend's `defaultLayoutForTabs` did: pane-id minting is the
    //    runtime layer's job (V42 Phase B, locked), and the id is opaque to
    //    every consumer, so reusing it is the same tree by any observable
    //    measure.
    let empty_root_id = match &tree {
        LayoutNodePersisted::Pane { id, tab_ids, .. } if tab_ids.is_empty() => Some(id.clone()),
        _ => None,
    };
    if let Some(root_id) = empty_root_id {
        if !live.is_empty() {
            if let LayoutNodePersisted::Pane {
                tab_ids,
                active_tab_id,
                ..
            } = &mut tree
            {
                *tab_ids = live.iter().map(|id| (*id).to_string()).collect();
                *active_tab_id = tab_ids.first().cloned();
            }
        }
        return LayoutPersisted {
            tree,
            focused_pane_id: root_id,
        };
    }

    LayoutPersisted {
        tree,
        focused_pane_id: focused,
    }
}

/// Clamp every split's ratio into `[MIN_RATIO, MAX_RATIO]`; a non-finite one
/// (a hand-edited `NaN`, an overflowed literal) resets to [`DEFAULT_RATIO`].
fn sanitize_ratios(node: &mut LayoutNodePersisted) {
    if let LayoutNodePersisted::Split {
        ratio,
        first,
        second,
        ..
    } = node
    {
        *ratio = if ratio.is_finite() {
            ratio.clamp(MIN_RATIO, MAX_RATIO)
        } else {
            DEFAULT_RATIO
        };
        sanitize_ratios(first);
        sanitize_ratios(second);
    }
}

/// Visit every pane in document order (leftmost leaf first), handing the
/// callback the pane's id plus mutable access to its tab list and active tab.
fn walk_panes_mut(
    node: &mut LayoutNodePersisted,
    f: &mut impl FnMut(&str, &mut Vec<String>, &mut Option<String>),
) {
    match node {
        LayoutNodePersisted::Pane {
            id,
            tab_ids,
            active_tab_id,
        } => f(id, tab_ids, active_tab_id),
        LayoutNodePersisted::Split { first, second, .. } => {
            walk_panes_mut(first, f);
            walk_panes_mut(second, f);
        }
    }
}

/// Visit every pane in document order, read-only, stopping as soon as `f`
/// answers `true`. Returns whether it ever did.
fn any_pane(node: &LayoutNodePersisted, f: &mut impl FnMut(&str, &[String]) -> bool) -> bool {
    match node {
        LayoutNodePersisted::Pane { id, tab_ids, .. } => f(id, tab_ids),
        LayoutNodePersisted::Split { first, second, .. } => any_pane(first, f) || any_pane(second, f),
    }
}

/// True when some pane in `node` carries `pane_id`.
fn has_pane(node: &LayoutNodePersisted, pane_id: &str) -> bool {
    any_pane(node, &mut |id, _| id == pane_id)
}

/// The id of the first pane in document order with an empty tab list.
fn first_empty_pane_id(node: &LayoutNodePersisted) -> Option<String> {
    let mut found = None;
    any_pane(node, &mut |id, tab_ids| {
        if tab_ids.is_empty() {
            found = Some(id.to_string());
            true
        } else {
            false
        }
    });
    found
}

/// Pane id of the leftmost leaf. The deterministic focus fallback: a tree
/// always has at least one pane, so this always answers.
pub fn leftmost_pane_id(node: &LayoutNodePersisted) -> String {
    match node {
        LayoutNodePersisted::Pane { id, .. } => id.clone(),
        LayoutNodePersisted::Split { first, .. } => leftmost_pane_id(first),
    }
}

/// Standard binary-tree deletion: the split holding `pane_id` as a direct
/// child is replaced by the other child. `None` when `pane_id` is the root or
/// is not in the tree — i.e. when there is nothing to collapse.
///
/// The runtime op (`closePane` in `layout/tree.ts`) also answers with the
/// surviving sibling's leftmost leaf so the caller can move focus close to
/// where the user just was; the hydration pass has no "where the user just was"
/// to preserve and re-resolves focus over the whole tree instead, so that half
/// stays in the frontend.
fn close_pane(node: &LayoutNodePersisted, pane_id: &str) -> Option<LayoutNodePersisted> {
    let LayoutNodePersisted::Split {
        id,
        direction,
        ratio,
        first,
        second,
    } = node
    else {
        return None;
    };
    if is_pane_with_id(first, pane_id) {
        return Some((**second).clone());
    }
    if is_pane_with_id(second, pane_id) {
        return Some((**first).clone());
    }
    if let Some(replacement) = close_pane(first, pane_id) {
        return Some(LayoutNodePersisted::Split {
            id: id.clone(),
            direction: *direction,
            ratio: *ratio,
            first: Box::new(replacement),
            second: second.clone(),
        });
    }
    close_pane(second, pane_id).map(|replacement| LayoutNodePersisted::Split {
        id: id.clone(),
        direction: *direction,
        ratio: *ratio,
        first: first.clone(),
        second: Box::new(replacement),
    })
}

fn is_pane_with_id(node: &LayoutNodePersisted, pane_id: &str) -> bool {
    matches!(node, LayoutNodePersisted::Pane { id, .. } if id == pane_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::schema::SplitDirection;

    fn pane(id: &str, tab_ids: &[&str], active: Option<&str>) -> LayoutNodePersisted {
        LayoutNodePersisted::Pane {
            id: id.to_string(),
            tab_ids: tab_ids.iter().map(|t| (*t).to_string()).collect(),
            active_tab_id: active.map(str::to_string),
        }
    }

    fn split(
        id: &str,
        ratio: f32,
        first: LayoutNodePersisted,
        second: LayoutNodePersisted,
    ) -> LayoutNodePersisted {
        LayoutNodePersisted::Split {
            id: id.to_string(),
            direction: SplitDirection::Horizontal,
            ratio,
            first: Box::new(first),
            second: Box::new(second),
        }
    }

    fn state(tree: LayoutNodePersisted, focused: &str) -> LayoutPersisted {
        LayoutPersisted {
            tree,
            focused_pane_id: focused.to_string(),
        }
    }

    fn no_hidden() -> HashSet<String> {
        HashSet::new()
    }

    fn hidden(ids: &[&str]) -> HashSet<String> {
        ids.iter().map(|i| (*i).to_string()).collect()
    }

    /// Every pane in document order, as `(id, tab_ids, active)`.
    fn panes(node: &LayoutNodePersisted) -> Vec<(String, Vec<String>, Option<String>)> {
        match node {
            LayoutNodePersisted::Pane {
                id,
                tab_ids,
                active_tab_id,
            } => vec![(id.clone(), tab_ids.clone(), active_tab_id.clone())],
            LayoutNodePersisted::Split { first, second, .. } => {
                let mut out = panes(first);
                out.extend(panes(second));
                out
            }
        }
    }

    /// Every split in document order, as `(id, ratio)`.
    fn ratios(node: &LayoutNodePersisted) -> Vec<(String, f32)> {
        match node {
            LayoutNodePersisted::Pane { .. } => Vec::new(),
            LayoutNodePersisted::Split {
                id,
                ratio,
                first,
                second,
                ..
            } => {
                let mut out = vec![(id.clone(), *ratio)];
                out.extend(ratios(first));
                out.extend(ratios(second));
                out
            }
        }
    }

    fn one_pane(id: &str, tab_ids: &[&str], active: Option<&str>) -> Vec<(String, Vec<String>, Option<String>)> {
        vec![(
            id.to_string(),
            tab_ids.iter().map(|t| (*t).to_string()).collect(),
            active.map(str::to_string),
        )]
    }

    /// Port of `persistence.test.ts` — "drops tab ids no longer in
    /// settings.tabs".
    #[test]
    fn drops_tab_ids_no_longer_in_settings_tabs() {
        let mut layout = state(pane("p1", &["tabA", "tabB", "tabC"], Some("tabB")), "p1");
        assert!(repair(&mut layout, &["tabA", "tabC"], &no_hidden()));
        // tabB was active and got dropped -> first remaining.
        assert_eq!(panes(&layout.tree), one_pane("p1", &["tabA", "tabC"], Some("tabA")));
    }

    /// Port of `persistence.test.ts` — "places orphan tabs at the end of the
    /// focused pane".
    #[test]
    fn places_orphan_tabs_at_the_end_of_the_focused_pane() {
        let mut layout = state(
            split(
                "s1",
                0.5,
                pane("p1", &["tabA"], Some("tabA")),
                pane("p2", &["tabB"], Some("tabB")),
            ),
            "p2",
        );
        // tabC was created after the layout was saved; it is an orphan.
        assert!(repair(&mut layout, &["tabA", "tabB", "tabC"], &no_hidden()));
        assert_eq!(layout.focused_pane_id, "p2");
        assert_eq!(
            panes(&layout.tree)[1].1,
            vec!["tabB".to_string(), "tabC".to_string()]
        );
    }

    /// Port of `persistence.test.ts` — "falls back to leftmost leaf when
    /// focused_pane_id is invalid".
    #[test]
    fn falls_back_to_leftmost_leaf_when_focused_pane_id_is_invalid() {
        let mut layout = state(
            split(
                "s1",
                0.5,
                pane("p1", &["tabA"], Some("tabA")),
                pane("p2", &["tabB"], Some("tabB")),
            ),
            "p-does-not-exist",
        );
        assert!(repair(&mut layout, &["tabA", "tabB"], &no_hidden()));
        assert_eq!(layout.focused_pane_id, "p1");
    }

    /// Port of `persistence.test.ts` — "collapses non-root empty panes".
    #[test]
    fn collapses_non_root_empty_panes() {
        let mut layout = state(
            split(
                "s1",
                0.5,
                pane("p1", &[], None),
                pane("p2", &["tabA"], Some("tabA")),
            ),
            "p2",
        );
        assert!(repair(&mut layout, &["tabA"], &no_hidden()));
        assert_eq!(
            panes(&layout.tree),
            one_pane("p2", &["tabA"], Some("tabA")),
            "the split is gone — only the surviving sibling remains"
        );
        assert_eq!(layout.focused_pane_id, "p2");
    }

    /// Port of `persistence.test.ts` — "drop-then-orphan-then-collapse
    /// interplay".
    #[test]
    fn drop_then_orphan_then_collapse_interplay() {
        // p1 held tabX (deleted) and tabY (alive); p2 held tabZ (deleted);
        // tabN was created since the save.
        let mut layout = state(
            split(
                "s1",
                0.5,
                pane("p1", &["tabX", "tabY"], Some("tabX")),
                pane("p2", &["tabZ"], Some("tabZ")),
            ),
            "p1",
        );
        assert!(repair(&mut layout, &["tabY", "tabN"], &no_hidden()));
        assert_eq!(panes(&layout.tree), one_pane("p1", &["tabY", "tabN"], Some("tabY")));
        assert_eq!(layout.focused_pane_id, "p1");
    }

    /// Port of `persistence.test.ts` — "completely empty root pane rebuilds
    /// from defaults". The recovery runs through orphan placement, which is
    /// why rule 5 almost never fires.
    #[test]
    fn completely_empty_root_pane_rebuilds_from_defaults() {
        let mut layout = state(pane("p1", &["stale"], Some("stale")), "p1");
        assert!(repair(&mut layout, &["tabA", "tabB"], &no_hidden()));
        assert_eq!(panes(&layout.tree), one_pane("p1", &["tabA", "tabB"], Some("tabA")));
    }

    /// Port of `persistence.test.ts` — "completely empty layout with empty tabs
    /// leaves an empty pane". The one input that reaches rule 5.
    #[test]
    fn completely_empty_layout_with_empty_tabs_leaves_an_empty_pane() {
        let mut layout = state(pane("p1", &[], None), "p1");
        assert!(!repair(&mut layout, &[], &no_hidden()), "nothing to change");
        assert_eq!(panes(&layout.tree), one_pane("p1", &[], None));
        assert_eq!(layout.focused_pane_id, "p1");
    }

    /// Port of `persistence.test.ts` — "preserves a healthy multi-pane layout
    /// untouched". Also the `changed` contract: a healthy tree must not report
    /// a repair, or every launch would rewrite the settings file.
    #[test]
    fn preserves_a_healthy_multi_pane_layout_untouched() {
        let original = state(
            split(
                "s1",
                0.5,
                pane("p1", &["tabA", "tabB"], Some("tabA")),
                split(
                    "s2",
                    0.5,
                    pane("p2", &["tabC"], Some("tabC")),
                    pane("p3", &["tabD"], Some("tabD")),
                ),
            ),
            "p2",
        );
        let mut layout = original.clone();
        assert!(!repair(
            &mut layout,
            &["tabA", "tabB", "tabC", "tabD"],
            &no_hidden()
        ));
        assert_eq!(layout, original);
    }

    /// Port of `persistence.test.ts` — "dedupes a tab id repeated within one
    /// pane". A duplicate key breaks TabBar's keyed `{#each}`.
    #[test]
    fn dedupes_a_tab_id_repeated_within_one_pane() {
        let mut layout = state(pane("p1", &["tabA", "tabA", "tabB"], Some("tabA")), "p1");
        assert!(repair(&mut layout, &["tabA", "tabB"], &no_hidden()));
        assert_eq!(panes(&layout.tree), one_pane("p1", &["tabA", "tabB"], Some("tabA")));
    }

    /// Port of `persistence.test.ts` — "dedupes a tab id present in two panes —
    /// first occurrence in document order wins". A cross-pane duplicate makes
    /// two panes fight over one terminal-host element.
    #[test]
    fn dedupes_a_tab_id_present_in_two_panes_first_occurrence_wins() {
        let mut layout = state(
            split(
                "s1",
                0.5,
                pane("p1", &["tabA"], Some("tabA")),
                pane("p2", &["tabA"], Some("tabA")),
            ),
            "p1",
        );
        assert!(repair(&mut layout, &["tabA"], &no_hidden()));
        assert_eq!(
            panes(&layout.tree),
            one_pane("p1", &["tabA"], Some("tabA")),
            "the later occurrence is dropped and the emptied pane collapses"
        );
        assert_eq!(layout.focused_pane_id, "p1");
    }

    /// Port of `persistence.test.ts` — "clamps out-of-range split ratios and
    /// leaves in-range ones alone".
    #[test]
    fn clamps_out_of_range_split_ratios_and_leaves_in_range_ones_alone() {
        let mut layout = state(
            split(
                "s0",
                0.42,
                split(
                    "s1",
                    5.0,
                    pane("p1", &["tabA"], Some("tabA")),
                    pane("p2", &["tabB"], Some("tabB")),
                ),
                split(
                    "s2",
                    -3.0,
                    pane("p3", &["tabC"], Some("tabC")),
                    pane("p4", &["tabD"], Some("tabD")),
                ),
            ),
            "p1",
        );
        assert!(repair(
            &mut layout,
            &["tabA", "tabB", "tabC", "tabD"],
            &no_hidden()
        ));
        assert_eq!(
            ratios(&layout.tree),
            vec![
                ("s0".to_string(), 0.42),
                ("s1".to_string(), MAX_RATIO),
                ("s2".to_string(), MIN_RATIO),
            ]
        );
    }

    /// Port of `persistence.test.ts` — "non-finite split ratio resets to 0.5".
    #[test]
    fn non_finite_split_ratio_resets_to_half() {
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let mut layout = state(
                split(
                    "s1",
                    bad,
                    pane("p1", &["tabA"], Some("tabA")),
                    pane("p2", &["tabB"], Some("tabB")),
                ),
                "p1",
            );
            assert!(repair(&mut layout, &["tabA", "tabB"], &no_hidden()));
            assert_eq!(ratios(&layout.tree), vec![("s1".to_string(), DEFAULT_RATIO)]);
        }
    }

    /// Port of `persistence.test.ts` — "orphans are placed once even when two
    /// panes share the focused id". Appending to every match would manufacture
    /// the cross-pane duplicate rule 1 exists to remove.
    #[test]
    fn orphans_are_placed_once_even_when_two_panes_share_the_focused_id() {
        let mut layout = state(
            split(
                "s1",
                0.5,
                pane("dup", &["tabA"], Some("tabA")),
                pane("dup", &["tabB"], Some("tabB")),
            ),
            "dup",
        );
        assert!(repair(&mut layout, &["tabA", "tabB", "tabN"], &no_hidden()));
        let occurrences: usize = panes(&layout.tree)
            .iter()
            .map(|(_, tab_ids, _)| tab_ids.iter().filter(|t| *t == "tabN").count())
            .sum();
        assert_eq!(occurrences, 1);
    }

    /// The hidden-set half (was `stripHiddenTabsFromLayout` in the frontend): a
    /// hidden tab is in `settings.tabs`, so without the filter rule 3 would
    /// place it as an orphan and un-hide it on every launch.
    #[test]
    fn a_hidden_tab_is_not_placed_as_an_orphan() {
        let mut layout = state(pane("p1", &["tabA"], Some("tabA")), "p1");
        assert!(!repair(
            &mut layout,
            &["tabA", "tabHidden"],
            &hidden(&["tabHidden"])
        ));
        assert_eq!(panes(&layout.tree), one_pane("p1", &["tabA"], Some("tabA")));
    }

    /// Defensive: a file torn between the ui-state write and the debounced
    /// layout save can name a hidden tab inside a pane. It is dropped like any
    /// other non-member, and a pane holding nothing else collapses.
    #[test]
    fn a_pane_holding_only_hidden_tabs_collapses() {
        let mut layout = state(
            split(
                "s1",
                0.5,
                pane("p1", &["tabHidden"], Some("tabHidden")),
                pane("p2", &["tabA"], Some("tabA")),
            ),
            "p1",
        );
        assert!(repair(
            &mut layout,
            &["tabA", "tabHidden"],
            &hidden(&["tabHidden"])
        ));
        assert_eq!(panes(&layout.tree), one_pane("p2", &["tabA"], Some("tabA")));
        assert_eq!(layout.focused_pane_id, "p2", "focus followed the collapse");
    }

    /// Every tab hidden is not the empty-tab-list case, but it lands in the
    /// same place: an empty root pane, which the frontend renders as bare
    /// chrome rather than a blank app.
    #[test]
    fn hiding_every_tab_leaves_an_empty_root_pane() {
        let mut layout = state(pane("p1", &["tabA"], Some("tabA")), "p1");
        assert!(repair(&mut layout, &["tabA"], &hidden(&["tabA"])));
        assert_eq!(panes(&layout.tree), one_pane("p1", &[], None));
    }

    /// A pane whose `active_tab_id` is not one of its own tabs renders blank.
    /// Nothing else in this tree is wrong, so rule 1's early-out has to notice.
    #[test]
    fn an_active_tab_that_is_not_in_its_own_pane_is_re_pointed() {
        let mut layout = state(pane("p1", &["tabA", "tabB"], Some("tabZ")), "p1");
        assert!(repair(&mut layout, &["tabA", "tabB"], &no_hidden()));
        assert_eq!(panes(&layout.tree)[0].2, Some("tabA".to_string()));
    }

    /// A pane whose tabs all vanished must not keep a stale `active_tab_id`.
    #[test]
    fn an_emptied_root_pane_with_no_live_tabs_clears_its_active_tab() {
        let mut layout = state(pane("p1", &["gone"], Some("gone")), "p1");
        assert!(repair(&mut layout, &[], &no_hidden()));
        assert_eq!(panes(&layout.tree), one_pane("p1", &[], None));
    }

    /// Port of `tree.test.ts` — closePane replaces the parent split with the
    /// surviving sibling, at depth, leaving the rest of the tree alone.
    #[test]
    fn close_pane_replaces_the_parent_split_with_the_sibling() {
        let tree = split(
            "s0",
            0.5,
            pane("p1", &["a"], Some("a")),
            split(
                "s1",
                0.5,
                pane("p2", &["b"], Some("b")),
                pane("p3", &["c"], Some("c")),
            ),
        );
        let collapsed = close_pane(&tree, "p2").expect("p2 has a parent split");
        assert_eq!(
            panes(&collapsed),
            vec![
                (
                    "p1".to_string(),
                    vec!["a".to_string()],
                    Some("a".to_string())
                ),
                (
                    "p3".to_string(),
                    vec!["c".to_string()],
                    Some("c".to_string())
                ),
            ]
        );
        assert_eq!(
            ratios(&collapsed),
            vec![("s0".to_string(), 0.5)],
            "the inner split is gone, the outer one is untouched"
        );
    }

    /// closePane is a no-op for the root pane and for an unknown id — the
    /// bail-out the collapse loop relies on to terminate.
    #[test]
    fn close_pane_is_a_no_op_for_the_root_and_for_an_unknown_id() {
        assert!(close_pane(&pane("root", &[], None), "root").is_none());
        let tree = split(
            "s0",
            0.5,
            pane("p1", &["a"], Some("a")),
            pane("p2", &["b"], Some("b")),
        );
        assert!(close_pane(&tree, "nope").is_none());
    }

    /// Port of `persistence.test.ts` — "leftmostLeafPaneId returns the
    /// deepest-leftmost pane id", plus its single-pane case.
    #[test]
    fn leftmost_pane_id_finds_the_deepest_leftmost_leaf() {
        let tree = split(
            "s0",
            0.5,
            split("s1", 0.5, pane("deep", &[], None), pane("p2", &[], None)),
            pane("p3", &[], None),
        );
        assert_eq!(leftmost_pane_id(&tree), "deep");
        assert_eq!(leftmost_pane_id(&pane("only", &[], None)), "only");
    }

    /// Several empty panes at once: the loop has to keep going after each
    /// rebalance rather than reusing a stale id list.
    #[test]
    fn every_empty_pane_collapses_not_just_the_first() {
        let mut layout = state(
            split(
                "s0",
                0.5,
                pane("e1", &[], None),
                split(
                    "s1",
                    0.5,
                    pane("e2", &[], None),
                    split(
                        "s2",
                        0.5,
                        pane("e3", &[], None),
                        pane("keep", &["tabA"], Some("tabA")),
                    ),
                ),
            ),
            "keep",
        );
        assert!(repair(&mut layout, &["tabA"], &no_hidden()));
        assert_eq!(panes(&layout.tree), one_pane("keep", &["tabA"], Some("tabA")));
        assert_eq!(layout.focused_pane_id, "keep");
    }

    /// Repair is idempotent: running it over its own output changes nothing.
    /// The load path leans on this — `integrity_check` runs against both the
    /// merged settings and the global baseline, and a rule that kept finding
    /// work would rewrite the project overlay on every launch.
    #[test]
    fn repair_is_idempotent() {
        let mut layout = state(
            split(
                "s1",
                9.0,
                pane("p1", &["tabX", "tabY", "tabY"], Some("tabX")),
                pane("p2", &["gone"], Some("gone")),
            ),
            "nowhere",
        );
        let tabs = ["tabY", "tabN", "tabHidden"];
        let h = hidden(&["tabHidden"]);
        assert!(repair(&mut layout, &tabs, &h));
        let once = layout.clone();
        assert!(!repair(&mut layout, &tabs, &h), "second pass found work");
        assert_eq!(layout, once);
    }
}
