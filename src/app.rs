//! egui front-end: a library screen, then one card at a time.
//!
//! - The start screen is the library: the decks in one folder
//!   (see [`crate::library`]). Clicking a deck opens the study
//!   screen; **Library** in its top bar goes back.
//! - `Space` shows the answer, then moves to the next card (wrapping
//!   around at the end, so browsing never stops).
//! - `L` toggles the current card in the study-later list.
//! - The study top bar picks the card order (deck order or shuffled)
//!   and which cards to show (all, unseen only, or study-later only).
//!
//! Study logic (order, filter, position, saving) lives in
//! [`crate::session::StudySession`]; this module keeps the config,
//! save debounce, error banners, file pickers, and drag-and-drop.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use eframe::egui;
use serde::{Deserialize, Serialize};

use crate::deck::{self, Card};
use crate::library::{self, DeckEntry, DeckStatus, LibraryLoader};
use crate::progress::{self, CardFilter, CardOrder, LoadOutcome};
use crate::session::StudySession;

/// How often a dirty state is written while studying.
const SAVE_INTERVAL: Duration = Duration::from_secs(2);

/// Storage key for [`Config`] in eframe's persistent storage.
pub const CONFIG_KEY: &str = "runki_config";

/// Per-machine app config (not synced): which folder is the library
/// and which deck was opened last. Stored via eframe persistence
/// (loaded from `cc.storage` in `main`, saved in [`eframe::App::save`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    /// The library folder, if one has been chosen.
    #[serde(default)]
    pub library: Option<PathBuf>,
    /// The deck most recently opened, from anywhere.
    #[serde(default)]
    pub last_open_deck: Option<PathBuf>,
}

/// Reveal a not-downloaded deck in Finder (`open -R`), pointing at
/// the real file if it exists and at its iCloud placeholder
/// otherwise, so the user can download it there. Best-effort: a
/// failed spawn is ignored. macOS only; other OSes hide the button.
#[cfg(target_os = "macos")]
fn show_in_finder(real_path: &std::path::Path) {
    let _ = std::process::Command::new("open")
        .arg("-R")
        .arg(library::reveal_path(real_path))
        .spawn();
}

/// Which screen is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Library,
    Study,
}

pub struct RunkiApp {
    config: Config,
    screen: Screen,
    /// Rows from the latest library scan, with loader summaries
    /// applied as they arrive.
    library_entries: Vec<DeckEntry>,
    /// Why the latest scan failed (missing/unreadable folder).
    library_error: Option<String>,
    /// Created on the first frame, when an egui context is at hand
    /// for its repaint callback.
    loader: Option<LibraryLoader>,
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
    /// Start on the library screen, unless `initial_deck` (a CLI
    /// argument) opens straight into the study screen. The last
    /// open deck is *not* auto-reopened: the library is the start
    /// screen; `last_open_deck` is only remembered in the config.
    /// A CLI argument is the only thing that wins over it, by
    /// construction.
    pub fn new(config: Config, initial_deck: Option<PathBuf>) -> Self {
        let mut app = Self {
            config,
            screen: Screen::Library,
            library_entries: Vec::new(),
            library_error: None,
            loader: None,
            session: None,
            last_save_attempt: None,
            show_reset_confirm: false,
            error: None,
            save_error: None,
        };
        app.rescan_library();
        if let Some(path) = initial_deck {
            app.open_path(path);
        }
        app
    }

