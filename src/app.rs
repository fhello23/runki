//! egui front-end: one card at a time.
//!
//! - `Space` (or a click on the card) shows the answer, then moves to
//!   the next card (wrapping around at the end, so browsing never stops).
//! - `L` toggles the current card in the study-later list.
//! - The top bar switches between all cards and study-later only.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use eframe::egui;

use crate::deck::{self, Deck};
use crate::progress::{self, DeckState, LoadOutcome};
use crate::theme::{self, Palette};

/// How often a dirty state is written while studying.
const SAVE_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    All,
    Later,
}

pub struct RunkiApp {
    deck: Option<Deck>,
    /// Light, dark or follow the system; saved in the app config.
    theme: egui::ThemePreference,
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
            theme: egui::ThemePreference::System,
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

    /// Start with this theme choice (as loaded from the app config).
    pub fn with_theme(mut self, theme: egui::ThemePreference) -> Self {
        self.theme = theme;
        self
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
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        theme::store(storage, self.theme);
    }

    /// Only settings saved in [`Self::save`] are kept; egui's own
    /// memory (which includes its styles) is rebuilt on every launch.
    fn persist_egui_memory(&self) -> bool {
        false
    }

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

        let p = Palette::of(ctx);
        self.top_bar(ctx, &p);
        // The welcome screen has nothing to study, so no bottom bar.
        if self.deck.is_some() {
            self.bottom_bar(ctx, &p);
        }
        self.central(ctx, &p);
        drop_overlay(ctx, &p);
    }
}

/// Widest a card gets; very long lines are hard to read.
const CARD_MAX_WIDTH: f32 = 680.0;
/// Shortest a card gets, so a one-word question still looks like a card.
const CARD_MIN_HEIGHT: f32 = 240.0;
/// Room for the window buttons: on macOS the content runs under a
/// transparent title bar (see `main.rs`).
const TITLEBAR_INSET: i8 = if cfg!(target_os = "macos") { 28 } else { 0 };

impl RunkiApp {
    fn top_bar(&mut self, ctx: &egui::Context, p: &Palette) {
        let frame = egui::Frame::new().fill(p.bg).inner_margin(egui::Margin {
            left: 20,
            right: 16,
            top: TITLEBAR_INSET + 12,
            bottom: 8,
        });
        egui::TopBottomPanel::top("top")
            .frame(frame)
            .show_separator_line(false)
            .show(ctx, |ui| {
                // One row height, so the name lines up with the controls.
                ui.spacing_mut().interact_size.y = 30.0;
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if theme_toggle(ui, p, self.theme) {
                        self.theme = theme::next(self.theme);
                        ctx.set_theme(self.theme);
                        // Redraw at once with the new palette.
                        ctx.request_discard("theme changed");
                    }
                    // The welcome screen has its own Open button.
                    let Some(deck) = &self.deck else {
                        return;
                    };
                    let name = egui::RichText::new(&deck.name).size(15.0).color(p.text);
                    let open = egui::Button::new(egui::RichText::new("Open…").color(p.muted));
                    if ui.add(open).on_hover_text("Open another deck").clicked() {
                        self.pick_and_open();
                    }
                    ui.add_space(4.0);
                    // Right to left, so the segments are listed last first.
                    let later = match self.later_count() {
                        0 => "Later".to_string(),
                        n => format!("Later  {n}"),
                    };
                    let segments = [(Mode::Later, later.as_str()), (Mode::All, "All")];
                    if let Some(mode) = segmented(ui, p, self.mode, &segments) {
                        self.set_mode(mode);
                    }
                    ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                        ui.add(egui::Label::new(name).truncate());
                    });
                });
            });
    }

    fn bottom_bar(&mut self, ctx: &egui::Context, p: &Palette) {
        let flagged = self
            .current()
            .map(|(card, _, _)| self.state.is_later(&card.id));
        let frame = egui::Frame::new().fill(p.bg).inner_margin(egui::Margin {
            left: 20,
            right: 20,
            top: 6,
            bottom: 18,
        });
        egui::TopBottomPanel::bottom("bottom")
            .frame(frame)
            .show_separator_line(false)
            .show(ctx, |ui| {
                // Nothing to act on (empty view): keep the bar's height.
                let Some(flagged) = flagged else {
                    ui.add_space(30.0);
                    return;
                };
                centered_row(ui, |ui| {
                    let label = if self.revealed { "Next" } else { "Show answer" };
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

    fn central(&mut self, ctx: &egui::Context, p: &Palette) {
        let frame = egui::Frame::new().fill(p.bg).inner_margin(egui::Margin {
            left: 24,
            right: 24,
            top: 8,
            bottom: 8,
        });
        egui::CentralPanel::default().frame(frame).show(ctx, |ui| {
            let width = ui.available_width().min(CARD_MAX_WIDTH);
            self.error_banners(ui, p, width);

            if self.deck.is_none() {
                self.welcome(ui, p);
                return;
            }

            let Some((card, n, total)) = self.current() else {
                let (title, hint) = if self.mode == Mode::Later {
                    (
                        "Nothing saved for later",
                        "Press L on any card to keep it here.",
                    )
                } else {
                    ("This deck has no cards", "Try opening another deck.")
                };
                let height = ui.available_height();
                centered_v(ui, height, |ui| {
                    ui.vertical_centered(|ui| {
                        ui.label(egui::RichText::new(title).size(20.0).color(p.text));
                        ui.label(egui::RichText::new(hint).color(p.muted));
                    });
                });
                return;
            };
            let flagged = self.state.is_later(&card.id);
            let (front, back) = (card.front.clone(), card.back.clone());

            let height = ui.available_height();
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
                                &front,
                                self.revealed.then_some(back.as_str()),
                                flagged,
                            );
                            ui.add_space(12.0);
                            progress_row(ui, p, n, total);
                        });
                    });
                });
            if card_clicked {
                self.space();
            }
        });
    }

    fn error_banners(&mut self, ui: &mut egui::Ui, p: &Palette, width: f32) {
        for slot in [&mut self.error, &mut self.save_error] {
            let Some(error) = slot.clone() else {
                continue;
            };
            ui.vertical_centered(|ui| {
                ui.allocate_ui(egui::vec2(width, 0.0), |ui| {
                    egui::Frame::new()
                        .fill(p.danger_bg)
                        .corner_radius(10)
                        .inner_margin(egui::Margin::symmetric(14, 10))
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                                let close = egui::Button::new(
                                    egui::RichText::new("×").size(18.0).color(p.danger),
                                )
                                .frame(false);
                                if ui.add(close).on_hover_text("Dismiss").clicked() {
                                    *slot = None;
                                }
                                ui.with_layout(
                                    egui::Layout::left_to_right(egui::Align::Min),
                                    |ui| {
                                        let text = egui::RichText::new(error).color(p.danger);
                                        ui.add(egui::Label::new(text).wrap());
                                    },
                                );
                            });
                        });
                });
            });
            ui.add_space(4.0);
        }
    }

    fn welcome(&mut self, ui: &mut egui::Ui, p: &Palette) {
        let height = ui.available_height();
        centered_v(ui, height, |ui| {
            ui.vertical_centered(|ui| {
                ui.label(egui::RichText::new("Runki").size(36.0).color(p.text));
                ui.label(
                    egui::RichText::new("Flip through your decks. Nothing to grade.")
                        .color(p.muted),
                );
                ui.add_space(28.0);
                let open = egui::Button::new(
                    egui::RichText::new("Open deck")
                        .size(15.0)
                        .color(p.on_accent),
                )
                .fill(p.accent)
                .corner_radius(10)
                .min_size(egui::vec2(160.0, 40.0));
                if ui
                    .add(open)
                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                    .clicked()
                {
                    self.pick_and_open();
                }
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new("or drop an .apkg, .csv, .tsv or .txt file here")
                        .size(12.0)
                        .color(p.muted),
                );
                ui.add_space(40.0);
                centered_row(ui, |ui| {
                    keycap(ui, p, "Space");
                    ui.label(egui::RichText::new("answer, then next").color(p.muted));
                    ui.add_space(16.0);
                    keycap(ui, p, "L");
                    ui.label(egui::RichText::new("study later").color(p.muted));
                });
            });
        });
    }
}

