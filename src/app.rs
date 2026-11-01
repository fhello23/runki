//! egui front-end: a library screen, then one card at a time.
//!
//! - The start screen is the library: the decks in one folder
//!   (see [`crate::library`]). Clicking a deck opens the study
//!   screen; **Library** in its top bar goes back.
//! - `Space` (or a click on the card) shows the answer, then moves to
//!   the next card (wrapping around at the end, so browsing never stops).
//! - `L` toggles the current card in the study-later list.
//! - The study top bar picks the card order (deck order or shuffled)
//!   and which cards to show (all, unseen only, or study-later only).
//!
//! Study logic (order, filter, position, saving) lives in
//! [`crate::session::StudySession`]; this module keeps the config,
//! save debounce, error banners, file pickers, and drag-and-drop.

use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use eframe::egui;

pub use crate::config::Config;

use crate::deck::{self, Card};
use crate::library::{self, DeckEntry, DeckStatus, LibraryLoader};
use crate::progress::{self, CardFilter, CardOrder, LoadOutcome};
use crate::session::StudySession;
use crate::theme::{self, Palette};
use crate::web::{LibrarySlot, WebOptions, WebServer, WARNING};
use crate::web_menu::{WebMenuBridge, WebMenuStatus};
use crate::VERSION;

/// How often a dirty state is written while studying.
const SAVE_INTERVAL: Duration = Duration::from_secs(2);

/// Storage key for [`Config`] in eframe's persistent storage.
pub const CONFIG_KEY: &str = "runki_config";

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
    library_slot: LibrarySlot,
    web: Option<WebServer>,
    web_port: u16,
    web_port_input: String,
    web_bridge: Arc<WebMenuBridge>,
    web_error: Option<String>,
    config: Config,
    config_path: Option<PathBuf>,
    theme: egui::ThemePreference,
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

fn parse_web_port(input: &str) -> Option<u16> {
    if input.is_empty() || !input.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    input.parse::<u16>().ok().filter(|&port| port != 0)
}

fn web_port_field_id() -> egui::Id {
    egui::Id::new("web_port")
}

impl RunkiApp {
    /// Start on the library screen, unless `initial_deck` (a CLI
    /// argument) opens straight into the study screen. The last
    /// open deck is *not* auto-reopened: the library is the start
    /// screen; `last_open_deck` is only remembered in the config.
    /// A CLI argument is the only thing that wins over it, by
    /// construction.
    pub fn new(config: Config, initial_deck: Option<PathBuf>) -> Self {
        Self::new_with_web_options(config, initial_deck, WebOptions::default())
    }