    /// The current config (what `App::save` persists).
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// The screen currently showing.
    pub fn screen(&self) -> Screen {
        self.screen
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
                // A deck in the library uses the library state file
                // however it was opened; others use a sidecar.
                let state_path = progress::state_path(&deck.path, self.config.library.as_deref());
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
                self.config.last_open_deck = Some(deck.path.clone());
                self.session = Some(StudySession::new(
                    deck,
                    state,
                    state_path,
                    save_blocked,
                    progress::now_ms(),
                ));
                self.show_reset_confirm = false;
                self.screen = Screen::Study;
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

    /// Re-scan the library folder; rows appear at once and the
    /// background loader fills in their counts.
    fn rescan_library(&mut self) {
        let Some(dir) = self.config.library.clone() else {
            self.library_entries.clear();
            self.library_error = None;
            return;
        };
        match library::scan(&dir) {
            Ok(entries) => {
                self.library_entries = entries;
                self.library_error = None;
                self.start_loader();
            }
            Err(err) => {
                self.library_entries.clear();
                self.library_error = Some(format!("{err:#}"));
            }
        }
    }

    /// (Re)start the background loader for the current rows, once
    /// it exists (it is created on the first frame).
    fn start_loader(&self) {
        if let (Some(loader), Some(dir)) = (&self.loader, &self.config.library) {
            // Not-downloaded rows are never sent to the loader:
            // reading one would block on a cloud download.
            loader.start(dir, library::loadable_paths(&self.library_entries));
        }
    }

    /// Create the loader on the first frame, wiring its repaint
    /// callback to this window, and start it for the scanned rows.
    fn ensure_loader(&mut self, ctx: &egui::Context) {
        if self.loader.is_none() {
            let ctx = ctx.clone();
            self.loader = Some(LibraryLoader::new(Box::new(move || {
                ctx.request_repaint();
            })));
            self.start_loader();
        }
    }

    /// Fold arrived background summaries into the rows.
    fn apply_summaries(&mut self) {
        let Some(loader) = &self.loader else { return };
        let summaries = loader.drain();
        if summaries.is_empty() {
            return;
        }
        for summary in summaries {
            if let Some(entry) = self
                .library_entries
                .iter_mut()
                .find(|entry| entry.path == summary.path)
            {
                entry.status = summary.status;
                entry.opened_at = summary.opened_at;
            }
        }
        library::sort_entries(&mut self.library_entries);
    }

    /// Point the library at `dir` and show it.
    fn set_library(&mut self, dir: PathBuf) {
        self.config.library = Some(dir);
        self.screen = Screen::Library;
        self.rescan_library();
    }

    fn choose_library_folder(&mut self) {
        if let Some(dir) = rfd::FileDialog::new().pick_folder() {
            self.set_library(dir);
        }
    }

    /// Use one of the suggested `…/Runki` folders, creating it first.
    fn choose_suggested_library(&mut self, dir: PathBuf) {
        match std::fs::create_dir_all(&dir) {
            Ok(()) => self.set_library(dir),
            Err(err) => {
                self.error = Some(format!("Could not create {}: {err}", dir.display()));
            }
        }
    }

    /// "Add deck…": copy picked files into the library.
    fn add_decks_via_picker(&mut self) {
        if let Some(paths) = rfd::FileDialog::new()
            .add_filter("Decks", &["apkg", "csv", "tsv", "txt"])
            .pick_files()
        {
            self.add_to_library(paths);
        }
    }

    /// Copy each file into the library (carrying progress over),
    /// report any failures, then rescan.
    fn add_to_library(&mut self, paths: Vec<PathBuf>) {
        let Some(dir) = self.config.library.clone() else {
            return;
        };
        let errors: Vec<String> = paths
            .iter()
            .filter_map(|path| {
                library::add_deck(&dir, path)
                    .err()
                    .map(|err| format!("{err:#}"))
            })
            .collect();
        if !errors.is_empty() {
            self.error = Some(errors.join("\n"));
        }
        self.rescan_library();
    }

    /// Back to the library: save first (reporting failure like a
    /// deck switch does, since the session is dropped), then rescan
    /// so the rows show the progress just made.
    fn go_to_library(&mut self) {
        let outgoing_name = self.session.as_ref().map(|s| s.deck().name.clone());
        self.save_now();
        if let Some(err) = self.save_error.take() {
            let name = outgoing_name.as_deref().unwrap_or("the previous deck");
            self.error = Some(format!("Progress for {name} could not be saved: {err}"));
        }
        self.session = None;
        self.show_reset_confirm = false;
        self.screen = Screen::Library;
        self.rescan_library();
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
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        self.save_now();
        eframe::set_value(storage, CONFIG_KEY, &self.config);
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.save_now();
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.ensure_loader(ctx);
        self.apply_summaries();

        // Drag-and-drop: onto the library screen copies files in;
        // onto the study screen opens the first one (as before).
        // Without a library folder there is nothing to copy into,
        // so opening is the fallback there too.
        let dropped: Vec<PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .filter_map(|f| f.path.clone())
                .collect()
        });
        if !dropped.is_empty() {
            if self.screen == Screen::Library && self.config.library.is_some() {
                self.add_to_library(dropped);
            } else if let Some(path) = dropped.into_iter().next() {
                self.open_path(path);
            }
        }

        if self.screen == Screen::Study {
            // Consume Space/L before widgets see them, so a focused
            // button cannot also trigger on Space and double-advance.
            // While the reset confirmation is open they are consumed
            // but ignored.
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
        }

        // Debounced save, with a repaint scheduled so a pending save
        // happens even without further input.
        if let Some(delay) = self.maybe_save_debounced_at(Instant::now()) {
            ctx.request_repaint_after(delay);
        }

