//! egui front-end: one card at a time.
//!
//! - `Space` shows the answer, then moves to the next card (wrapping
//!   around at the end, so browsing never stops).
//! - `L` toggles the current card in the study-later list.
//! - The top bar switches between all cards and study-later only.

use std::collections::HashSet;
use std::path::PathBuf;

use eframe::egui;

use crate::deck::{self, Deck};

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
    later: HashSet<String>,
    /// Set when the study-later file could not be read: toggles still
    /// update `later` in memory but must not overwrite the file.
    later_save_blocked: bool,
    error: Option<String>,
}

impl RunkiApp {
    pub fn new(initial_deck: Option<PathBuf>) -> Self {
        let mut app = Self {
            deck: None,
            pos: 0,
            revealed: false,
            mode: Mode::All,
            later: HashSet::new(),
            later_save_blocked: false,
            error: None,
        };
        if let Some(path) = initial_deck {
            app.open_path(path);
        }
        app
    }

    fn open_path(&mut self, path: PathBuf) {
        match deck::load_deck(&path) {
            Ok(deck) => {
                match deck::load_later(&deck.path) {
                    Ok(later) => {
                        self.later = later;
                        self.later_save_blocked = false;
                        self.error = None;
                    }
                    Err(err) => {
                        self.later = HashSet::new();
                        self.later_save_blocked = true;
                        self.error = Some(format!(
                            "{err:#}. The study-later list could not be read, so \
                             flags in this session won't be saved until the file \
                             is fixed or removed."
                        ));
                    }
                }
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

    fn pick_and_open(&mut self) {
        if let Some(path) = rfd::FileDialog::new()
            .add_filter("Decks", &["apkg", "csv", "tsv", "txt"])
            .pick_file()
        {
            self.open_path(path);
        }
    }

    /// Indices of the cards visible in the current mode.
    fn view(&self) -> Vec<usize> {
        let Some(deck) = &self.deck else {
            return Vec::new();
        };
        match self.mode {
            Mode::All => (0..deck.cards.len()).collect(),
            Mode::Later => deck
                .cards
                .iter()
                .enumerate()
                .filter(|(_, card)| self.later.contains(&card.id))
                .map(|(i, _)| i)
                .collect(),
        }
    }

    fn current(&self) -> Option<(deck::Card, usize, usize)> {
        let deck = self.deck.as_ref()?;
        let view = self.view();
        let idx = *view.get(self.pos)?;
        Some((deck.cards[idx].clone(), self.pos + 1, view.len()))
    }

    /// Space: reveal the answer, or advance (wrapping) if already shown.
    fn space(&mut self) {
        let view_len = self.view().len();
        if view_len == 0 {
            return;
        }
        if self.revealed {
            self.pos = (self.pos + 1) % view_len;
            self.revealed = false;
        } else {
            self.revealed = true;
        }
    }

    /// L: toggle the current card in the study-later list and save.
    fn toggle_later(&mut self) {
        let Some((card, _, _)) = self.current() else {
            return;
        };
        if !self.later.insert(card.id.clone()) {
            self.later.remove(&card.id);
        }
        if !self.later_save_blocked {
            if let Some(deck) = &self.deck {
                if let Err(err) = deck::save_later(&deck.path, &self.later) {
                    self.error = Some(format!("{err:#}"));
                }
            }
        }
        // Unflagging while reviewing the later list shrinks the view;
        // keep the position pointing at the next remaining card.
        let view_len = self.view().len();
        if view_len == 0 {
            self.pos = 0;
        } else if self.pos >= view_len {
            self.pos = view_len - 1;
        }
        // If a different card is now shown, hide its answer.
        if self.current().map(|(c, _, _)| c.id) != Some(card.id) {
            self.revealed = false;
        }
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
                    let later_label = format!("Study later ({})", self.later.len());
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
        let current = self.current();
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
                    let flagged = current
                        .as_ref()
                        .is_some_and(|(card, _, _)| self.later.contains(&card.id));
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
                if let Some((_, n, total)) = &current {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(format!("{n} / {total}"));
                    });
                }
            });
        });
    }

    fn central(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default().show(ctx, |ui| {
            if let Some(error) = self.error.clone() {
                ui.add_space(16.0);
                ui.colored_label(egui::Color32::LIGHT_RED, error);
                ui.add_space(16.0);
            }

            let Some(current) = self.current() else {
                ui.vertical_centered(|ui| {
                    ui.add_space(80.0);
                    if self.deck.is_none() {
                        ui.heading("Runki");
                        ui.add_space(8.0);
                        ui.label("Open an .apkg, .csv, or .tsv deck, or drop the file here.");
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
                    if self.later.contains(&card.id) {
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

    fn test_app(
        test_name: &str,
        later: &[&str],
        mode: Mode,
        pos: usize,
        revealed: bool,
    ) -> RunkiApp {
        let dir =
            std::env::temp_dir().join(format!("runki-test-{}-{}", test_name, std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let cards = ["a", "b", "c"]
            .into_iter()
            .map(|id| Card {
                id: id.to_string(),
                front: format!("front {id}"),
                back: format!("back {id}"),
            })
            .collect();
        RunkiApp {
            deck: Some(Deck {
                name: "test".to_string(),
                path: dir.join("deck.csv"),
                cards,
            }),
            pos,
            revealed,
            mode,
            later: later.iter().map(|id| (*id).to_string()).collect(),
            later_save_blocked: false,
            error: None,
        }
    }

    #[test]
    fn later_unflag_first_hides_next_cards_answer() {
        let mut app = test_app("later-unflag-first", &["a", "b", "c"], Mode::Later, 0, true);

        app.toggle_later();

        assert_eq!(app.pos, 0);
        assert!(!app.revealed);
        let (card, _, _) = app.current().expect("next card shown");
        assert_eq!(card.id, "b");
    }

    #[test]
    fn later_unflag_middle_hides_next_cards_answer() {
        let mut app = test_app(
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
        let mut app = test_app("later-unflag-last", &["a"], Mode::Later, 0, true);

        app.toggle_later();

        assert_eq!(app.pos, 0);
        assert!(!app.revealed);
        assert!(app.current().is_none());
        assert!(app.view().is_empty());
    }

    #[test]
    fn all_toggle_flag_keeps_answer_shown_on_same_card() {
        let mut app = test_app("all-toggle-flag", &[], Mode::All, 0, true);

        app.toggle_later();

        assert!(app.revealed);
        assert!(app.later.contains("a"));
        let (card, _, _) = app.current().expect("card shown");
        assert_eq!(card.id, "a");

        app.toggle_later();

        assert!(app.revealed);
        assert!(!app.later.contains("a"));
        let (card, _, _) = app.current().expect("card shown");
        assert_eq!(card.id, "a");
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
        let sidecar = crate::deck::later_path(&deck_path);
        std::fs::write(&sidecar, corrupt).expect("write corrupt sidecar");

        let mut app = RunkiApp::new(Some(deck_path));
        assert!(app.deck.is_some(), "deck still opens");
        assert!(app.later_save_blocked);
        assert!(
            app.error
                .as_deref()
                .is_some_and(|e| e.contains("could not be read")),
            "error should explain saving is blocked: {:?}",
            app.error
        );

        app.toggle_later();

        assert_eq!(app.later.len(), 1, "toggle still updates in-memory list");
        assert_eq!(
            std::fs::read_to_string(&sidecar).expect("read sidecar"),
            corrupt,
            "corrupt file must be unchanged on disk"
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
        std::fs::write(crate::deck::later_path(&bad_path), "not valid json {{{")
            .expect("write corrupt sidecar");
        let good_path = dir.join("good.tsv");
        std::fs::write(&good_path, "Q1\tA1\n").expect("write good deck");

        let mut app = RunkiApp::new(Some(bad_path));
        assert!(app.later_save_blocked);

        app.open_path(good_path.clone());
        assert!(!app.later_save_blocked);
        assert!(app.error.is_none());

        app.toggle_later();
        assert!(
            crate::deck::later_path(&good_path).exists(),
            "toggle saves after the flag is reset"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