    pub fn new_with_web_options(
        config: Config,
        initial_deck: Option<PathBuf>,
        options: WebOptions,
    ) -> Self {
        let web_on = options.on.unwrap_or(config.web_on);
        let web_port = options.port.unwrap_or(config.web_port);
        let mut app = Self {
            library_slot: Arc::new(RwLock::new(config.library.clone())),
            web: None,
            web_port,
            web_port_input: web_port.to_string(),
            web_bridge: Arc::new(WebMenuBridge::default()),
            web_error: None,
            config,
            config_path: None,
            theme: egui::ThemePreference::System,
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
        if web_on {
            app.start_web();
        }
        app.publish_web_status();
        app.rescan_library();
        if let Some(path) = initial_deck {
            app.open_path(path);
        }
        app
    }

    fn start_web(&mut self) {
        match WebServer::start(Arc::clone(&self.library_slot), self.web_port) {
            Ok(server) => {
                self.web = Some(server);
                self.web_error = None;
            }
            Err(err) => self.web_error = Some(format!("{err:#}")),
        }
    }

    fn toggle_web(&mut self) {
        if let Some(mut server) = self.web.take() {
            self.web_error = server.stop().err().map(|err| format!("{err:#}"));
        } else {
            self.start_web();
        }
        self.config.web_on = self.web.is_some();
        self.publish_web_status();
    }

    pub fn with_web_bridge(mut self, bridge: Arc<WebMenuBridge>) -> Self {
        self.web_bridge = bridge;
        self.publish_web_status();
        self
    }

    fn publish_web_status(&self) {
        self.web_bridge.publish(WebMenuStatus {
            on: self.web.is_some(),
            lan_url: self.web.as_ref().map(|server| server.lan_url().to_owned()),
            port: self.web_port,
        });
    }

    fn handle_web_menu_requests(&mut self) {
        for _ in 0..self.web_bridge.take_toggle_requests() {
            self.toggle_web();
        }
    }

    fn apply_web_port(&mut self) {
        self.apply_web_port_with(WebServer::start);
    }

    fn apply_web_port_with(
        &mut self,
        mut start: impl FnMut(LibrarySlot, u16) -> anyhow::Result<WebServer>,
    ) {
        let Some(port) = parse_web_port(&self.web_port_input).filter(|&port| port != self.web_port)
        else {
            return;
        };
        if let Some(mut previous) = self.web.take() {
            let stop_error = previous
                .stop()
                .err()
                .map(|err| format!("Previous Web progress could not be saved: {err:#}"));
            drop(previous);
            match start(Arc::clone(&self.library_slot), port) {
                Ok(server) => {
                    self.web = Some(server);
                    self.web_port = port;
                    self.config.web_port = port;
                    self.web_error = stop_error;
                }
                Err(err) => {
                    let mut error = format!("Could not use Web port {port}: {err:#}");
                    match start(Arc::clone(&self.library_slot), self.web_port) {
                        Ok(server) => {
                            self.web = Some(server);
                            error.push_str(&format!(". Still serving on port {}.", self.web_port));
                        }
                        Err(rollback) => {
                            self.config.web_on = false;
                            error.push_str(&format!(
                                ". Could not restore port {}: {rollback:#}. Web is off.",
                                self.web_port
                            ));
                        }
                    }
                    if let Some(stop_error) = stop_error {
                        error.push_str(&format!("\n{stop_error}"));
                    }
                    self.web_error = Some(error);
                }
            }
        } else {
            self.web_port = port;
            self.config.web_port = port;
            self.web_error = None;
        }
        self.publish_web_status();
    }

    /// Start with this theme choice (as loaded from the app config).
    pub fn with_theme(mut self, theme: egui::ThemePreference) -> Self {
        self.theme = theme;
        self
    }

    /// Mirror library settings to the file also used by the TUI.
    pub fn with_config_path(mut self, path: PathBuf) -> Self {
        self.config_path = Some(path);
        self
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
        *self.library_slot.write().expect("library slot") = self.config.library.clone();
        self.screen = Screen::Library;
        self.rescan_library();
    }

    fn choose_library_folder(&mut self) {
        // New Folder lets the user make e.g. a `Runki` folder inside
        // iCloud Drive right from the picker.
        if let Some(dir) = rfd::FileDialog::new()
            .set_can_create_directories(true)
            .pick_folder()
        {
            self.set_library(dir);
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

    fn study_shortcuts(&mut self, ctx: &egui::Context) {
        if self.screen == Screen::Study
            && !ctx.memory(|memory| memory.has_focus(web_port_field_id()))
        {
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
        if let Some(path) = &self.config_path {
            if let Err(err) = crate::config::save(path, &self.config) {
                self.error = Some(format!("{err:#}"));
            }
        }
        theme::store(storage, self.theme);
    }

    // Rebuild styles from the installed palettes on each launch.
    fn persist_egui_memory(&self) -> bool {
        false
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.save_now();
        if let Some(mut server) = self.web.take() {
            if let Err(err) = server.stop() {
                eprintln!("runki: Web progress could not be saved: {err:#}");
            }
        }
        self.publish_web_status();
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.handle_web_menu_requests();
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

        self.study_shortcuts(ctx);

        // Debounced save, with a repaint scheduled so a pending save
        // happens even without further input.
        if let Some(delay) = self.maybe_save_debounced_at(Instant::now()) {
            ctx.request_repaint_after(delay);
        }

        let p = Palette::of(ctx);
        match self.screen {
            Screen::Library => {
                self.library_top_bar(ctx, &p);
                self.library_central(ctx, &p);
            }
            Screen::Study => {
                self.top_bar(ctx, &p);
                if self.screen == Screen::Study {
                    self.bottom_bar(ctx, &p);
                    self.central(ctx, &p);
                    self.reset_confirm(ctx);
                } else {
                    self.library_central(ctx, &p);
                }
            }
        }
        let adding = self.screen == Screen::Library && self.config.library.is_some();
        drop_overlay(ctx, &p, adding);
    }
}

/// Keep long card lines readable while allowing small windows.
const CARD_MAX_WIDTH: f32 = 680.0;
const CARD_MIN_HEIGHT: f32 = 240.0;
const TITLEBAR_INSET: i8 = if cfg!(target_os = "macos") { 28 } else { 0 };

fn top_frame(p: &Palette) -> egui::Frame {
    egui::Frame::new().fill(p.bg).inner_margin(egui::Margin {
        left: 20,
        right: 16,
        top: TITLEBAR_INSET + 12,
        bottom: 8,
    })
}

fn central_frame(p: &Palette) -> egui::Frame {
    egui::Frame::new()
        .fill(p.bg)
        .inner_margin(egui::Margin::symmetric(24, 8))
}

impl RunkiApp {
    fn theme_button(&mut self, ui: &mut egui::Ui, p: &Palette) {
        if theme_toggle(ui, p, self.theme) {
            self.theme = theme::next(self.theme);
            ui.ctx().set_theme(self.theme);
            ui.ctx().request_discard("theme changed");
        }
    }

    fn web_menu(&mut self, ui: &mut egui::Ui, p: &Palette) {
        let label = if self.web.is_some() {
            "Web: On"
        } else {
            "Web: Off"
        };
        egui::menu::menu_custom_button(
            ui,
            egui::Button::new(label).selected(self.web.is_some()),
            |ui| {
                ui.set_width(310.0);
                let mut on = self.web.is_some();
                if ui.checkbox(&mut on, "Web server on").changed() {
                    self.toggle_web();
                }
                ui.separator();
                ui.horizontal(|ui| {
                    ui.label("Port");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.web_port_input)
                            .id(web_port_field_id())
                            .desired_width(85.0),
                    );
                    let port = parse_web_port(&self.web_port_input);
                    if ui
                        .add_enabled(
                            port.is_some_and(|port| port != self.web_port),
                            egui::Button::new("Apply"),
                        )
                        .clicked()
                    {
                        self.apply_web_port();
                    }
                });
                if parse_web_port(&self.web_port_input).is_none() {
                    ui.label(
                        egui::RichText::new("Enter a port from 1 to 65535 (digits only).")
                            .small()
                            .color(p.danger),
                    );
                }
                if let Some(server) = &self.web {
                    ui.separator();
                    ui.add(egui::Label::new(server.lan_url()).selectable(true));
                    if ui.button("Copy URL").clicked() {
                        ui.ctx().copy_text(server.lan_url().to_owned());
                    }
                    ui.add(egui::Label::new(server.localhost_url()).selectable(true));
                    ui.label(egui::RichText::new(WARNING).small().color(p.muted));
                }
                if let Some(error) = &self.web_error {
                    ui.separator();
                    ui.add(egui::Label::new(egui::RichText::new(error).color(p.danger)).wrap());
                }
            },
        );
    }

    fn top_bar(&mut self, ctx: &egui::Context, p: &Palette) {
        let mut go_back = false;
        egui::TopBottomPanel::top("top")
            .frame(top_frame(p))
            .show_separator_line(false)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        self.theme_button(ui, p);
                        self.web_menu(ui, p);
                        if ui.button("Open deck…").clicked() {
                            self.pick_and_open();
                        }
                        ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                            if ui.button("Library").clicked() {
                                go_back = true;
                            }
                            if let Some(session) = &self.session {
                                ui.add(
                                    egui::Label::new(
                                        egui::RichText::new(&session.deck().name).color(p.text),
                                    )
                                    .truncate(),
                                );
                            }
                        });
                    });
                });
                let Some(session) = &mut self.session else {
                    return;
                };
                let now = progress::now_ms();
                ui.add_space(4.0);
                ui.horizontal_wrapped(|ui| {
                    let later = match session.later_count() {
                        0 => "Later".to_owned(),
                        n => format!("Later ({n})"),
                    };
                    let filters = [
                        (CardFilter::All, "All"),
                        (CardFilter::Unseen, "Unseen"),
                        (CardFilter::Later, later.as_str()),
                    ];
                    if let Some(filter) = segmented(ui, p, session.filter(), &filters) {
                        session.set_filter(filter, now);
                    }
                    let order = session.card_order();
                    let order_picker = egui::ComboBox::from_id_salt("card_order")
                        .selected_text(match order {
                            CardOrder::Deck => "Deck order",
                            CardOrder::Shuffled => "Shuffled",
                        })
                        .width(110.0)
                        .show_ui(ui, |ui| {
                            for (value, label) in [
                                (CardOrder::Deck, "Deck order"),
                                (CardOrder::Shuffled, "Shuffled"),
                            ] {
                                if ui.selectable_label(order == value, label).clicked() {
                                    session.set_order(value, now);
                                }
                            }
                        });
                    order_picker.response.widget_info(|| {
                        egui::WidgetInfo::labeled(
                            egui::WidgetType::ComboBox,
                            ui.is_enabled(),
                            "Card order",
                        )
                    });
                    if session.card_order() == CardOrder::Shuffled
                        && ui.button("Reshuffle").clicked()
                    {
                        session.reshuffle(now);
                    }
                });
            });
        if go_back {
            self.go_to_library();
        }
    }

    fn bottom_bar(&mut self, ctx: &egui::Context, p: &Palette) {
        let current = self.session.as_ref().and_then(|session| {
            let card = session.current_card()?;
            Some((session.is_later(&card.id), session.revealed()))
        });
        egui::TopBottomPanel::bottom("bottom")
            .frame(egui::Frame::new().fill(p.bg).inner_margin(egui::Margin {
                left: 12,
                right: 12,
                top: 6,
                bottom: 18,
            }))
            .show_separator_line(false)
            .show(ctx, |ui| {
                let Some((flagged, revealed)) = current else {
                    ui.add_space(30.0);
                    return;
                };
                centered_row(ui, |ui| {
                    let label = if revealed { "Next" } else { "Show answer" };
                    if key_hint(ui, p, "Space", label, p.muted).clicked() {
                        self.space();
                    }
                    let (label, color) = if flagged {
                        ("Saved for later", p.accent)
                    } else {
                        ("Study later", p.muted)
                    };
                    if key_hint(ui, p, "L", label, color).clicked() {
                        self.toggle_later();
                    }
                });
            });
    }

    fn library_top_bar(&mut self, ctx: &egui::Context, p: &Palette) {
        egui::TopBottomPanel::top("library_top")
            .frame(top_frame(p))
            .show_separator_line(false)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        self.theme_button(ui, p);
                        self.web_menu(ui, p);
                        ui.label(
                            egui::RichText::new(format!("v{VERSION}"))
                                .size(12.0)
                                .color(p.muted),
                        );
                        ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                            ui.label(egui::RichText::new("Library").size(18.0).color(p.text));
                        });
                    });
                });
                if let Some(dir) = self.config.library.clone() {
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(dir.display().to_string())
                                .size(12.0)
                                .color(p.muted),
                        )
                        .truncate(),
                    )
                    .on_hover_text(dir.display().to_string());
                    ui.horizontal_wrapped(|ui| {
                        let add =
                            egui::Button::new(egui::RichText::new("Add deck…").color(p.on_accent))
                                .fill(p.accent);
                        if ui.add(add).clicked() {
                            self.add_decks_via_picker();
                        }
                        if ui.button("Refresh").clicked() {
                            self.rescan_library();
                        }
                        if ui.button("Change folder…").clicked() {
                            self.choose_library_folder();
                        }
                        if ui.button("Open deck…").clicked() {
                            self.pick_and_open();
                        }
                    });
                }
            });
    }

    fn library_central(&mut self, ctx: &egui::Context, p: &Palette) {
        egui::CentralPanel::default()
            .frame(central_frame(p))
            .show(ctx, |ui| {
                let width = ui.available_width().min(CARD_MAX_WIDTH);
                self.error_banners(ui, p, width);
                if self.config.library.is_none() {
                    self.welcome(ui, p);
                    return;
                }
                if let Some(error) = self.library_error.clone() {
                    ui.add(egui::Label::new(egui::RichText::new(error).color(p.danger)).wrap());
                    if ui.button("Choose another folder…").clicked() {
                        self.choose_library_folder();
                    }
                    return;
                }
                self.library_list(ui, p);
            });
    }

    /// First launch always lets the user choose the library folder.
    fn welcome(&mut self, ui: &mut egui::Ui, p: &Palette) {
        let height = ui.available_height();
        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            centered_v(ui, height, |ui| {
                ui.vertical_centered(|ui| {
                    ui.label(egui::RichText::new("Runki").size(36.0).color(p.text));
                    ui.label(egui::RichText::new("Flip through your decks. Nothing to grade.").color(p.muted));
                    ui.add_space(24.0);
                    let choose = egui::Button::new(egui::RichText::new("Choose a library folder…").color(p.on_accent))
                        .fill(p.accent).min_size(egui::vec2(220.0, 40.0));
                    if ui.add(choose).clicked() { self.choose_library_folder(); }
                    ui.add_space(4.0);
                    ui.add(egui::Label::new(egui::RichText::new(
                        "Any folder works. Choose one in iCloud Drive or Dropbox to sync decks and progress between Macs."
                    ).size(12.0).color(p.muted)).wrap());
                    ui.add_space(16.0);
                    if ui.button("Open deck…").clicked() { self.pick_and_open(); }
                    ui.label(egui::RichText::new("or drop an .apkg, .csv, .tsv or .txt file here").size(12.0).color(p.muted));
                });
            });
        });
    }

    fn library_list(&mut self, ui: &mut egui::Ui, p: &Palette) {
        if self.library_entries.is_empty() {
            let height = ui.available_height();
            centered_v(ui, height, |ui| {
                ui.vertical_centered(|ui| {
                    ui.label(
                        egui::RichText::new("Your library starts here")
                            .size(20.0)
                            .color(p.text),
                    );
                    ui.label(
                        egui::RichText::new("Use Add deck… or drop deck files into this window.")
                            .color(p.muted),
                    );
                });
            });
            return;
        }
        let now = progress::now_ms();
        let width = ui.available_width().min(800.0);
        let mut to_open = None;
        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            ui.vertical_centered(|ui| {
                for entry in &self.library_entries {
                    ui.push_id(&entry.path, |ui| {
                        let ready = matches!(entry.status, DeckStatus::Ready { .. });
                        let row = ui.allocate_ui(egui::vec2(width, 0.0), |ui| {
                            egui::Frame::new().fill(p.surface).stroke(egui::Stroke::new(1.0_f32, p.line))
                                .corner_radius(12).inner_margin(16).show(ui, |ui| {
                                    ui.set_width(ui.available_width());
                                    ui.with_layout(egui::Layout::left_to_right(egui::Align::Min), |ui| {
                                        ui.vertical(|ui| {
                                            let color = if entry.status == DeckStatus::NotDownloaded { p.muted } else { p.text };
                                            ui.add(egui::Label::new(egui::RichText::new(&entry.display_name).size(16.0).color(color)).wrap());
                                            match &entry.status {
                                                DeckStatus::Loading => { ui.horizontal(|ui| { ui.spinner(); ui.label(egui::RichText::new("Loading…").color(p.muted)); }); }
                                                DeckStatus::Ready { total, seen, later } => {
                                                    ui.horizontal_wrapped(|ui| {
                                                        ui.label(egui::RichText::new(format!("{total} cards")).size(12.0).color(p.muted));
                                                        ui.label(egui::RichText::new(format!("{seen} / {total} seen")).size(12.0).color(p.muted));
                                                        ui.label(egui::RichText::new(format!("{later} for later")).size(12.0).color(if *later > 0 { p.accent } else { p.muted }));
                                                        ui.label(egui::RichText::new(format!("Last opened: {}", library::last_opened_label(entry.opened_at, now))).size(12.0).color(p.muted));
                                                    });
                                                    let fraction = seen_fraction(*seen, *total);
                                                    let (track, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 3.0), egui::Sense::hover());
                                                    ui.painter().rect_filled(track, 2, p.line);
                                                    if fraction > 0.0 {
                                                        let fill = egui::Rect::from_min_size(track.min, egui::vec2(track.width() * fraction, track.height()));
                                                        ui.painter().rect_filled(fill, 2, p.accent);
                                                    }
                                                }
                                                DeckStatus::Failed(error) => { ui.add(egui::Label::new(egui::RichText::new(error).color(p.danger)).wrap()); }
                                                DeckStatus::NotDownloaded => {
                                                    ui.label(egui::RichText::new("Not downloaded – open in Finder to download").size(12.0).color(p.muted));
                                                    #[cfg(target_os = "macos")]
                                                    if ui.small_button("Show in Finder").clicked() { show_in_finder(&entry.path); }
                                                }
                                            }
                                        });
                                    });
                                });
                        });
                        if ready {
                            let response = row.response.interact(egui::Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand);
                            if response.hovered() { ui.painter().rect_stroke(response.rect, 12, egui::Stroke::new(1.0_f32, p.accent), egui::StrokeKind::Inside); }
                            if response.clicked() { to_open = Some(entry.path.clone()); }
                        }
                    });
                    ui.add_space(4.0);
                }
            });
        });
        if let Some(path) = to_open {
            self.open_path(path);
        }
    }

    fn error_banners(&mut self, ui: &mut egui::Ui, p: &Palette, width: f32) {
        for slot in [&mut self.error, &mut self.save_error, &mut self.web_error] {
            let Some(error) = slot.clone() else { continue };
            ui.vertical_centered(|ui| {
                ui.allocate_ui(egui::vec2(width, 0.0), |ui| {
                    egui::Frame::new()
                        .fill(p.danger_bg)
                        .corner_radius(10)
                        .inner_margin(egui::Margin::symmetric(14, 10))
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                                if ui
                                    .add(
                                        egui::Button::new(
                                            egui::RichText::new("×").size(18.0).color(p.danger),
                                        )
                                        .frame(false),
                                    )
                                    .on_hover_text("Dismiss")
                                    .clicked()
                                {
                                    *slot = None;
                                }
                                ui.with_layout(
                                    egui::Layout::left_to_right(egui::Align::Min),
                                    |ui| {
                                        ui.add(
                                            egui::Label::new(
                                                egui::RichText::new(error).color(p.danger),
                                            )
                                            .wrap(),
                                        );
                                    },
                                );
                            });
                        });
                });
            });
            ui.add_space(4.0);
        }
    }

    /// Below the card: a thin bar of % seen (`seen / deck total`)
    /// with the seen count and **Reset…**, then the card's place in
    /// the shown list as plain text. The bar is deliberately not
    /// the place in the list: resuming a shuffled deck can start
    /// far into the list (e.g. card 1412 of 3090) with few cards
    /// seen, so a position bar would look like progress not made.
    fn progress_row(&mut self, ui: &mut egui::Ui, p: &Palette, n: usize, view_len: usize) {
        let Some(session) = &self.session else { return };
        let seen = session.seen_count();
        let total = session.total();
        let fraction = study_progress_fraction(session);
        centered_row(ui, |ui| {
            let (rect, _) = ui.allocate_exact_size(egui::vec2(96.0, 4.0), egui::Sense::hover());
            ui.painter().rect_filled(rect, 2, p.line);
            if fraction > 0.0 {
                let fill = egui::Rect::from_min_size(
                    rect.min,
                    egui::vec2(rect.width() * fraction, rect.height()),
                );
                ui.painter().rect_filled(fill, 2, p.accent);
            }
            ui.label(
                egui::RichText::new(format!("{seen} / {total} seen"))
                    .size(12.0)
                    .color(p.muted),
            );
            if seen > 0
                && ui
                    .small_button("Reset…")
                    .on_hover_text("Clear which cards you have seen")
                    .clicked()
            {
                self.show_reset_confirm = true;
            }
        });
        centered_row(ui, |ui| {
            ui.label(
                egui::RichText::new(format!("Card {n} / {view_len}"))
                    .size(12.0)
                    .color(p.muted),
            );
        });
    }

    fn seen_row(&mut self, ui: &mut egui::Ui, p: &Palette) {
        let Some(session) = &self.session else { return };
        let seen = session.seen_count();
        let total = session.total();
        centered_row(ui, |ui| {
            ui.label(
                egui::RichText::new(format!("{seen} / {total} seen"))
                    .size(12.0)
                    .color(p.muted),
            );
            if seen > 0
                && ui
                    .small_button("Reset…")
                    .on_hover_text("Clear which cards you have seen")
                    .clicked()
            {
                self.show_reset_confirm = true;
            }
        });
    }

    fn central(&mut self, ctx: &egui::Context, p: &Palette) {
        egui::CentralPanel::default()
            .frame(central_frame(p))
            .show(ctx, |ui| {
                let width = ui.available_width().min(CARD_MAX_WIDTH);
                self.error_banners(ui, p, width);
                let height = ui.available_height();
                let Some((card, n, total)) = self.current() else {
                    egui::ScrollArea::vertical()
                        .auto_shrink(false)
                        .show(ui, |ui| {
                            centered_v(ui, height, |ui| {
                                ui.vertical_centered(|ui| {
                                    let Some(session) = &self.session else { return };
                                    match session.filter() {
                                        CardFilter::Unseen => {
                                            ui.label(
                                                egui::RichText::new(format!(
                                                    "You've seen all {} cards",
                                                    session.total()
                                                ))
                                                .size(20.0)
                                                .color(p.text),
                                            );
                                            if ui.button("Reset progress").clicked() {
                                                self.show_reset_confirm = true;
                                            }
                                        }
                                        CardFilter::Later => {
                                            ui.label(
                                                egui::RichText::new("Nothing saved for later")
                                                    .size(20.0)
                                                    .color(p.text),
                                            );
                                            ui.label(
                                                egui::RichText::new(
                                                    "Press L on any card to keep it here.",
                                                )
                                                .color(p.muted),
                                            );
                                        }
                                        CardFilter::All => {
                                            ui.label(
                                                egui::RichText::new("This deck has no cards")
                                                    .size(20.0)
                                                    .color(p.text),
                                            );
                                        }
                                    }
                                    if session.filter() == CardFilter::Later {
                                        self.seen_row(ui, p);
                                    }
                                });
                            });
                        });
                    return;
                };
                let session = self.session.as_ref().expect("session for current card");
                let flagged = session.is_later(&card.id);
                let revealed = session.revealed();
                let (front, back) = (card.front.clone(), card.back.clone());
                let mut card_clicked = false;
                egui::ScrollArea::vertical()
                    .auto_shrink(false)
                    .show(ui, |ui| {
                        centered_v(ui, height, |ui| {
                            ui.vertical_centered(|ui| {
                                card_clicked = study_card(
                                    ui,
                                    p,
                                    width,
                                    CARD_MIN_HEIGHT.min((height - 90.0).max(120.0)),
                                    &front,
                                    revealed.then_some(back.as_str()),
                                    flagged,
                                );
                                ui.add_space(12.0);
                                self.progress_row(ui, p, n, total);
                            });
                        });
                    });
                if card_clicked && !self.show_reset_confirm {
                    self.space();
                }
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

/// The card itself: question, then the answer once revealed. Returns
/// whether it was clicked (which acts like Space).
fn study_card(
    ui: &mut egui::Ui,
    p: &Palette,
    width: f32,
    min_height: f32,
    front: &str,
    back: Option<&str>,
    flagged: bool,
) -> bool {
    let shown = ui.allocate_ui(egui::vec2(width, 0.0), |ui| {
        egui::Frame::new()
            .fill(p.surface)
            .stroke(egui::Stroke::new(1.0_f32, p.line))
            .corner_radius(16)
            .shadow(egui::Shadow {
                offset: [0, 6],
                blur: 24,
                spread: 0,
                color: p.shadow,
            })
            .inner_margin(egui::Margin::symmetric(40, 36))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                centered_v(ui, min_height - 72.0, |ui| {
                    ui.vertical_centered(|ui| {
                        let front = egui::RichText::new(front).size(26.0).color(p.text);
                        ui.add(egui::Label::new(front).wrap());
                        if let Some(back) = back {
                            ui.add(egui::Separator::default().spacing(40.0));
                            let back = egui::RichText::new(back).size(20.0).color(p.text_soft);
                            ui.add(egui::Label::new(back).wrap());
                        }
                    });
                });
            })
            .response
            .rect
    });
    let rect = shown.inner;

    if flagged {
        let anchor = rect.right_top() + egui::vec2(-20.0, 18.0);
        let text = ui.painter().text(
            anchor,
            egui::Align2::RIGHT_TOP,
            "Later",
            egui::FontId::proportional(12.0),
            p.accent,
        );
        ui.painter().circle_filled(
            egui::pos2(text.left() - 8.0, text.center().y),
            3.0,
            p.accent,
        );
    }

    ui.interact(rect, ui.id().with("card"), egui::Sense::click())
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .clicked()
}

/// Fraction of the deck seen: `seen / total` (0 for an empty deck).
fn seen_fraction(seen: usize, total: usize) -> f32 {
    seen as f32 / total.max(1) as f32
}

/// The study progress bar's fill: % seen, never the card's place
/// in the shown (possibly shuffled) list.
fn study_progress_fraction(session: &StudySession) -> f32 {
    seen_fraction(session.seen_count(), session.total())
}

/// A segmented control; returns the newly picked value, if any.
fn segmented<T: Copy + PartialEq>(
    ui: &mut egui::Ui,
    p: &Palette,
    selected: T,
    segments: &[(T, &str)],
) -> Option<T> {
    let mut picked = None;
    egui::Frame::new()
        .fill(p.line)
        .corner_radius(9)
        .inner_margin(3)
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            ui.horizontal(|ui| {
                for &(value, label) in segments {
                    let on = value == selected;
                    let text = egui::RichText::new(label).color(if on { p.text } else { p.muted });
                    let fill = if on {
                        p.chip
                    } else {
                        egui::Color32::TRANSPARENT
                    };
                    let button = egui::Button::new(text)
                        .fill(fill)
                        .corner_radius(7)
                        .min_size(egui::vec2(0.0, 24.0));
                    if ui.add(button).clicked() && !on {
                        picked = Some(value);
                    }
                }
            });
        });
    picked
}

