//! egui front-end: one card at a time.
//!
//! - `Space` shows the answer, then moves to the next card (wrapping
//!   around at the end, so browsing never stops).
//! - `L` toggles the current card in the study-later list.
//! - The top bar switches between all cards and study-later only.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use eframe::egui;

use crate::deck::{self, Deck};
use crate::progress::{self, DeckState, LoadOutcome};

/// How often a dirty state is written while studying.
const SAVE_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    All,
    Later,
}

pub struct RunkiApp {
    deck: Option<Deck>,
    /// Index into the current view (all cards, or flagged only).
    pos: usize,
    revealed: bool,
    mode: Mode,
    state: DeckState,
    state_path: Option<PathBuf>,
    /// Set when the progress file could not be read: changes still
    /// update `state` in memory but must not overwrite the file.
    save_blocked: bool,
    /// `state` has unsaved changes.
    dirty: bool,
    last_save_attempt: Option<Instant>,
    error: Option<String>,
    /// Error from the last save attempt; cleared by the next
    /// successful save. Kept apart from `error` so a background save
    /// never hides an unrelated message.
    save_error: Option<String>,
}

impl RunkiApp {
    pub fn new(initial_deck: Option<PathBuf>) -> Self {
        let mut app = Self {
            deck: None,
            pos: 0,
            revealed: false,
            mode: Mode::All,
            state: DeckState::new(),
            state_path: None,
            save_blocked: false,
            dirty: false,
            last_save_attempt: None,
            error: None,
            save_error: None,
        };
        if let Some(path) = initial_deck {
            app.open_path(path);
        }
        app
    }

    fn open_path(&mut self, path: PathBuf) {
        match deck::load_deck(&path) {
            Ok(deck) => {
                // Always save the outgoing deck's progress first, and
                // say so if that failed: its unsaved changes are lost.
                self.save_now();
                let outgoing_error = self.save_error.take().map(|err| {
                    let name = self.deck.as_ref().map_or("the previous deck", |d| &d.name);
                    format!("Progress for {name} could not be saved: {err}")
                });
                let state_path = progress::state_path(&deck.path, None);
                let outcome = progress::load(&deck.path, &state_path);
                self.save_blocked = outcome.is_blocked();
                let load_error = match &outcome {
                    LoadOutcome::Blocked { error, .. } => Some(format!(
                        "{error:#}. Progress could not be read, so \
                         changes in this session won't be saved until the file \
                         is fixed or removed."
                    )),
                    _ => None,
                };
                self.error = match (outgoing_error, load_error) {
                    (Some(a), Some(b)) => Some(format!("{a}\n{b}")),
                    (a, b) => a.or(b),
                };
                // A migrated state only exists in memory until saved.
                self.dirty = outcome.is_migrated();
                self.state = outcome.into_state();
                self.state_path = Some(state_path);
                // Opening is itself a state change.
                self.state.mark_opened(progress::now_ms());
                self.dirty = true;
                self.deck = Some(deck);
                self.pos = 0;
                self.revealed = false;
                self.mode = Mode::All;
            }
            Err(err) => {
                self.error = Some(format!("{err:#}"));
            }
        }
    }

    /// Save now if there is anything to save. Used on deck switch and
    /// on exit; periodic saves go through the debounced path.
    fn save_now(&mut self) {
        self.save_now_at(Instant::now());
    }

    fn save_now_at(&mut self, now: Instant) {
        if !self.dirty || self.save_blocked {
            return;
        }
        let Some(state_path) = &self.state_path else {
            return;
        };
        if self.deck.is_none() {
            return;
        }
        self.last_save_attempt = Some(now);
        match progress::save(state_path, &self.state) {
            Ok(merged) => {
                self.state = merged;
                self.dirty = false;
                self.save_error = None;
            }
            Err(err) => {
                self.save_error = Some(format!("{err:#}"));
            }
        }
    }