        match self.screen {
            Screen::Library => {
                self.library_top_bar(ctx);
                self.library_central(ctx);
            }
            Screen::Study => {
                self.top_bar(ctx);
                // The Library button may have just switched screens;
                // only draw the rest of the study screen if not.
                if self.screen == Screen::Study {
                    self.bottom_bar(ctx);
                    self.central(ctx);
                    self.reset_confirm(ctx);
                } else {
                    self.library_central(ctx);
                }
            }
        }
    }
}

impl RunkiApp {
    fn top_bar(&mut self, ctx: &egui::Context) {
        let mut go_back = false;
        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.horizontal_wrapped(|ui| {
                if ui.button("Library").clicked() {
                    go_back = true;
                }
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
        if go_back {
            self.go_to_library();
        }
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

    fn library_top_bar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("library_top").show(ctx, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.strong("Library");
                match &self.config.library {
                    Some(dir) => {
                        ui.label(dir.display().to_string());
                        if ui.button("Change folder…").clicked() {
                            self.choose_library_folder();
                        }
                        if ui.button("Add deck…").clicked() {
                            self.add_decks_via_picker();
                        }
                        if ui.button("Refresh").clicked() {
                            self.rescan_library();
                        }
                    }
                    None => {
                        ui.label("No library folder chosen");
                    }
                }
                ui.separator();
                if ui.button("Open deck…").clicked() {
                    self.pick_and_open();
                }
            });
        });
    }

    fn library_central(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default().show(ctx, |ui| {
            self.banners(ui);
            if self.config.library.is_none() {
                self.welcome(ui);
                return;
            }
            if let Some(error) = self.library_error.clone() {
                ui.add_space(16.0);
                ui.colored_label(egui::Color32::LIGHT_RED, error);
                ui.add_space(16.0);
                if ui.button("Choose another folder…").clicked() {
                    self.choose_library_folder();
                }
                return;
            }
            self.library_list(ui);
        });
    }

    /// First-launch view: pick a library folder (with synced-folder
    /// shortcuts), or just open a one-off deck.
    fn welcome(&mut self, ui: &mut egui::Ui) {
        ui.vertical_centered(|ui| {
            ui.add_space(80.0);
            ui.heading("Runki");
            ui.add_space(8.0);
            ui.label(
                "Choose a library folder for your decks. Decks and progress \
                 live there, so a folder synced by Dropbox or iCloud Drive \
                 follows you between Macs.",
            );
            ui.add_space(16.0);
            if ui
                .add(egui::Button::new(
                    egui::RichText::new("Choose a library folder…").size(20.0),
                ))
                .clicked()
            {
                self.choose_library_folder();
            }
            for dir in library::suggested_library_folders() {
                ui.add_space(8.0);
                if ui
                    .button(format!("Use {}", dir.display()))
                    .on_hover_text("Create this folder and use it as the library")
                    .clicked()
                {
                    self.choose_suggested_library(dir);
                }
            }
            ui.add_space(24.0);
            ui.label("Or open a deck file without a library:");
            ui.add_space(8.0);
            if ui.button("Open deck…").clicked() {
                self.pick_and_open();
            }
        });
    }

    /// The scrollable deck list. Clicking a ready row opens it.
    fn library_list(&mut self, ui: &mut egui::Ui) {
        if self.library_entries.is_empty() {
            ui.add_space(16.0);
            ui.label("No decks in this folder yet. Use “Add deck…”, or drop deck files here.");
            return;
        }
        let now = progress::now_ms();
        let mut to_open: Option<PathBuf> = None;
        egui::ScrollArea::vertical().show(ui, |ui| {
            for entry in &self.library_entries {
                let ready = matches!(entry.status, DeckStatus::Ready { .. });
                let row = ui.horizontal_wrapped(|ui| {
                    if entry.status == DeckStatus::NotDownloaded {
                        ui.weak(&entry.display_name);
                    } else {
                        ui.strong(&entry.display_name);
                    }
                    match &entry.status {
                        DeckStatus::Loading => {
                            ui.spinner();
                            ui.label("Loading…");
                        }
                        DeckStatus::Ready { total, seen, later } => {
                            let fraction = if *total == 0 {
                                0.0
                            } else {
                                *seen as f32 / *total as f32
                            };
                            ui.add(
                                egui::ProgressBar::new(fraction)
                                    .desired_width(140.0)
                                    .text(format!("{seen} / {total}")),
                            );
                            ui.label(format!("{total} cards"));
                            ui.label(format!("{later} for later"));
                            ui.label(format!(
                                "Last opened: {}",
                                library::last_opened_label(entry.opened_at, now)
                            ));
                        }
                        DeckStatus::Failed(error) => {
                            ui.colored_label(egui::Color32::LIGHT_RED, error);
                        }
                        DeckStatus::NotDownloaded => {
                            ui.weak("Not downloaded – open in Finder to download");
                            #[cfg(target_os = "macos")]
                            if ui.small_button("Show in Finder").clicked() {
                                show_in_finder(&entry.path);
                            }
                        }
                    }
                });
                let response = row.response.interact(egui::Sense::click());
                if ready {
                    let response = response.on_hover_cursor(egui::CursorIcon::PointingHand);
                    if response.clicked() {
                        to_open = Some(entry.path.clone());
                    }
                }
                ui.separator();
            }
        });
        if let Some(path) = to_open {
            self.open_path(path);
        }
    }

    /// Dismissable red banners for `error` and `save_error`,
    /// shared by both screens.
    fn banners(&mut self, ui: &mut egui::Ui) {
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
    }

    fn central(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default().show(ctx, |ui| {
            self.banners(ui);

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
            config: Config::default(),
            screen: Screen::Study,
            library_entries: Vec::new(),
            library_error: None,
            loader: None,
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

        let mut app = RunkiApp::new(Config::default(), Some(first.clone()));
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

        let mut app = RunkiApp::new(Config::default(), Some(deck_path));
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

        let mut app = RunkiApp::new(Config::default(), Some(deck_path));
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

        let mut app = RunkiApp::new(Config::default(), Some(bad_path));
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

    #[test]
    fn library_deck_opened_by_path_uses_runki_state() {
        let dir = std::env::temp_dir().join(format!(
            "runki-test-library-state-path-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let library = dir.join("library");
        std::fs::create_dir_all(&library).expect("create library");
        let deck_path = library.join("deck.tsv");
        std::fs::write(&deck_path, "Q1\tA1\n").expect("write deck");

        let config = Config {
            library: Some(library.clone()),
            last_open_deck: None,
        };
        // Opened by path (as a CLI deck would be), not via a row click.
        let mut app = RunkiApp::new(config, Some(deck_path.clone()));
        assert_eq!(app.screen(), Screen::Study);
        assert_eq!(app.config().last_open_deck.as_ref(), Some(&deck_path));

        let library_state = library.join(".runki").join("deck.tsv.json");
        assert_eq!(session(&app).state_path(), &library_state);

        app.toggle_later();
        app.save_now();
        assert!(library_state.exists(), "progress saved under .runki/");
        assert!(
            !progress::state_path(&deck_path, None).exists(),
            "no sidecar for a library deck"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn starts_on_library_and_goes_back_with_rescan() {
        let dir =
            std::env::temp_dir().join(format!("runki-test-start-screen-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let library = dir.join("library");
        std::fs::create_dir_all(&library).expect("create library");
        let deck_path = library.join("deck.tsv");
        std::fs::write(&deck_path, "Q1\tA1\n").expect("write deck");

        let config = Config {
            library: Some(library.clone()),
            last_open_deck: Some(deck_path.clone()),
        };
        // Even with a remembered deck, the start screen is the
        // library (only a CLI deck auto-opens), already scanned.
        let mut app = RunkiApp::new(config, None);
        assert_eq!(app.screen(), Screen::Library);
        assert!(app.session.is_none());
        assert_eq!(app.library_entries.len(), 1);
        assert_eq!(app.library_entries[0].display_name, "deck");
        assert!(app.library_error.is_none());

        app.open_path(deck_path.clone());
        assert_eq!(app.screen(), Screen::Study);
        app.space(); // Mark the only card seen.

        app.go_to_library();
        assert_eq!(app.screen(), Screen::Library);
        assert!(app.session.is_none());
        assert_eq!(app.library_entries.len(), 1, "rescanned on the way back");
        assert_eq!(
            app.library_entries[0].opened_at, 0,
            "scan no longer reads state; the loader supplies opened_at"
        );
        let saved = progress::load(
            &deck_path,
            &progress::state_path(&deck_path, Some(&library)),
        );
        assert!(
            saved.state().opened_at > 0,
            "the saved opened_at is on disk for the loader to report"
        );
        assert!(
            saved.state().seen.values().any(|&t| t > 0),
            "going back saved the session"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_library_folder_is_shown_as_error() {
        let dir =
            std::env::temp_dir().join(format!("runki-test-missing-library-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let config = Config {
            library: Some(dir),
            last_open_deck: None,
        };
        let app = RunkiApp::new(config, None);
        assert_eq!(app.screen(), Screen::Library);
        assert!(
            app.library_error
                .as_deref()
                .is_some_and(|e| e.contains("Could not read library folder")),
            "scan failure is kept for the library view: {:?}",
            app.library_error
        );
        assert!(app.library_entries.is_empty());
    }
}