/// The light / dark / system button; returns whether it was clicked.
fn theme_toggle(ui: &mut egui::Ui, p: &Palette, current: egui::ThemePreference) -> bool {
    let next = theme::next(current);
    let icon = egui::RichText::new(theme::icon(current))
        .size(16.0)
        .color(p.muted);
    ui.add(egui::Button::new(icon).min_size(egui::vec2(30.0, 30.0)))
        .on_hover_text(format!(
            "Theme: {}. Click for {}.",
            theme::name(current),
            theme::name(next).to_lowercase()
        ))
        .clicked()
}

/// A key drawn as a small keycap.
fn keycap(ui: &mut egui::Ui, p: &Palette, key: &str) {
    egui::Frame::new()
        .fill(p.surface)
        .stroke(egui::Stroke::new(1.0_f32, p.line))
        .corner_radius(5)
        .inner_margin(egui::Margin::symmetric(6, 1))
        .show(ui, |ui| {
            ui.label(egui::RichText::new(key).size(11.0).color(p.muted));
        });
}

/// A clickable "keycap + label" hint, for mouse users.
fn key_hint(
    ui: &mut egui::Ui,
    p: &Palette,
    key: &str,
    label: &str,
    color: egui::Color32,
) -> egui::Response {
    let background = ui.painter().add(egui::Shape::Noop);
    let response = ui
        .scope_builder(egui::UiBuilder::new().sense(egui::Sense::click()), |ui| {
            egui::Frame::new()
                .inner_margin(egui::Margin::symmetric(10, 6))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        keycap(ui, p, key);
                        ui.label(egui::RichText::new(label).color(color));
                    });
                });
        })
        .response;
    if response.hovered() {
        ui.painter().set(
            background,
            egui::epaint::RectShape::filled(response.rect, 8, p.hover),
        );
    }
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Lay out a row centred horizontally, using the width it measured on
/// the previous pass (a changed width triggers one discarded pass).
fn centered_row(ui: &mut egui::Ui, add_contents: impl FnOnce(&mut egui::Ui)) {
    let id = ui.next_auto_id().with("centered_row");
    let last: f32 = ui.ctx().data(|d| d.get_temp(id)).unwrap_or(0.0);
    let pad = ((ui.available_width() - last) / 2.0).max(0.0);
    ui.horizontal(|ui| {
        ui.add_space(pad);
        let width = ui.horizontal(add_contents).response.rect.width();
        remember_size(ui, id, last, width);
    });
}

