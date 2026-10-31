//! Study-session logic for one open deck: order, filter, position.
//!
//! Pure logic, no egui. `StudySession` owns the loaded deck and its
//! [`DeckState`], tracks the current card by id (so it survives
//! reshuffles, filter changes, and deck edits), and saves through
//! [`crate::progress`]. Methods that need the time take `now` (unix
//! milliseconds) so tests are deterministic; the app passes
//! [`crate::progress::now_ms`].
//!
//! The shown card never vanishes on reveal: in the `Unseen` filter the
//! current card stays in the view after being marked seen, and drops
//! out only when advancing. The same rule applies after a save adopts
//! a merged state from another machine.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::Result;

use crate::deck::{self, Card, Deck};
use crate::progress::{self, CardFilter, CardOrder, DeckState};

/// SplitMix64 finalizer: a fast, stable 64-bit mix, identical on every
/// machine and app version (unlike the std hasher).
pub(crate) fn splitmix64(mut x: u64) -> u64 {
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

/// A fresh shuffle seed from the clock, mixed so two reshuffles in the
/// same millisecond still differ (the old seed feeds into the mix).
fn new_seed(now: i64, old_seed: u64) -> u64 {
    splitmix64((now as u64) ^ old_seed.wrapping_mul(0x9e37_79b9_7f4a_7c15))
}

/// Card indices in deck order or shuffled order. Shuffled order sorts
/// by `(splitmix64(seed ^ fnv1a(card.id)), deck index)`, so adding or
/// removing cards does not reorder the rest.
fn build_order(deck: &Deck, order: CardOrder, seed: u64) -> Vec<usize> {
    let mut indices: Vec<usize> = (0..deck.cards.len()).collect();
    if order == CardOrder::Shuffled {
        indices.sort_by_key(|&i| (splitmix64(seed ^ deck::fnv1a(&deck.cards[i].id)), i));
    }
    indices
}

/// One deck open for studying.
pub struct StudySession {
    deck: Deck,
    state: DeckState,
    state_path: PathBuf,
    /// Set when the progress file could not be read: changes still
    /// update `state` in memory but must not overwrite the file.
    save_blocked: bool,
    /// `state` has unsaved changes.
    dirty: bool,
    /// Card id of the card on screen, if any.
    current: Option<String>,
    revealed: bool,
    /// Cached card indices in the current order; rebuilt only when
    /// the order or seed changes, never per frame.
    order_cache: Vec<usize>,
    /// Deck index -> position in `order_cache`.
    rank: Vec<usize>,
    /// Card id -> deck index (first occurrence).
    by_id: HashMap<String, usize>,
}

impl StudySession {
    /// Open a session, resuming at `state.prefs.position`: that card
    /// if it is in the view, else the next card after it (in order)
    /// passing the filter, else the first card in the view. Opening
    /// stamps `opened_at` and marks the session dirty.
    pub fn new(
        deck: Deck,
        mut state: DeckState,
        state_path: PathBuf,
        save_blocked: bool,
        now: i64,
    ) -> Self {
        state.mark_opened(now);
        let by_id = deck
            .cards
            .iter()
            .enumerate()
            .rev()
            .map(|(i, card)| (card.id.clone(), i))
            .collect();
        let order_cache = build_order(&deck, state.prefs.order, state.prefs.shuffle_seed);
        let rank = ranks(&order_cache, deck.cards.len());
        let mut session = Self {
            deck,
            state,
            state_path,
            save_blocked,
            dirty: true,
            current: None,
            revealed: false,
            order_cache,
            rank,
            by_id,
        };
        let resumed = session.resume_index();
        session.set_current_index(resumed, now);
        session.dirty = true;
        session
    }

    /// The deck being studied.
    pub fn deck(&self) -> &Deck {
        &self.deck
    }

    /// The progress state for the deck.
    pub fn state(&self) -> &DeckState {
        &self.state
    }

    /// Where the progress state is saved.
    pub fn state_path(&self) -> &PathBuf {
        &self.state_path
    }

    /// Whether saving is blocked (unreadable progress file).
    pub fn save_blocked(&self) -> bool {
        self.save_blocked
    }

    /// Whether there are unsaved changes.
    pub fn dirty(&self) -> bool {
        self.dirty
    }

    /// Whether the answer is shown.
    pub fn revealed(&self) -> bool {
        self.revealed
    }

    /// The current card order preference.
    pub fn card_order(&self) -> CardOrder {
        self.state.prefs.order
    }

    /// The current card filter preference.
    pub fn filter(&self) -> CardFilter {
        self.state.prefs.filter
    }

    /// Card indices in the current order (deck or shuffled).
    pub fn order(&self) -> &[usize] {
        &self.order_cache
    }

    /// Card indices in order passing the filter, plus the current
    /// card if it no longer passes (just seen in `Unseen`, or a merged
    /// state changed its marks): the shown card never vanishes
    /// mid-study; it drops out on the next advance.
    pub fn view(&self) -> Vec<usize> {
        let current = self.current_index();
        self.order_cache
            .iter()
            .copied()
            .filter(|&i| Some(i) == current || self.passes(i))
            .collect()
    }

    /// The card on screen, if any.
    pub fn current_card(&self) -> Option<&Card> {
        self.current_index().map(|i| &self.deck.cards[i])
    }

    /// The id of the card on screen, if any.
    pub fn current_id(&self) -> Option<&str> {
        self.current.as_deref()
    }

    /// Deck index of the card on screen, if any.
    fn current_index(&self) -> Option<usize> {
        self.current
            .as_ref()
            .and_then(|id| self.by_id.get(id).copied())
    }

    /// `(n, view_len)`: 1-based position of the current card in the
    /// view and the size of the view.
    pub fn view_position(&self) -> Option<(usize, usize)> {
        let current = self.current_index()?;
        let view = self.view();
        let pos = view.iter().position(|&i| i == current)?;
        Some((pos + 1, view.len()))
    }

    /// Number of cards in the deck.
    pub fn total(&self) -> usize {
        self.deck.cards.len()
    }

    /// Number of cards in the deck seen since the last reset.
    /// Stale ids matching no card are ignored (but kept in the state).
    pub fn seen_count(&self) -> usize {
        self.state
            .seen_count(self.deck.cards.iter().map(|card| card.id.as_str()))
    }

    /// Number of cards in the deck flagged for later.
    pub fn later_count(&self) -> usize {
        self.state
            .later_count(self.deck.cards.iter().map(|card| card.id.as_str()))
    }

    /// Whether a card is flagged for later.
    pub fn is_later(&self, id: &str) -> bool {
        self.state.is_later(id)
    }

    /// Whether a card has been seen since the last reset.
    pub fn is_seen(&self, id: &str) -> bool {
        self.state.is_seen(id)
    }

    /// Whether the card at a deck index passes the current filter.
    fn passes(&self, index: usize) -> bool {
        let id = &self.deck.cards[index].id;
        match self.state.prefs.filter {
            CardFilter::All => true,
            CardFilter::Unseen => !self.state.is_seen(id),
            CardFilter::Later => self.state.is_later(id),
        }
    }

    /// Next card after `index` in order passing the filter, wrapping
    /// (a full wrap may return `index` itself if it alone passes).
    fn next_passing_after(&self, index: usize) -> Option<usize> {
        let n = self.order_cache.len();
        if n == 0 {
            return None;
        }
        let start = self.rank[index];
        (1..=n)
            .map(|step| self.order_cache[(start + step) % n])
            .find(|&i| self.passes(i))
    }

    /// First card in order passing the filter.
    fn first_passing(&self) -> Option<usize> {
        self.order_cache.iter().copied().find(|&i| self.passes(i))
    }

    /// Where to resume on open (see [`StudySession::new`]).
    fn resume_index(&self) -> Option<usize> {
        if let Some(idx) = self
            .state
            .prefs
            .position
            .as_ref()
            .and_then(|id| self.by_id.get(id).copied())
        {
            if self.passes(idx) {
                return Some(idx);
            }
            if let Some(next) = self.next_passing_after(idx) {
                return Some(next);
            }
        }
        self.first_passing()
    }

    /// Move to a card (or none), recording it in `prefs` when changed.
    fn set_current_index(&mut self, index: Option<usize>, now: i64) {
        let id = index.map(|i| self.deck.cards[i].id.clone());
        if id != self.current {
            self.current.clone_from(&id);
            self.state.prefs.position = id;
            self.state.prefs.t = now;
            self.dirty = true;
        }
    }

    fn rebuild_order(&mut self) {
        self.order_cache = build_order(
            &self.deck,
            self.state.prefs.order,
            self.state.prefs.shuffle_seed,
        );
        self.rank = ranks(&self.order_cache, self.deck.cards.len());
    }
}

impl StudySession {
    /// Space: reveal the answer (marking the card seen), or advance
    /// to the next card after the current one in order that passes
    /// the filter (wrapping) and hide the answer. If none passes,
    /// there is no current card (empty view).
    pub fn space(&mut self, now: i64) {
        let Some(idx) = self.current_index() else {
            return;
        };
        if self.revealed {
            let next = self.next_passing_after(idx);
            self.set_current_index(next, now);
            self.revealed = false;
        } else {
            let id = self.deck.cards[idx].id.clone();
            self.state.mark_seen(&id, now);
            self.dirty = true;
            self.revealed = true;
        }
    }

    /// Toggle the current card in the study-later list. Unflagging
    /// while reviewing that list removes the card at once and moves
    /// to the next flagged card after it in order (wrapping), with
    /// the answer hidden.
    pub fn toggle_later(&mut self, now: i64) {
        let Some(idx) = self.current_index() else {
            return;
        };
        let id = self.deck.cards[idx].id.clone();
        let on = !self.state.is_later(&id);
        self.state.set_later(&id, on, now);
        self.dirty = true;
        if !on && self.state.prefs.filter == CardFilter::Later {
            let next = self.next_passing_after(idx);
            self.set_current_index(next, now);
            self.revealed = false;
        }
    }

    /// Switch card order, keeping the current card (its filter
    /// membership does not change) and the answer state.
    pub fn set_order(&mut self, order: CardOrder, now: i64) {
        if self.state.prefs.order == order {
            return;
        }
        self.state.prefs.order = order;
        self.state.prefs.t = now;
        self.dirty = true;
        self.rebuild_order();
        if self.current.is_none() {
            let first = self.first_passing();
            self.set_current_index(first, now);
        }
    }

    /// Switch filter, keeping the current card if it passes the new
    /// filter, else moving to the next card after it in order that
    /// passes (wrapping) and hiding the answer.
    pub fn set_filter(&mut self, filter: CardFilter, now: i64) {
        if self.state.prefs.filter == filter {
            return;
        }
        self.state.prefs.filter = filter;
        self.state.prefs.t = now;
        self.dirty = true;
        match self.current_index() {
            Some(idx) if self.passes(idx) => {}
            Some(idx) => {
                let next = self.next_passing_after(idx);
                self.set_current_index(next, now);
                self.revealed = false;
            }
            None => {
                let first = self.first_passing();
                if first.is_some() {
                    self.set_current_index(first, now);
                    self.revealed = false;
                }
            }
        }
    }

    /// Pick a new shuffle seed from `now`, keeping the current card.
    pub fn reshuffle(&mut self, now: i64) {
        let seed = new_seed(now, self.state.prefs.shuffle_seed);
        self.reshuffle_with_seed(seed, now);
    }

    /// Shuffle with an explicit seed, keeping the current card.
    pub fn reshuffle_with_seed(&mut self, seed: u64, now: i64) {
        if self.state.prefs.shuffle_seed == seed {
            return;
        }
        self.state.prefs.shuffle_seed = seed;
        self.state.prefs.t = now;
        self.dirty = true;
        self.rebuild_order();
    }

    /// Clear seen marks (study-later flags are kept). In the
    /// `Unseen` filter, jump to the first card in order.
    pub fn reset(&mut self, now: i64) {
        self.state.reset(now);
        self.dirty = true;
        if self.state.prefs.filter == CardFilter::Unseen {
            let first = self.first_passing();
            if first != self.current_index() {
                self.set_current_index(first, now);
                self.revealed = false;
            }
        }
    }

    /// Save if dirty and not blocked, adopting the merged state.
    /// The session keeps its own prefs after adopting, so a newer
    /// prefs value from another machine never changes the view
    /// mid-session; merged flags, seen marks, and `reset_at` are
    /// adopted (the current card stays on screen until the next
    /// advance even if it no longer passes the filter).
    pub fn save(&mut self) -> Result<()> {
        if !self.dirty || self.save_blocked {
            return Ok(());
        }
        let prefs = self.state.prefs.clone();
        let merged = progress::save(&self.state_path, &self.state)?;
        self.state = merged;
        self.state.prefs = prefs;
        self.state.prefs.position.clone_from(&self.current);
        self.dirty = false;
        Ok(())
    }
}

/// Deck index -> position in `order`.
fn ranks(order: &[usize], len: usize) -> Vec<usize> {
    let mut rank = vec![0; len];
    for (pos, &idx) in order.iter().enumerate() {
        rank[idx] = pos;
    }
    rank
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::progress::Prefs;

    const OPEN: i64 = 1_000;

    fn deck(ids: &[&str]) -> Deck {
        Deck {
            name: "test".to_string(),
            path: PathBuf::from("deck.csv"),
            cards: ids
                .iter()
                .map(|id| Card {
                    id: (*id).to_string(),
                    front: format!("front {id}"),
                    back: format!("back {id}"),
                })
                .collect(),
        }
    }

    fn state(
        later: &[&str],
        seen: &[&str],
        order: CardOrder,
        filter: CardFilter,
        seed: u64,
        position: Option<&str>,
    ) -> DeckState {
        let mut state = DeckState::new();
        for id in later {
            state.set_later(id, true, 10);
        }
        for id in seen {
            state.mark_seen(id, 10);
        }
        state.prefs = Prefs {
            t: 10,
            order,
            filter,
            shuffle_seed: seed,
            position: position.map(str::to_string),
        };
        state
    }

    fn session(ids: &[&str], state: DeckState) -> StudySession {
        StudySession::new(
            deck(ids),
            state,
            PathBuf::from("unused.runki.json"),
            false,
            OPEN,
        )
    }

    fn ordered_ids(s: &StudySession) -> Vec<String> {
        s.order()
            .iter()
            .map(|&i| s.deck().cards[i].id.clone())
            .collect()
    }

    #[test]
    fn shuffled_order_is_stable_for_a_seed() {
        let ids = ["a", "b", "c", "d", "e", "f", "g", "h"];
        let mk = || {
            session(
                &ids,
                state(&[], &[], CardOrder::Shuffled, CardFilter::All, 42, None),
            )
        };
        let first = ordered_ids(&mk());
        assert_eq!(first, ordered_ids(&mk()), "same seed, same order");
        let mut sorted = first.clone();
        sorted.sort();
        assert_eq!(sorted, ids, "order is a permutation of the deck");
        let other_seed = session(
            &ids,
            state(&[], &[], CardOrder::Shuffled, CardFilter::All, 7, None),
        );
        assert_ne!(first, ordered_ids(&other_seed), "seed changes order");
    }

    #[test]
    fn shuffled_order_ignores_deck_edits() {
        let ids = ["a", "b", "c", "d", "e", "f", "g", "h"];
        let before = ordered_ids(&session(
            &ids,
            state(&[], &[], CardOrder::Shuffled, CardFilter::All, 42, None),
        ));
        // A card inserted at the front shifts every deck index.
        let mut edited = vec!["new"];
        edited.extend(ids);
        let after = ordered_ids(&session(
            &edited,
            state(&[], &[], CardOrder::Shuffled, CardFilter::All, 42, None),
        ));
        let kept: Vec<_> = after.into_iter().filter(|id| id != "new").collect();
        assert_eq!(kept, before, "adding a card does not reorder others");
    }

    #[test]
    fn reshuffle_keeps_current_card() {
        let ids = ["a", "b", "c", "d", "e", "f", "g", "h"];
        let mut s = session(
            &ids,
            state(
                &[],
                &[],
                CardOrder::Shuffled,
                CardFilter::All,
                42,
                Some("d"),
            ),
        );
        assert_eq!(s.current_id(), Some("d"));
        s.space(2_000);
        assert!(s.revealed());
        let old_order = ordered_ids(&s);

        s.reshuffle(3_000);

        assert_eq!(s.current_id(), Some("d"), "reshuffle keeps the card");
        assert!(s.revealed(), "answer state is kept");
        assert_ne!(s.state().prefs.shuffle_seed, 42);
        assert_ne!(ordered_ids(&s), old_order);
        assert_eq!(s.state().prefs.position.as_deref(), Some("d"));
    }

    #[test]
    fn reveal_marks_seen_and_advance_does_not() {
        let mut s = session(
            &["a", "b", "c"],
            state(&[], &[], CardOrder::Deck, CardFilter::All, 0, None),
        );
        s.space(2_000);
        assert!(s.revealed());
        assert!(s.is_seen("a"));
        assert!(s.dirty());
        s.space(2_100);
        assert_eq!(s.current_id(), Some("b"));
        assert!(!s.revealed());
        assert!(!s.is_seen("b"));
    }

    #[test]
    fn unseen_keeps_current_after_reveal_and_drops_it_on_advance() {
        let mut s = session(
            &["a", "b", "c"],
            state(&[], &[], CardOrder::Deck, CardFilter::Unseen, 0, None),
        );
        assert_eq!(s.view(), vec![0, 1, 2]);
        s.space(2_000); // reveal a -> seen
        assert_eq!(s.current_id(), Some("a"), "card stays on reveal");
        assert_eq!(s.view(), vec![0, 1, 2], "view still holds current");
        assert_eq!(s.view_position(), Some((1, 3)));

        s.space(2_100); // advance
        assert_eq!(s.current_id(), Some("b"));
        assert_eq!(s.view(), vec![1, 2], "seen card drops out");
        assert_eq!(s.view_position(), Some((1, 2)));
    }

    #[test]
    fn unseen_advance_wraps_around() {
        // b already seen; current is c. Revealing and advancing must
        // wrap past the end (and past nothing else unseen) to a.
        let mut s = session(
            &["a", "b", "c"],
            state(
                &[],
                &["b"],
                CardOrder::Deck,
                CardFilter::Unseen,
                0,
                Some("c"),
            ),
        );
        s.space(2_000);
        s.space(2_100);
        assert_eq!(s.current_id(), Some("a"));
        assert!(!s.revealed());
    }

    #[test]
    fn unseen_becoming_empty_clears_current() {
        let mut s = session(
            &["a"],
            state(&[], &[], CardOrder::Deck, CardFilter::Unseen, 0, None),
        );
        s.space(2_000);
        assert_eq!(s.current_id(), Some("a"));
        s.space(2_100);
        assert_eq!(s.current_id(), None);
        assert!(s.current_card().is_none());
        assert!(s.view().is_empty());
        assert_eq!(s.view_position(), None);
        assert_eq!(s.state().prefs.position, None);
    }

    #[test]
    fn reset_brings_cards_back_and_keeps_flags() {
        let mut s = session(
            &["a", "b", "c"],
            state(
                &["b"],
                &["a", "b", "c"],
                CardOrder::Deck,
                CardFilter::Unseen,
                0,
                None,
            ),
        );
        assert_eq!(s.current_id(), None, "everything seen");
        assert_eq!(s.seen_count(), 3);

        s.reset(5_000);

        assert_eq!(s.seen_count(), 0);
        assert_eq!(s.current_id(), Some("a"), "jumps to first card");
        assert!(!s.revealed());
        assert!(s.is_later("b"), "flags are kept");
        assert_eq!(s.later_count(), 1);
        assert_eq!(s.view(), vec![0, 1, 2]);
    }

    #[test]
    fn later_unflag_moves_to_next_flagged_and_hides_answer() {
        // First, middle, last (wrapping), and last remaining.
        for (position, expected) in [("a", Some("b")), ("b", Some("c")), ("c", Some("a"))] {
            let mut s = session(
                &["a", "b", "c"],
                state(
                    &["a", "b", "c"],
                    &[],
                    CardOrder::Deck,
                    CardFilter::Later,
                    0,
                    Some(position),
                ),
            );
            s.revealed = true;
            s.toggle_later(2_000);
            assert_eq!(s.current_id(), expected, "unflagging {position}");
            assert!(!s.revealed(), "next card starts hidden");
            assert!(!s.is_later(position));
        }
        let mut s = session(
            &["a", "b"],
            state(
                &["a"],
                &[],
                CardOrder::Deck,
                CardFilter::Later,
                0,
                Some("a"),
            ),
        );
        s.revealed = true;
        s.toggle_later(2_000);
        assert_eq!(s.current_id(), None);
        assert!(s.view().is_empty());
    }

    #[test]
    fn set_filter_keeps_passing_card_and_moves_off_failing_one() {
        // Keep: flagged card stays (answer still shown) in Later.
        let mut s = session(
            &["a", "b", "c"],
            state(&["b"], &[], CardOrder::Deck, CardFilter::All, 0, Some("b")),
        );
        s.revealed = true;
        s.set_filter(CardFilter::Later, 2_000);
        assert_eq!(s.current_id(), Some("b"));
        assert!(s.revealed());

        // Move: unflagged card steps to the next flagged one, hidden.
        let mut s = session(
            &["a", "b", "c"],
            state(&["c"], &[], CardOrder::Deck, CardFilter::All, 0, Some("a")),
        );
        s.revealed = true;
        s.set_filter(CardFilter::Later, 2_000);
        assert_eq!(s.current_id(), Some("c"));
        assert!(!s.revealed());

        // Move wraps: only flagged card sits before the current one.
        let mut s = session(
            &["a", "b", "c"],
            state(&["a"], &[], CardOrder::Deck, CardFilter::All, 0, Some("c")),
        );
        s.set_filter(CardFilter::Later, 2_000);
        assert_eq!(s.current_id(), Some("a"));

        // Move to none, then back to All picks the first card again.
        let mut s = session(
            &["a", "b"],
            state(&[], &[], CardOrder::Deck, CardFilter::All, 0, Some("b")),
        );
        s.set_filter(CardFilter::Later, 2_000);
        assert_eq!(s.current_id(), None);
        s.set_filter(CardFilter::All, 2_100);
        assert_eq!(s.current_id(), Some("a"));

        // Seen card moves to the next unseen one.
        let mut s = session(
            &["a", "b", "c"],
            state(&[], &["a"], CardOrder::Deck, CardFilter::All, 0, Some("a")),
        );
        s.set_filter(CardFilter::Unseen, 2_000);
        assert_eq!(s.current_id(), Some("b"));
    }

    #[test]
    fn set_order_keeps_current_card() {
        let ids = ["a", "b", "c", "d", "e", "f"];
        let mut s = session(
            &ids,
            state(&[], &[], CardOrder::Deck, CardFilter::All, 42, Some("e")),
        );
        s.revealed = true;
        s.set_order(CardOrder::Shuffled, 2_000);
        assert_eq!(s.current_id(), Some("e"));
        assert!(s.revealed());
        assert_eq!(s.card_order(), CardOrder::Shuffled);
        s.set_order(CardOrder::Deck, 2_100);
        assert_eq!(s.current_id(), Some("e"));
        assert_eq!(s.order(), &[0, 1, 2, 3, 4, 5]);
    }

    #[test]
    fn resume_fallbacks() {
        let ids = ["a", "b", "c", "d"];
        // Position present and visible.
        let s = session(
            &ids,
            state(
                &[],
                &["a"],
                CardOrder::Deck,
                CardFilter::Unseen,
                0,
                Some("c"),
            ),
        );
        assert_eq!(s.current_id(), Some("c"));

        // Position present but filtered out: next after it.
        let s = session(
            &ids,
            state(
                &[],
                &["a", "b"],
                CardOrder::Deck,
                CardFilter::Unseen,
                0,
                Some("a"),
            ),
        );
        assert_eq!(s.current_id(), Some("c"));

        // Position filtered out with nothing unseen after it: first.
        let s = session(
            &ids,
            state(
                &[],
                &["c", "d"],
                CardOrder::Deck,
                CardFilter::Unseen,
                0,
                Some("d"),
            ),
        );
        assert_eq!(s.current_id(), Some("a"));

        // Missing id: first in view.
        let s = session(
            &ids,
            state(
                &[],
                &["a"],
                CardOrder::Deck,
                CardFilter::Unseen,
                0,
                Some("gone"),
            ),
        );
        assert_eq!(s.current_id(), Some("b"));
        assert_eq!(s.state().prefs.position.as_deref(), Some("b"));

        // No position: first in view.
        let s = session(
            &ids,
            state(&["d"], &[], CardOrder::Deck, CardFilter::Later, 0, None),
        );
        assert_eq!(s.current_id(), Some("d"));
    }

    #[test]
    fn changes_update_prefs_and_mark_dirty() {
        let mut s = session(
            &["a", "b", "c"],
            state(&[], &[], CardOrder::Deck, CardFilter::All, 0, None),
        );
        s.dirty = false;

        s.revealed = true;
        s.space(2_000); // advance a -> b
        assert!(s.dirty());
        assert_eq!(s.state().prefs.position.as_deref(), Some("b"));
        assert_eq!(s.state().prefs.t, 2_000);

        s.dirty = false;
        s.set_filter(CardFilter::Unseen, 2_100);
        assert!(s.dirty());
        assert_eq!(s.state().prefs.filter, CardFilter::Unseen);
        assert_eq!(s.state().prefs.t, 2_100);

        s.dirty = false;
        s.set_order(CardOrder::Shuffled, 2_200);
        assert!(s.dirty());
        assert_eq!(s.state().prefs.order, CardOrder::Shuffled);
        assert_eq!(s.state().prefs.t, 2_200);

        s.dirty = false;
        s.reshuffle(2_300);
        assert!(s.dirty());
        assert_eq!(s.state().prefs.t, 2_300);
        assert_ne!(s.state().prefs.shuffle_seed, 0);

        s.dirty = false;
        s.set_filter(CardFilter::Unseen, 2_400); // no change
        assert!(!s.dirty(), "same filter is not a change");
        assert_eq!(s.state().prefs.t, 2_300);
    }

    #[test]
    fn counts_ignore_stale_ids() {
        let s = session(
            &["a", "b"],
            state(
                &["a", "stale-1"],
                &["b", "stale-2"],
                CardOrder::Deck,
                CardFilter::All,
                0,
                None,
            ),
        );
        assert_eq!(s.later_count(), 1);
        assert_eq!(s.seen_count(), 1);
        assert_eq!(s.total(), 2);
        assert_eq!(s.state().later.len(), 2, "stale ids stay in state");
        assert_eq!(s.state().seen.len(), 2);
    }

    #[test]
    fn save_adopts_merged_marks_but_keeps_session_prefs() {
        let dir = std::env::temp_dir().join(format!("runki-session-adopt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let state_path = dir.join("deck.csv.runki.json");

        // Session studying unseen cards in deck order, on card a.
        let st = state(&[], &[], CardOrder::Deck, CardFilter::Unseen, 5, Some("a"));
        let mut s = StudySession::new(deck(&["a", "b", "c"]), st, state_path.clone(), false, OPEN);
        s.save().expect("first save");
        assert!(!s.dirty());

        // Another machine writes: a now seen, b flagged, and prefs
        // (newer t) switched to shuffled / all on card c.
        let mut other = state(
            &["b"],
            &["a"],
            CardOrder::Shuffled,
            CardFilter::All,
            99,
            Some("c"),
        );
        other.prefs.t = 90_000;
        other.later.get_mut("b").expect("entry").t = 90_000;
        other.seen.insert("a".to_string(), 90_000);
        crate::progress::save(&state_path, &other).expect("seed disk state");

        // Reveal b locally so the session is dirty, then save.
        s.toggle_later(2_000); // flag a locally
        s.save().expect("merged save");
        assert!(!s.dirty());

        // Merged marks adopted...
        assert!(s.is_seen("a"), "seen mark from disk adopted");
        assert!(s.is_later("b"), "flag from disk adopted");
        // ...but the session keeps its own prefs and current card.
        assert_eq!(s.card_order(), CardOrder::Deck);
        assert_eq!(s.filter(), CardFilter::Unseen);
        assert_eq!(s.state().prefs.shuffle_seed, 5);
        assert_eq!(s.current_id(), Some("a"));
        assert_eq!(s.order(), &[0, 1, 2], "view order unchanged");
        // a no longer passes Unseen, yet stays until advance.
        assert!(s.current_card().is_some());
        assert_eq!(s.view(), vec![0, 1, 2]);

        s.space(3_000); // reveal (already seen; stays)
        s.space(3_100); // advance drops it
        assert_eq!(s.current_id(), Some("b"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_respects_blocked_and_clean() {
        let dir =
            std::env::temp_dir().join(format!("runki-session-blocked-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let state_path = dir.join("deck.csv.runki.json");

        let st = state(&[], &[], CardOrder::Deck, CardFilter::All, 0, None);
        let mut s = StudySession::new(deck(&["a"]), st, state_path.clone(), true, OPEN);
        assert!(s.dirty());
        s.save().expect("blocked save is a no-op");
        assert!(!state_path.exists(), "blocked save never writes");

        let st = state(&[], &[], CardOrder::Deck, CardFilter::All, 0, None);
        let mut s = StudySession::new(deck(&["a"]), st, state_path.clone(), false, OPEN);
        s.dirty = false;
        s.save().expect("clean save is a no-op");
        assert!(!state_path.exists(), "clean save writes nothing");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn toggle_in_all_keeps_card_and_answer() {
        let mut s = session(
            &["a", "b"],
            state(&[], &[], CardOrder::Deck, CardFilter::All, 0, None),
        );
        s.revealed = true;
        s.toggle_later(2_000);
        assert!(s.is_later("a"));
        assert_eq!(s.current_id(), Some("a"));
        assert!(s.revealed());
        s.toggle_later(2_100);
        assert!(!s.is_later("a"));
        assert_eq!(s.current_id(), Some("a"));
        assert!(s.revealed());
    }
}