    /// Save if dirty and at least [`SAVE_INTERVAL`] has passed since
    /// the last save attempt. Returns how long until the next attempt
    /// is due, so the caller can schedule a repaint for it.
    fn maybe_save_debounced_at(&mut self, now: Instant) -> Option<Duration> {
        if !self.dirty || self.save_blocked {
            return None;
        }
        let due = self
            .last_save_attempt
            .is_none_or(|last| now.saturating_duration_since(last) >= SAVE_INTERVAL);
        if due {
            self.save_now_at(now);
        }
        // Still dirty (not due yet, or the save failed): next attempt
        // is due one interval after the last one.
        self.dirty.then(|| {
            self.last_save_attempt
                .map(|last| (last + SAVE_INTERVAL).saturating_duration_since(now))
                .unwrap_or(Duration::ZERO)
        })
    }

    fn pick_and_open(&mut self) {
        if let Some(path) = rfd::FileDialog::new()
            .add_filter("Decks", &["apkg", "csv", "tsv", "txt"])
            .pick_file()
        {
            self.open_path(path);
        }
    }

    /// Indices of the flagged cards, visible in `Mode::Later`.
    fn later_view(&self) -> Vec<usize> {
        let Some(deck) = &self.deck else {
            return Vec::new();
        };
        deck.cards
            .iter()
            .enumerate()
            .filter(|(_, card)| self.state.is_later(&card.id))
            .map(|(i, _)| i)
            .collect()
    }

    /// Number of cards visible in the current mode.
    fn view_len(&self) -> usize {
        let Some(deck) = &self.deck else {
            return 0;
        };
        match self.mode {
            Mode::All => deck.cards.len(),
            Mode::Later => self.later_count(),
        }
    }

    /// Index into `deck.cards` of the card at `self.pos` in the view.
    fn current_index(&self) -> Option<usize> {
        let deck = self.deck.as_ref()?;
        match self.mode {
            Mode::All => (self.pos < deck.cards.len()).then_some(self.pos),
            Mode::Later => self.later_view().get(self.pos).copied(),
        }
    }

    fn current(&self) -> Option<(&deck::Card, usize, usize)> {
        let deck = self.deck.as_ref()?;
        let idx = self.current_index()?;
        Some((&deck.cards[idx], self.pos + 1, self.view_len()))
    }

    /// Space: reveal the answer, or advance (wrapping) if already shown.
    /// Revealing marks the card seen.
    fn space(&mut self) {
        let view_len = self.view_len();
        if view_len == 0 {
            return;
        }
        if self.revealed {
            self.pos = (self.pos + 1) % view_len;
            self.revealed = false;
        } else {
            if let Some(card_id) = self.current().map(|(card, _, _)| card.id.clone()) {
                self.state.mark_seen(&card_id, progress::now_ms());
                self.dirty = true;
            }
            self.revealed = true;
        }
    }

    /// L: toggle the current card in the study-later list.
    /// The change is saved by the debounced saver.
    fn toggle_later(&mut self) {
        let Some(card_id) = self.current().map(|(card, _, _)| card.id.clone()) else {
            return;
        };
        let on = !self.state.is_later(&card_id);
        self.state.set_later(&card_id, on, progress::now_ms());
        self.dirty = true;
        // Unflagging while reviewing the later list shrinks the view;
        // keep the position pointing at the next remaining card.
        let view_len = self.view_len();
        if view_len == 0 {
            self.pos = 0;
        } else if self.pos >= view_len {
            self.pos = view_len - 1;
        }
        // If a different card is now shown, hide its answer.
        if self.current().map(|(c, _, _)| &c.id) != Some(&card_id) {
            self.revealed = false;
        }
    }

    /// Number of cards in the open deck that are flagged for later.
    /// Stale ids matching no card are ignored (but kept in the state).
    fn later_count(&self) -> usize {
        let Some(deck) = &self.deck else {
            return 0;
        };
        self.state
            .later_count(deck.cards.iter().map(|card| card.id.as_str()))
    }