/// Lay out contents centred vertically in `height`, using the height
/// measured on the previous pass, and take up at least `height`.
fn centered_v(ui: &mut egui::Ui, height: f32, add_contents: impl FnOnce(&mut egui::Ui)) {
    let start = ui.next_widget_position().y;
    let id = ui.next_auto_id().with("centered_v");
    let last: f32 = ui.ctx().data(|d| d.get_temp(id)).unwrap_or(0.0);
    let pad = ((height - last) / 2.0).max(0.0);
    ui.add_space(pad);
    let measured = ui.vertical(add_contents).response.rect.height();
    remember_size(ui, id, last, measured);
    let used = ui.next_widget_position().y - start;
    ui.add_space((height - used).max(0.0));
}

fn remember_size(ui: &egui::Ui, id: egui::Id, last: f32, measured: f32) {
    if (measured - last).abs() > 0.5 {
        ui.ctx().data_mut(|d| d.insert_temp(id, measured));
        ui.ctx().request_discard("re-centre after a size change");
    }
}

/// While a file is dragged over the window, say it can be dropped.
fn drop_overlay(ctx: &egui::Context, p: &Palette, adding: bool) {
    if ctx.input(|i| i.raw.hovered_files.is_empty()) {
        return;
    }
    let painter = ctx.layer_painter(egui::LayerId::new(
        egui::Order::Foreground,
        egui::Id::new("drop_overlay"),
    ));
    let screen = ctx.screen_rect();
    painter.rect_filled(screen, 0, p.bg.gamma_multiply(0.92));
    painter.rect_stroke(
        screen.shrink(16.0),
        16,
        egui::Stroke::new(2.0_f32, p.accent),
        egui::StrokeKind::Inside,
    );
    painter.text(
        screen.center(),
        egui::Align2::CENTER_CENTER,
        if adding {
            "Drop to add to library"
        } else {
            "Drop to open"
        },
        egui::FontId::proportional(20.0),
        p.accent,
    );
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
            library_slot: Arc::new(RwLock::new(None)),
            web: None,
            web_port: 8765,
            web_port_input: "8765".into(),
            web_bridge: Arc::new(WebMenuBridge::default()),
            web_error: None,
            config: Config::default(),
            config_path: None,
            theme: egui::ThemePreference::System,
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

    #[derive(Default)]
    struct MemoryStorage(std::collections::HashMap<String, String>);

    impl eframe::Storage for MemoryStorage {
        fn get_string(&self, key: &str) -> Option<String> {
            self.0.get(key).cloned()
        }

        fn set_string(&mut self, key: &str, value: String) {
            self.0.insert(key.to_owned(), value);
        }

        fn flush(&mut self) {}
    }

    #[test]
    fn app_save_preserves_library_theme_and_deck_progress() {
        let (mut app, temp) = test_app("save-all-settings", &[]);
        app.config.library = Some(temp.0.clone());
        app.config.last_open_deck = Some(session(&app).deck().path.clone());
        app.theme = egui::ThemePreference::Light;
        app.toggle_later();

        let mut storage = MemoryStorage::default();
        eframe::App::save(&mut app, &mut storage);

        let restored: Config = eframe::get_value(&storage, CONFIG_KEY).expect("saved config");
        assert_eq!(restored, app.config);
        assert_eq!(theme::load(Some(&storage)), egui::ThemePreference::Light);
        assert!(
            !session(&app).dirty(),
            "app save also flushes deck progress"
        );
        assert!(session(&app).state_path().exists());
        assert!(
            !eframe::App::persist_egui_memory(&app),
            "styles are rebuilt on launch"
        );
    }

    #[test]
    fn desktop_save_mirrors_settings_used_by_the_terminal() {
        let (app, temp) = test_app("shared-config", &[]);
        let path = temp.0.join("config.json");
        let mut app = app.with_config_path(path.clone());
        app.config.terminal_light = true;
        let mut storage = MemoryStorage::default();
        eframe::App::save(&mut app, &mut storage);
        assert_eq!(
            crate::config::load(&path).unwrap(),
            Some(app.config.clone())
        );
    }

    #[test]
    fn existing_config_without_a_theme_follows_the_system() {
        let config = Config {
            library: Some(PathBuf::from("existing-library")),
            last_open_deck: Some(PathBuf::from("existing-library/deck.tsv")),
            ..Default::default()
        };
        let mut storage = MemoryStorage::default();
        eframe::set_value(&mut storage, CONFIG_KEY, &config);

        assert_eq!(theme::load(Some(&storage)), egui::ThemePreference::System);
        assert_eq!(
            eframe::get_value::<Config>(&storage, CONFIG_KEY),
            Some(config)
        );
    }

    #[test]
    fn study_layout_settles_at_normal_and_minimum_window_sizes() {
        let (mut app, _temp) = test_app("study-layout", &["a"]);
        app.space();
        app.session
            .as_mut()
            .expect("session")
            .set_order(CardOrder::Shuffled, 2_000);
        let ctx = egui::Context::default();
        theme::install(&ctx);
        for size in [egui::vec2(900.0, 650.0), egui::vec2(520.0, 400.0)] {
            for frame in 0..8 {
                let output = ctx.run(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
                        ..Default::default()
                    },
                    |ctx| {
                        let p = Palette::of(ctx);
                        app.top_bar(ctx, &p);
                        assert!(
                            ctx.available_rect().top() < 180.0,
                            "header leaves room for the card"
                        );
                        app.bottom_bar(ctx, &p);
                        app.central(ctx, &p);
                    },
                );
                if frame == 7 {
                    assert_eq!(
                        output.platform_output.num_completed_passes, 1,
                        "layout converges without repeated measurement"
                    );
                    assert!(!output.platform_output.requested_discard());
                }
            }
        }
    }

    #[test]
    fn study_progress_bar_uses_seen_not_view_position() {
        // Mirrors a resumed shuffle: the card on screen can sit far
        // into the shown list while few cards have been seen. The
        // bar must show % seen, not that position.
        let (mut app, _temp) = test_app("progress-seen", &[]);
        app.space(); // reveal a -> 1 of 3 seen
        app.space(); // advance to b -> position 2 of 3, still 1 seen
        assert_eq!(session(&app).view_position(), Some((2, 3)));
        assert_eq!(session(&app).seen_count(), 1);
        assert!(
            (study_progress_fraction(session(&app)) - 1.0 / 3.0).abs() < f32::EPSILON,
            "bar is seen/total, not view position 2/3"
        );
        assert_eq!(seen_fraction(0, 0), 0.0);
        assert_eq!(seen_fraction(3, 3), 1.0);
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
            ..Default::default()
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
            ..Default::default()
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
            ..Default::default()
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

    #[test]
    fn web_overrides_toggle_persistence_and_library_slot() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            web_on: true,
            web_port: 4321,
            ..Default::default()
        };
        let mut app = RunkiApp::new_with_web_options(
            config.clone(),
            None,
            WebOptions {
                on: Some(false),
                port: Some(0),
            },
        );
        assert!(app.web.is_none());
        let mut storage = MemoryStorage::default();
        eframe::App::save(&mut app, &mut storage);
        assert_eq!(
            eframe::get_value::<Config>(&storage, CONFIG_KEY),
            Some(config)
        );
        app.set_library(dir.path().to_path_buf());
        assert_eq!(*app.library_slot.read().unwrap(), app.config.library);
        app.toggle_web();
        assert!(app.web.is_some());
        assert!(app.config.web_on);
        assert_eq!(app.config.web_port, 4321);
        let port = app.web.as_ref().unwrap().local_addr.port();
        app.toggle_web();
        assert!(app.web.is_none());
        assert!(!app.config.web_on);
        let _listener =
            std::net::TcpListener::bind((std::net::Ipv4Addr::UNSPECIFIED, port)).unwrap();
    }

    #[test]
    fn failed_web_start_leaves_toggle_off_and_reports_error() {
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::UNSPECIFIED, 0)).unwrap();
        let config = Config {
            web_port: listener.local_addr().unwrap().port(),
            ..Default::default()
        };
        let mut app = RunkiApp::new(config, None);
        app.toggle_web();
        assert!(app.web.is_none());
        assert!(!app.config.web_on);
        assert!(app
            .web_error
            .as_deref()
            .unwrap()
            .contains("Could not start Web"));
    }

    fn unused_web_port() -> u16 {
        std::net::TcpListener::bind((std::net::Ipv4Addr::UNSPECIFIED, 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    fn running_web_app() -> RunkiApp {
        let port = unused_web_port();
        let app = RunkiApp::new(
            Config {
                web_on: true,
                web_port: port,
                ..Default::default()
            },
            None,
        );
        assert!(app.web.is_some(), "{:?}", app.web_error);
        app
    }

    #[test]
    fn web_port_validation_matches_cli_rules_and_invalid_edits_do_not_apply() {
        for (input, expected) in [("1", 1), ("8765", 8765), ("65535", 65535), ("00080", 80)] {
            assert_eq!(parse_web_port(input), Some(expected));
        }
        let mut app = RunkiApp::new(Config::default(), None);
        for input in [
            "",
            "0",
            "65536",
            "port",
            "-1",
            "+1",
            " 80",
            "80 ",
            "8.0",
            "８０",
            "999999999999999999999999",
        ] {
            assert_eq!(parse_web_port(input), None, "{input:?}");
            app.web_port_input = input.into();
            app.apply_web_port_with(|_, _| panic!("invalid port must not start Web"));
            assert_eq!(app.web_port, 8765);
            assert_eq!(app.config.web_port, 8765);
            assert!(app.web.is_none());
        }
    }

    #[test]
    fn applying_web_port_while_off_is_remembered_by_shared_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let mut app = RunkiApp::new_with_web_options(
            Config::default(),
            None,
            WebOptions {
                on: Some(false),
                port: Some(9000),
            },
        )
        .with_config_path(path.clone());
        assert_eq!(app.web_port_input, "9000");
        app.web_port_input = "9100".into();
        app.apply_web_port();
        assert!(app.web.is_none());
        assert_eq!(app.web_port, 9100);
        assert_eq!(app.config.web_port, 9100);
        assert_eq!(app.web_bridge.status().port, 9100);
        let mut storage = MemoryStorage::default();
        eframe::App::save(&mut app, &mut storage);
        // The TUI loads this same config on its next launch.
        assert_eq!(crate::config::load(&path).unwrap().unwrap().web_port, 9100);
        assert_eq!(
            eframe::get_value::<Config>(&storage, CONFIG_KEY)
                .unwrap()
                .web_port,
            9100
        );
    }

    #[test]
    fn applying_web_port_while_on_moves_listener_and_publishes_address() {
        let mut app = running_web_app();
        let old_port = app.web_port;
        let port = unused_web_port();
        app.web_port_input = port.to_string();
        app.apply_web_port();
        assert_eq!(app.web_port, port);
        assert_eq!(app.config.web_port, port);
        assert!(app.config.web_on);
        assert!(app.web_error.is_none());
        assert_eq!(app.web.as_ref().unwrap().local_addr.port(), port);
        assert!(std::net::TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port)).is_ok());
        assert!(std::net::TcpListener::bind((std::net::Ipv4Addr::UNSPECIFIED, port)).is_err());
        let _old =
            std::net::TcpListener::bind((std::net::Ipv4Addr::UNSPECIFIED, old_port)).unwrap();
        let status = app.web_bridge.status();
        assert!(status.on);
        assert_eq!(status.port, port);
        assert_eq!(
            status.lan_url.as_deref(),
            Some(app.web.as_ref().unwrap().lan_url())
        );
    }

    #[test]
    fn applying_occupied_web_port_restores_old_listener_and_remembered_port() {
        let occupied = std::net::TcpListener::bind((std::net::Ipv4Addr::UNSPECIFIED, 0)).unwrap();
        let port = occupied.local_addr().unwrap().port();
        let mut app = running_web_app();
        let old_port = app.web_port;
        // Exercise a temporary CLI override that differs from the remembered port.
        app.config.web_port = 4321;
        app.web_port_input = port.to_string();
        app.apply_web_port();
        assert_eq!(app.web_port, old_port);
        assert_eq!(app.config.web_port, 4321);
        assert!(app.config.web_on);
        assert_eq!(app.web.as_ref().unwrap().local_addr.port(), old_port);
        assert!(std::net::TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, old_port)).is_ok());
        assert!(std::net::TcpListener::bind((std::net::Ipv4Addr::UNSPECIFIED, old_port)).is_err());
        assert!(app
            .web_error
            .as_deref()
            .unwrap()
            .contains(&format!("Could not use Web port {port}")));
        assert!(app
            .web_error
            .as_deref()
            .unwrap()
            .contains(&format!("Still serving on port {old_port}")));
        assert!(app.web_bridge.status().on);
        assert_eq!(app.web_bridge.status().port, old_port);
    }

    #[test]
    fn failed_web_port_apply_and_rollback_turn_web_off_without_remembering_new_port() {
        let mut app = running_web_app();
        let old_port = app.web_port;
        let port = unused_web_port();
        app.web_port_input = port.to_string();
        let mut attempts = Vec::new();
        app.apply_web_port_with(|_, port| {
            attempts.push(port);
            Err(anyhow::anyhow!("simulated bind failure"))
        });
        assert_eq!(attempts, [port, old_port]);
        assert!(app.web.is_none());
        assert!(!app.config.web_on);
        assert_eq!(app.config.web_port, old_port);
        assert_eq!(app.web_port, old_port);
        assert!(!app.web_bridge.status().on);
        assert!(app.web_bridge.status().lan_url.is_none());
        assert!(app
            .web_error
            .as_deref()
            .unwrap()
            .contains("Could not restore port"));
        let _released =
            std::net::TcpListener::bind((std::net::Ipv4Addr::UNSPECIFIED, old_port)).unwrap();
    }

    #[test]
    fn cli_web_port_stays_temporary_without_deliberate_apply() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let config = Config {
            web_port: 4321,
            ..Default::default()
        };
        let mut app = RunkiApp::new_with_web_options(
            config.clone(),
            None,
            WebOptions {
                on: Some(false),
                port: Some(9000),
            },
        )
        .with_config_path(path.clone());
        assert_eq!(app.web_port, 9000);
        assert_eq!(app.web_port_input, "9000");
        let mut storage = MemoryStorage::default();
        eframe::App::save(&mut app, &mut storage);
        assert_eq!(crate::config::load(&path).unwrap(), Some(config.clone()));
        assert_eq!(
            eframe::get_value::<Config>(&storage, CONFIG_KEY),
            Some(config)
        );
    }

    #[test]
    fn native_menu_requests_use_gui_toggle_and_publish_actual_state() {
        let bridge = Arc::new(WebMenuBridge::default());
        let mut app = RunkiApp::new(
            Config {
                web_port: unused_web_port(),
                ..Default::default()
            },
            None,
        )
        .with_web_bridge(Arc::clone(&bridge));
        bridge.request_toggle();
        assert!(app.web.is_none(), "menu callback only records intent");
        app.handle_web_menu_requests();
        assert!(app.web.is_some());
        assert!(app.config.web_on);
        assert!(bridge.status().on);
        assert_eq!(
            bridge.status().lan_url.as_deref(),
            Some(app.web.as_ref().unwrap().lan_url())
        );
        app.handle_web_menu_requests();
        assert!(app.web.is_some(), "request is not consumed twice");
        bridge.request_toggle();
        app.handle_web_menu_requests();
        assert!(app.web.is_none());
        assert!(!app.config.web_on);
        assert!(!bridge.status().on);
        assert!(bridge.status().lan_url.is_none());
    }

    #[test]
    fn editing_web_port_does_not_consume_study_shortcuts() {
        let (mut app, _temp) = test_app("web-port-focus", &[]);
        let ctx = egui::Context::default();
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                ui.add(egui::TextEdit::singleline(&mut app.web_port_input).id(web_port_field_id()))
                    .request_focus();
            });
        });
        let keys = || {
            vec![egui::Key::Space, egui::Key::L]
                .into_iter()
                .map(|key| egui::Event::Key {
                    key,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                })
                .collect()
        };
        let _ = ctx.run(
            egui::RawInput {
                events: keys(),
                ..Default::default()
            },
            |ctx| {
                assert!(ctx.memory(|memory| memory.has_focus(web_port_field_id())));
                app.study_shortcuts(ctx);
                assert!(!session(&app).revealed());
                assert_eq!(session(&app).later_count(), 0);
                assert!(ctx.input(|input| input.key_pressed(egui::Key::Space)));
                assert!(ctx.input(|input| input.key_pressed(egui::Key::L)));
                ctx.memory_mut(|memory| memory.surrender_focus(web_port_field_id()));
            },
        );
        let _ = ctx.run(
            egui::RawInput {
                events: keys(),
                ..Default::default()
            },
            |ctx| {
                app.study_shortcuts(ctx);
                assert!(session(&app).revealed());
                assert_eq!(session(&app).later_count(), 1);
            },
        );
    }
}
