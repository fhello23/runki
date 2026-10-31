//! egui front-end: one card at a time.
//!
//! - `Space` shows the answer, then moves to the next card (wrapping
//!   around at the end, so browsing never stops).
//! - `L` toggles the current card in the study-later list.
//! - The top bar picks the card order (deck order or shuffled) and
//!   which cards to show (all, unseen only, or study-later only).
//!
//! Study logic (order, filter, position, saving) lives in
//! [`crate::session::StudySession`]; this module keeps the save
//! debounce, error banners, file picker, and drag-and-drop.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use eframe::egui;

use crate::deck::{self, Card};
use crate::progress::{self, CardFilter, CardOrder, LoadOutcome};
use crate::session::StudySession;

/// How often a dirty state is written while studying.
const SAVE_INTERVAL: Duration = Duration::from_secs(2);

pub struct RunkiApp {
    session: Option<StudySession>,
    last_save_attempt: Option<Instant>,
    /// The reset-progress confirmation dialog is open.
    show_reset_confirm: bool,
    error: Option<String>,
    /// Error from the last save attempt; cleared by the next
    /// successful save. Kept apart from `error` so a background save
    /// never hides an unrelated message.
    save_error: Option<String>,
}

impl RunkiApp {
    pub fn new(initial_deck: Option<PathBuf>) -> Self {
        let mut app = Self {
            session: None,
            last_save_attempt: None,
            show_reset_confirm: false,
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
                let outgoing_name = self.session.as_ref().map(|s| s.deck().name.clone());
                self.save_now();
                let outgoing_error = self.save_error.take().map(|err| {
                    let name = outgoing_name.as_deref().unwrap_or("the previous deck");
                    format!("Progress for {name} could not be saved: {err}")
                });
                let state_path = progress::state_path(&deck.path, None);
                let outcome = progress::load(&deck.path, &state_path);
                let save_blocked = outcome.is_blocked();
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
                let state = outcome.into_state();
                self.session = Some(StudySession::new(
                    deck,
                    state,
                    state_path,
                    save_blocked,
                    progress::now_ms(),
                ));
                self.show_reset_confirm = false;
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
        let Some(session) = &mut self.session else {
            return;
        };
        if !session.dirty() || session.save_blocked() {
            return;
        }
        self.last_save_attempt = Some(now);
        match session.save() {
            Ok(()) => {
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
        let session = self.session.as_ref()?;
        if !session.dirty() || session.save_blocked() {
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
        self.session
            .as_ref()
            .is_some_and(StudySession::dirty)
            .then(|| {
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

    /// The card on screen with its `(n, view_len)` position.
    fn current(&self) -> Option<(&Card, usize, usize)> {
        let session = self.session.as_ref()?;
        let card = session.current_card()?;
        let (n, total) = session.view_position()?;
        Some((card, n, total))
    }

    /// Space: reveal the answer, or advance if already shown.
    fn space(&mut self) {
        if let Some(session) = &mut self.session {
            session.space(progress::now_ms());
        }
    }

    /// L: toggle the current card in the study-later list.
    fn toggle_later(&mut self) {
        if let Some(session) = &mut self.session {
            session.toggle_later(progress::now_ms());
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
        // cannot also trigger on Space and double-advance. While the
        // reset confirmation is open they are consumed but ignored.
        let space = ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Space));
        let later_key = ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::L));
        let escape = ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
        if escape {
            self.show_reset_confirm = false;
        }
        if !self.show_reset_confirm {
            if space {
                self.space();
            }
            if later_key {
                self.toggle_later();
            }
        }

        // Debounced save, with a repaint scheduled so a pending save
        // happens even without further input.
        if let Some(delay) = self.maybe_save_debounced_at(Instant::now()) {
            ctx.request_repaint_after(delay);
        }

        self.top_bar(ctx);
        self.bottom_bar(ctx);
        self.central(ctx);
        self.reset_confirm(ctx);
    }
}

impl RunkiApp {
    fn top_bar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.horizontal_wrapped(|ui| {
                if ui.button("Open deck...").clicked() {
                    self.pick_and_open();
                }
                ui.separator();
                match &self.session {
                    Some(session) => {
                        ui.strong(&session.deck().name);
                        let seen = session.seen_count();
                        ui.label(format!("{seen} / {} seen", session.total()));
                        // Starting over must not require seeing every card first.
                        if seen > 0
                            && ui
                                .small_button("Reset…")
                                .on_hover_text("Clear which cards you have seen")
                                .clicked()
                        {
                            self.show_reset_confirm = true;
                        }
                    }
                    None => {
                        ui.label("No deck open");
                    }
                }
                let Some(session) = &mut self.session else {
                    return;
                };
                let now = progress::now_ms();
                ui.separator();
                ui.label("Order:");
                if ui
                    .selectable_label(session.card_order() == CardOrder::Deck, "Deck order")
                    .clicked()
                {
                    session.set_order(CardOrder::Deck, now);
                }
                if ui
                    .selectable_label(session.card_order() == CardOrder::Shuffled, "Shuffled")
                    .clicked()
                {
                    session.set_order(CardOrder::Shuffled, now);
                }
                if session.card_order() == CardOrder::Shuffled && ui.button("Reshuffle").clicked() {
                    session.reshuffle(now);
                }
                ui.separator();
                ui.label("Show:");
                if ui
                    .selectable_label(session.filter() == CardFilter::All, "All")
                    .clicked()
                {
                    session.set_filter(CardFilter::All, now);
                }
                if ui
                    .selectable_label(session.filter() == CardFilter::Unseen, "Unseen")
                    .clicked()
                {
                    session.set_filter(CardFilter::Unseen, now);
                }
                let later_label = format!("Study later ({})", session.later_count());
                if ui
                    .selectable_label(session.filter() == CardFilter::Later, later_label)
                    .clicked()
                {
                    session.set_filter(CardFilter::Later, now);
                }
            });
        });
    }

    fn bottom_bar(&mut self, ctx: &egui::Context) {
        let current = self.session.as_ref().and_then(|session| {
            let card = session.current_card()?;
            let (n, total) = session.view_position()?;
            Some((session.is_later(&card.id), session.revealed(), n, total))
        });
        egui::TopBottomPanel::bottom("bottom").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.add_enabled_ui(current.is_some(), |ui| {
                    let label = if current.is_some_and(|(_, revealed, _, _)| revealed) {
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
                    let flagged = current.is_some_and(|(flagged, _, _, _)| flagged);
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
                if let Some((_, _, n, total)) = current {
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
                    let Some(session) = &self.session else {
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
                        return;
                    };
                    match session.filter() {
                        CardFilter::Unseen => {
                            ui.heading(format!("You've seen all {} cards", session.total()));
                            ui.add_space(16.0);
                            if ui
                                .add(egui::Button::new(
                                    egui::RichText::new("Reset progress").size(18.0),
                                ))
                                .clicked()
                            {
                                self.show_reset_confirm = true;
                            }
                        }
                        CardFilter::Later => {
                            ui.heading("Nothing flagged yet");
                            ui.add_space(8.0);
                            ui.label("Press L on a card to add it to this list.");
                        }
                        CardFilter::All => {
                            ui.heading("This deck has no cards to show");
                        }
                    }
                });
                return;
            };

            let (card, _, _) = current;
            let session = self.session.as_ref().expect("session for current card");
            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.add_space(24.0);
                ui.vertical_centered(|ui| {
                    if session.is_later(&card.id) {
                        ui.small("STUDY LATER");
                        ui.add_space(8.0);
                    }
                    ui.add(egui::Label::new(egui::RichText::new(&card.front).size(30.0)).wrap());
                    if session.revealed() {
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

    /// Confirmation dialog for clearing a deck's seen marks.
    fn reset_confirm(&mut self, ctx: &egui::Context) {
        if !self.show_reset_confirm {
            return;
        }
        let response = egui::Modal::new(egui::Id::new("reset_progress")).show(ctx, |ui| {
            ui.heading("Reset progress?");
            ui.add_space(8.0);
            ui.label(
                "This clears which cards you have seen in this deck. \
                 Study-later flags are kept.",
            );
            ui.add_space(16.0);
            let mut choice = None;
            ui.horizontal(|ui| {
                if ui.button("Cancel").clicked() {
                    choice = Some(false);
                }
                if ui.button("Reset progress").clicked() {
                    choice = Some(true);
                }
            });
            choice
        });
        match response.inner {
            Some(true) => {
                if let Some(session) = &mut self.session {
                    session.reset(progress::now_ms());
                }
                self.show_reset_confirm = false;
            }
            Some(false) => {
                self.show_reset_confirm = false;
            }
            None => {
                if response.backdrop_response.clicked() {
                    self.show_reset_confirm = false;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deck::{Card, Deck};
    use crate::progress::DeckState;

    struct TempDir(std::path::PathBuf);

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn test_app(test_name: &str, later: &[&str]) -> (RunkiApp, TempDir) {
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
        let session = StudySession::new(
            Deck {
                name: "test".to_string(),
                path: deck_path,
                cards,
            },
            state,
            state_path,
            false,
            1_000,
        );
        let app = RunkiApp {
            session: Some(session),
            last_save_attempt: None,
            show_reset_confirm: false,
            error: None,
            save_error: None,
        };
        (app, guard)
    }

    fn session(app: &RunkiApp) -> &StudySession {
        app.session.as_ref().expect("session")
    }

    #[test]
    fn later_count_ignores_stale_ids() {
        let (app, _temp) = test_app("later-count-stale", &["a", "stale-1", "stale-2"]);

        assert_eq!(session(&app).later_count(), 1);
        // Stale ids are not deleted from the in-memory state (or file).
        assert_eq!(session(&app).state().later.len(), 3);
        assert!(session(&app).state().is_later("stale-1"));
    }

    #[test]
    fn error_cleared_after_successful_save() {
        let (mut app, _temp) = test_app("error-cleared-on-save", &[]);
        // Make the first save fail by occupying the sidecar path with a dir.
        let deck_path = session(&app).deck().path.clone();
        let sidecar = progress::state_path(&deck_path, None);
        std::fs::create_dir_all(&sidecar).expect("create blocking dir");

        app.toggle_later();
        app.save_now();
        assert!(app.save_error.is_some(), "failed save should set error");
        assert!(session(&app).dirty(), "failed save stays dirty");

        std::fs::remove_dir_all(&sidecar).expect("remove blocking dir");
        app.toggle_later();
        app.save_now();
        assert_eq!(app.save_error, None, "successful save should clear error");
        assert!(!session(&app).dirty());
    }

    #[test]
    fn successful_save_keeps_unrelated_error() {
        let (mut app, _temp) = test_app("save-keeps-error", &[]);
        app.error = Some("Could not open other.apkg".to_string());

        app.toggle_later();
        app.save_now();

        assert!(!session(&app).dirty());
        assert_eq!(app.error.as_deref(), Some("Could not open other.apkg"));
    }

    #[test]
    fn failed_save_on_deck_switch_is_reported() {
        let (mut app, temp) = test_app("switch-save-fails", &[]);
        let deck_path = session(&app).deck().path.clone();
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
    fn debounced_save_writes_file() {
        let (mut app, _temp) = test_app("debounced-save", &[]);
        let state_path = session(&app).state_path().clone();
        assert!(!state_path.exists());

        // First save attempt is immediate (nothing attempted yet).
        let now = Instant::now();
        app.toggle_later();
        assert_eq!(app.maybe_save_debounced_at(now), None);
        assert!(state_path.exists(), "debounced save writes the file");
        assert!(!session(&app).dirty());

        // A new change inside the interval waits, then saves once due.
        app.toggle_later();
        let delay = app
            .maybe_save_debounced_at(now + Duration::from_secs(1))
            .expect("pending save should schedule a retry");
        assert_eq!(delay, Duration::from_secs(1));
        assert!(session(&app).dirty());
        app.maybe_save_debounced_at(now + Duration::from_secs(2));
        assert!(!session(&app).dirty());
        let loaded = progress::load(&session(&app).deck().path, &state_path);
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
        assert!(
            session(&app).state().opened_at > 0,
            "opening stamps opened_at"
        );
        app.space();
        app.open_path(second);

        let first_state = progress::state_path(&first, None);
        assert!(first_state.exists(), "switching saved the first deck");
        let loaded = progress::load(&first, &first_state);
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
        assert!(app.session.is_some(), "deck still opens");
        assert!(session(&app).save_blocked());
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
            session(&app).state().later.len(),
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
        assert!(session(&app).save_blocked());

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
        assert!(session(&app).save_blocked());

        app.open_path(good_path.clone());
        assert!(!session(&app).save_blocked());
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