    fn set_mode(&mut self, mode: Mode) {
        if self.mode != mode {
            self.mode = mode;
            self.pos = 0;
            self.revealed = false;
        }
    }
}

impl eframe::App for RunkiApp {
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.save_now();
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Drag-and-drop a deck file onto the window to open it.
        let dropped = ctx.input(|i| i.raw.dropped_files.iter().find_map(|f| f.path.clone()));
        if let Some(path) = dropped {
            self.open_path(path);
        }

        // Consume Space/L before widgets see them, so a focused button
        // cannot also trigger on Space and double-advance.
        let space = ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Space));
        let later_key = ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::L));
        if space {
            self.space();
        }
        if later_key {
            self.toggle_later();
        }

        // Debounced save, with a repaint scheduled so a pending save
        // happens even without further input.
        if let Some(delay) = self.maybe_save_debounced_at(Instant::now()) {
            ctx.request_repaint_after(delay);
        }

        self.top_bar(ctx);
        self.bottom_bar(ctx);
        self.central(ctx);
    }
}

impl RunkiApp {
    fn top_bar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.horizontal(|ui| {
                if ui.button("Open deck...").clicked() {
                    self.pick_and_open();
                }
                ui.separator();
                match &self.deck {
                    Some(deck) => {
                        ui.strong(&deck.name);
                        ui.label(format!("{} cards", deck.cards.len()));
                    }
                    None => {
                        ui.label("No deck open");
                    }
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let later_label = format!("Study later ({})", self.later_count());
                    if ui
                        .selectable_label(self.mode == Mode::Later, later_label)
                        .clicked()
                    {
                        self.set_mode(Mode::Later);
                    }
                    if ui
                        .selectable_label(self.mode == Mode::All, "All cards")
                        .clicked()
                    {
                        self.set_mode(Mode::All);
                    }
                });
            });
        });
    }

    fn bottom_bar(&mut self, ctx: &egui::Context) {
        let current = self
            .current()
            .map(|(card, n, total)| (self.state.is_later(&card.id), n, total));
        egui::TopBottomPanel::bottom("bottom").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.add_enabled_ui(current.is_some(), |ui| {
                    let label = if self.revealed {
                        "Next card  (Space)"
                    } else {
                        "Show answer  (Space)"
                    };
                    if ui
                        .add(egui::Button::new(egui::RichText::new(label).size(18.0)))
                        .clicked()
                    {
                        self.space();
                    }
                    let flagged = current.is_some_and(|(flagged, _, _)| flagged);
                    let label = if flagged {
                        "In study-later list  (L)"
                    } else {
                        "Study later  (L)"
                    };
                    let button =
                        egui::Button::new(egui::RichText::new(label).size(18.0)).selected(flagged);
                    if ui.add(button).clicked() {
                        self.toggle_later();
                    }
                });
                if let Some((_, n, total)) = current {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(format!("{n} / {total}"));
                    });
                }
            });
        });
    }

    fn central(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default().show(ctx, |ui| {
            for slot in [&mut self.error, &mut self.save_error] {
                if let Some(error) = slot.clone() {
                    ui.add_space(16.0);
                    ui.horizontal(|ui| {
                        ui.colored_label(egui::Color32::LIGHT_RED, error);
                        if ui.small_button("×").clicked() {
                            *slot = None;
                        }
                    });
                    ui.add_space(16.0);
                }
            }

            let Some(current) = self.current() else {
                ui.vertical_centered(|ui| {
                    ui.add_space(80.0);
                    if self.deck.is_none() {
                        ui.heading("Runki");
                        ui.add_space(8.0);
                        ui.label("Open an .apkg, .csv, .tsv, or .txt deck, or drop the file here.");
                        ui.add_space(4.0);
                        ui.label("Space: show answer / next card.  L: study later.");
                        ui.add_space(16.0);
                        if ui
                            .add(egui::Button::new(
                                egui::RichText::new("Open deck...").size(20.0),
                            ))
                            .clicked()
                        {
                            self.pick_and_open();
                        }
                    } else if self.mode == Mode::Later {
                        ui.heading("Nothing flagged yet");
                        ui.add_space(8.0);
                        ui.label("Press L on a card to add it to this list.");
                    } else {
                        ui.heading("This deck has no cards to show");
                    }
                });
                return;
            };

            let (card, _, _) = current;
            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.add_space(24.0);
                ui.vertical_centered(|ui| {
                    if self.state.is_later(&card.id) {
                        ui.small("STUDY LATER");
                        ui.add_space(8.0);
                    }
                    ui.add(egui::Label::new(egui::RichText::new(&card.front).size(30.0)).wrap());
                    if self.revealed {
                        ui.add_space(16.0);
                        ui.separator();
                        ui.add_space(16.0);
                        ui.add(egui::Label::new(egui::RichText::new(&card.back).size(26.0)).wrap());
                    }
                    ui.add_space(24.0);
                });
            });
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deck::Card;

    struct TempDir(std::path::PathBuf);

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn test_app(
        test_name: &str,
        later: &[&str],
        mode: Mode,
        pos: usize,
        revealed: bool,
    ) -> (RunkiApp, TempDir) {
        let dir =
            std::env::temp_dir().join(format!("runki-test-{}-{}", test_name, std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let guard = TempDir(dir.clone());
        let cards = ["a", "b", "c"]
            .into_iter()
            .map(|id| Card {
                id: id.to_string(),
                front: format!("front {id}"),
                back: format!("back {id}"),
            })
            .collect();
        let deck_path = dir.join("deck.csv");
        let mut state = DeckState::new();
        for id in later {
            state.set_later(id, true, 0);
        }
        let state_path = progress::state_path(&deck_path, None);
        let app = RunkiApp {
            deck: Some(Deck {
                name: "test".to_string(),
                path: deck_path,
                cards,
            }),
            pos,
            revealed,
            mode,
            state,
            state_path: Some(state_path),
            save_blocked: false,
            dirty: false,
            last_save_attempt: None,
            error: None,
            save_error: None,
        };
        (app, guard)
    }

    #[test]
    fn later_unflag_first_hides_next_cards_answer() {
        let (mut app, _temp) =
            test_app("later-unflag-first", &["a", "b", "c"], Mode::Later, 0, true);

        app.toggle_later();

        assert_eq!(app.pos, 0);
        assert!(!app.revealed);
        let (card, _, _) = app.current().expect("next card shown");
        assert_eq!(card.id, "b");
    }

    #[test]
    fn later_unflag_middle_hides_next_cards_answer() {
        let (mut app, _temp) = test_app(
            "later-unflag-middle",
            &["a", "b", "c"],
            Mode::Later,
            1,
            true,
        );

        app.toggle_later();

        assert_eq!(app.pos, 1);
        assert!(!app.revealed);
        let (card, _, _) = app.current().expect("next card shown");
        assert_eq!(card.id, "c");
    }

    #[test]
    fn later_unflag_last_remaining_empties_view() {
        let (mut app, _temp) = test_app("later-unflag-last", &["a"], Mode::Later, 0, true);

        app.toggle_later();

        assert_eq!(app.pos, 0);
        assert!(!app.revealed);
        assert!(app.current().is_none());
        assert_eq!(app.view_len(), 0);
    }

    #[test]
    fn later_count_ignores_stale_ids() {
        let (app, _temp) = test_app(
            "later-count-stale",
            &["a", "stale-1", "stale-2"],
            Mode::All,
            0,
            false,
        );

        assert_eq!(app.later_count(), 1);
        // Stale ids are not deleted from the in-memory state (or file).
        assert_eq!(app.state.later.len(), 3);
        assert!(app.state.is_later("stale-1"));
    }

    #[test]
    fn error_cleared_after_successful_save() {
        let (mut app, _temp) = test_app("error-cleared-on-save", &[], Mode::All, 0, false);
        // Make the first save fail by occupying the sidecar path with a dir.
        let deck_path = app.deck.as_ref().expect("deck").path.clone();
        let sidecar = progress::state_path(&deck_path, None);
        std::fs::create_dir_all(&sidecar).expect("create blocking dir");

        app.toggle_later();
        app.save_now();
        assert!(app.save_error.is_some(), "failed save should set error");
        assert!(app.dirty, "failed save stays dirty for a retry");

        std::fs::remove_dir_all(&sidecar).expect("remove blocking dir");
        app.toggle_later();
        app.save_now();
        assert_eq!(app.save_error, None, "successful save should clear error");
        assert!(!app.dirty);
    }

    #[test]
    fn successful_save_keeps_unrelated_error() {
        let (mut app, _temp) = test_app("save-keeps-error", &[], Mode::All, 0, false);
        app.error = Some("Could not open other.apkg".to_string());

        app.toggle_later();
        app.save_now();

        assert!(!app.dirty);
        assert_eq!(app.error.as_deref(), Some("Could not open other.apkg"));
    }

    #[test]
    fn failed_save_on_deck_switch_is_reported() {
        let (mut app, temp) = test_app("switch-save-fails", &[], Mode::All, 0, false);
        let deck_path = app.deck.as_ref().expect("deck").path.clone();
        // Make saving the outgoing deck fail.
        std::fs::create_dir_all(progress::state_path(&deck_path, None))
            .expect("create blocking dir");
        app.toggle_later();
        let next = temp.0.join("next.tsv");
        std::fs::write(&next, "Q\tA\n").expect("write next deck");

        app.open_path(next);

        let error = app.error.as_deref().expect("lost progress is reported");
        assert!(
            error.contains("Progress for test could not be saved"),
            "{error}"
        );
        assert_eq!(app.save_error, None);
    }

    #[test]
    fn all_toggle_flag_keeps_answer_shown_on_same_card() {
        let (mut app, _temp) = test_app("all-toggle-flag", &[], Mode::All, 0, true);

        app.toggle_later();

        assert!(app.revealed);
        assert!(app.state.is_later("a"));
        let (card, _, _) = app.current().expect("card shown");
        assert_eq!(card.id, "a");

        app.toggle_later();

        assert!(app.revealed);
        assert!(!app.state.is_later("a"));
        let (card, _, _) = app.current().expect("card shown");
        assert_eq!(card.id, "a");
    }

    #[test]
    fn reveal_marks_card_seen() {
        let (mut app, _temp) = test_app("reveal-marks-seen", &[], Mode::All, 0, false);
        assert!(!app.state.is_seen("a"));

        app.space();

        assert!(app.revealed);
        assert!(app.state.is_seen("a"), "revealing marks the card seen");
        assert!(app.dirty);

        // Advancing does not mark the next card.
        app.space();
        assert!(!app.revealed);
        assert!(!app.state.is_seen("b"));
    }

    #[test]
    fn debounced_save_writes_file() {
        let (mut app, _temp) = test_app("debounced-save", &[], Mode::All, 0, false);
        let state_path = app.state_path.clone().expect("state path");
        assert!(!state_path.exists());

        // First save attempt is immediate (nothing attempted yet).
        let now = Instant::now();
        app.toggle_later();
        assert_eq!(app.maybe_save_debounced_at(now), None);
        assert!(state_path.exists(), "debounced save writes the file");
        assert!(!app.dirty);

        // A new change inside the interval waits, then saves once due.
        app.toggle_later();
        let delay = app
            .maybe_save_debounced_at(now + Duration::from_secs(1))
            .expect("pending save should schedule a retry");
        assert_eq!(delay, Duration::from_secs(1));
        assert!(app.dirty);
        app.maybe_save_debounced_at(now + Duration::from_secs(2));
        assert!(!app.dirty);
        let loaded = progress::load(&app.deck.as_ref().expect("deck").path, &state_path);
        assert!(!loaded.state().is_later("a"), "second toggle was saved");
    }

    #[test]
    fn switching_decks_saves_outgoing_deck() {
        let dir =
            std::env::temp_dir().join(format!("runki-test-switch-saves-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let first = dir.join("first.tsv");
        std::fs::write(&first, "Q1\tA1\n").expect("write first deck");
        let second = dir.join("second.tsv");
        std::fs::write(&second, "Q2\tA2\n").expect("write second deck");

        let mut app = RunkiApp::new(Some(first.clone()));
        assert!(app.state.opened_at > 0, "opening stamps opened_at");
        app.space();
        app.open_path(second);

        let first_state = progress::state_path(&first, None);
        assert!(first_state.exists(), "switching saved the first deck");
        let loaded = progress::load(&first, &first_state);
        assert_eq!(
            loaded.state().seen_count(["x"].into_iter()),
            0,
            "sanity: unknown ids are not seen"
        );
        assert!(
            loaded.state().seen.values().any(|&t| t > 0),
            "seen mark was saved on switch"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_later_file_is_not_overwritten_by_toggle() {
        let dir = std::env::temp_dir().join(format!(
            "runki-test-corrupt-later-toggle-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let deck_path = dir.join("deck.tsv");
        std::fs::write(&deck_path, "Q1\tA1\nQ2\tA2\n").expect("write deck");
        let corrupt = "not valid json {{{";
        // Legacy sidecar: a corrupt one blocks saving (migration path).
        let legacy = dir.join("deck.tsv.later.json");
        std::fs::write(&legacy, corrupt).expect("write corrupt legacy file");
        let state_path = progress::state_path(&deck_path, None);

        let mut app = RunkiApp::new(Some(deck_path));
        assert!(app.deck.is_some(), "deck still opens");
        assert!(app.save_blocked);
        assert!(
            app.error
                .as_deref()
                .is_some_and(|e| e.contains("could not be read")),
            "error should explain saving is blocked: {:?}",
            app.error
        );

        app.toggle_later();
        app.save_now();

        assert_eq!(
            app.state.later.len(),
            1,
            "toggle still updates in-memory state"
        );
        assert_eq!(
            std::fs::read_to_string(&legacy).expect("read legacy file"),
            corrupt,
            "corrupt file must be unchanged on disk"
        );
        assert!(!state_path.exists(), "blocked save never writes state");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_state_file_is_not_overwritten() {
        let dir = std::env::temp_dir().join(format!(
            "runki-test-corrupt-state-toggle-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let deck_path = dir.join("deck.tsv");
        std::fs::write(&deck_path, "Q1\tA1\n").expect("write deck");
        let corrupt = "not valid json {{{";
        let state_path = progress::state_path(&deck_path, None);
        std::fs::write(&state_path, corrupt).expect("write corrupt state");

        let mut app = RunkiApp::new(Some(deck_path));
        assert!(app.save_blocked);

        app.toggle_later();
        app.space();
        app.save_now();
        app.maybe_save_debounced_at(Instant::now());

        assert_eq!(
            std::fs::read_to_string(&state_path).expect("read state"),
            corrupt,
            "blocked save must leave the file untouched"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn opening_good_deck_resets_later_save_blocked() {
        let dir = std::env::temp_dir().join(format!(
            "runki-test-corrupt-later-reset-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let bad_path = dir.join("bad.tsv");
        std::fs::write(&bad_path, "Q1\tA1\n").expect("write bad deck");
        std::fs::write(dir.join("bad.tsv.later.json"), "not valid json {{{")
            .expect("write corrupt legacy file");
        let good_path = dir.join("good.tsv");
        std::fs::write(&good_path, "Q1\tA1\n").expect("write good deck");

        let mut app = RunkiApp::new(Some(bad_path));
        assert!(app.save_blocked);

        app.open_path(good_path.clone());
        assert!(!app.save_blocked);
        assert!(app.error.is_none());

        app.toggle_later();
        app.save_now();
        assert!(
            progress::state_path(&good_path, None).exists(),
            "save works after the flag is reset"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