/// The card itself: question, then the answer once revealed. Returns
/// whether it was clicked (which acts like Space).
fn study_card(
    ui: &mut egui::Ui,
    p: &Palette,
    width: f32,
    front: &str,
    back: Option<&str>,
    flagged: bool,
) -> bool {
    let shown = ui.allocate_ui(egui::vec2(width, 0.0), |ui| {
        egui::Frame::new()
            .fill(p.surface)
            .stroke(egui::Stroke::new(1.0, p.line))
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
                centered_v(ui, CARD_MIN_HEIGHT - 72.0, |ui| {
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

/// Where we are in the view: a thin bar and `n / total`.
fn progress_row(ui: &mut egui::Ui, p: &Palette, n: usize, total: usize) {
    centered_row(ui, |ui| {
        let (rect, _) = ui.allocate_exact_size(egui::vec2(96.0, 4.0), egui::Sense::hover());
        ui.painter().rect_filled(rect, 2, p.line);
        let done = rect.width() * n as f32 / total.max(1) as f32;
        let fill = egui::Rect::from_min_size(rect.min, egui::vec2(done, rect.height()));
        ui.painter().rect_filled(fill, 2, p.accent);
        ui.label(
            egui::RichText::new(format!("{n} / {total}"))
                .size(12.0)
                .color(p.muted),
        );
    });
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
        .stroke(egui::Stroke::new(1.0, p.line))
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
    let id = ui.id().with("centered_row");
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
    let id = ui.id().with("centered_v");
    let last: f32 = ui.ctx().data(|d| d.get_temp(id)).unwrap_or(0.0);
    let pad = ((height - last) / 2.0).max(0.0);
    ui.add_space(pad);
    let measured = ui.vertical(add_contents).response.rect.height();
    remember_size(ui, id, last, measured);
    ui.add_space((height - pad - measured).max(0.0));
}

fn remember_size(ui: &egui::Ui, id: egui::Id, last: f32, measured: f32) {
    if (measured - last).abs() > 0.5 {
        ui.ctx().data_mut(|d| d.insert_temp(id, measured));
        ui.ctx().request_discard("re-centre after a size change");
    }
}

/// While a file is dragged over the window, say it can be dropped.
fn drop_overlay(ctx: &egui::Context, p: &Palette) {
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
        egui::Stroke::new(2.0, p.accent),
        egui::StrokeKind::Inside,
    );
    painter.text(
        screen.center(),
        egui::Align2::CENTER_CENTER,
        "Drop to open",
        egui::FontId::proportional(20.0),
        p.accent,
    );
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
            theme: egui::ThemePreference::System,
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
