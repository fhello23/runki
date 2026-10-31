//! Look and feel: a quiet, card-centred theme for light and dark mode.
//!
//! Colours live in a [`Palette`] per theme. The user picks light, dark
//! or "follow the system" with a toggle; the choice is kept in eframe's
//! app storage, and [`Palette::of`] returns the palette in use.

use eframe::egui::{
    self, Color32, CornerRadius, FontFamily, FontId, Shadow, Stroke, TextStyle, Theme,
    ThemePreference, Visuals,
};

/// Key of the theme choice in eframe's app storage.
const STORAGE_KEY: &str = "theme";

#[derive(Debug, Clone, Copy)]
pub struct Palette {
    /// Window background.
    pub bg: Color32,
    /// Cards and other raised surfaces.
    pub surface: Color32,
    /// Hairlines, tracks and the segmented-control background.
    pub line: Color32,
    /// Hover fill for ghost buttons.
    pub hover: Color32,
    /// The selected segment of a segmented control.
    pub chip: Color32,
    pub text: Color32,
    /// The answer: readable, but a step back from the question.
    pub text_soft: Color32,
    pub muted: Color32,
    pub accent: Color32,
    pub on_accent: Color32,
    pub danger: Color32,
    pub danger_bg: Color32,
    pub shadow: Color32,
}

pub const LIGHT: Palette = Palette {
    bg: Color32::from_rgb(0xF6, 0xF6, 0xF4),
    surface: Color32::WHITE,
    line: Color32::from_rgb(0xE7, 0xE7, 0xE4),
    hover: Color32::from_rgb(0xEC, 0xEC, 0xE9),
    chip: Color32::WHITE,
    text: Color32::from_rgb(0x1C, 0x1C, 0x1E),
    text_soft: Color32::from_rgb(0x48, 0x48, 0x4C),
    muted: Color32::from_rgb(0x8E, 0x8E, 0x93),
    accent: Color32::from_rgb(0x5B, 0x5B, 0xD6),
    on_accent: Color32::WHITE,
    danger: Color32::from_rgb(0xC2, 0x3B, 0x32),
    danger_bg: Color32::from_rgb(0xFC, 0xEC, 0xEA),
    shadow: Color32::from_black_alpha(14),
};

pub const DARK: Palette = Palette {
    bg: Color32::from_rgb(0x12, 0x12, 0x14),
    surface: Color32::from_rgb(0x1C, 0x1C, 0x1F),
    line: Color32::from_rgb(0x2A, 0x2A, 0x2E),
    hover: Color32::from_rgb(0x24, 0x24, 0x28),
    chip: Color32::from_rgb(0x3A, 0x3A, 0x40),
    text: Color32::from_rgb(0xED, 0xED, 0xEF),
    text_soft: Color32::from_rgb(0xB4, 0xB4, 0xBA),
    muted: Color32::from_rgb(0x7C, 0x7C, 0x84),
    accent: Color32::from_rgb(0x8B, 0x8B, 0xF5),
    on_accent: Color32::from_rgb(0x12, 0x12, 0x14),
    danger: Color32::from_rgb(0xF2, 0x8B, 0x82),
    danger_bg: Color32::from_rgb(0x2E, 0x1A, 0x19),
    shadow: Color32::from_black_alpha(90),
};

impl Palette {
    /// The palette for the theme currently in use.
    pub fn of(ctx: &egui::Context) -> Self {
        if ctx.style().visuals.dark_mode {
            DARK
        } else {
            LIGHT
        }
    }
}

/// The saved theme choice; follows the system if none was saved.
pub fn load(storage: Option<&dyn eframe::Storage>) -> ThemePreference {
    storage
        .and_then(|storage| eframe::get_value(storage, STORAGE_KEY))
        .unwrap_or(ThemePreference::System)
}

pub fn store(storage: &mut dyn eframe::Storage, theme: ThemePreference) {
    eframe::set_value(storage, STORAGE_KEY, &theme);
}

/// The toggle cycles System → Light → Dark.
pub fn next(theme: ThemePreference) -> ThemePreference {
    match theme {
        ThemePreference::System => ThemePreference::Light,
        ThemePreference::Light => ThemePreference::Dark,
        ThemePreference::Dark => ThemePreference::System,
    }
}

/// The toggle's icon for a choice (all in egui's bundled fonts).
pub fn icon(theme: ThemePreference) -> &'static str {
    match theme {
        ThemePreference::System => "💻",
        ThemePreference::Light => "☀",
        ThemePreference::Dark => "🌙",
    }
}

pub fn name(theme: ThemePreference) -> &'static str {
    match theme {
        ThemePreference::System => "Match system",
        ThemePreference::Light => "Light",
        ThemePreference::Dark => "Dark",
    }
}

/// Install the light and dark styles; which one is active depends on
/// the theme choice.
pub fn install(ctx: &egui::Context) {
    ctx.style_mut_of(Theme::Light, |style| apply(style, &LIGHT, Visuals::light()));
    ctx.style_mut_of(Theme::Dark, |style| apply(style, &DARK, Visuals::dark()));
}

fn apply(style: &mut egui::Style, p: &Palette, base: Visuals) {
    style.text_styles = [
        (
            TextStyle::Small,
            FontId::new(12.0, FontFamily::Proportional),
        ),
        (TextStyle::Body, FontId::new(15.0, FontFamily::Proportional)),
        (
            TextStyle::Button,
            FontId::new(14.0, FontFamily::Proportional),
        ),
        (
            TextStyle::Heading,
            FontId::new(30.0, FontFamily::Proportional),
        ),
        (
            TextStyle::Monospace,
            FontId::new(12.0, FontFamily::Monospace),
        ),
    ]
    .into();
    style.spacing.item_spacing = egui::vec2(8.0, 8.0);
    style.spacing.button_padding = egui::vec2(12.0, 6.0);
    // Labels must not swallow clicks meant for the card behind them.
    style.interaction.selectable_labels = false;

    let mut v = base;
    v.panel_fill = p.bg;
    v.window_fill = p.surface;
    v.extreme_bg_color = p.surface;
    v.faint_bg_color = p.hover;
    v.hyperlink_color = p.accent;
    v.selection.bg_fill = p.accent.gamma_multiply(0.3);
    v.selection.stroke = Stroke::new(1.0_f32, p.accent);
    v.window_corner_radius = CornerRadius::same(12);
    v.window_stroke = Stroke::new(1.0_f32, p.line);
    v.window_shadow = Shadow {
        offset: [0, 8],
        blur: 24,
        spread: 0,
        color: p.shadow,
    };
    v.popup_shadow = v.window_shadow;

    let w = &mut v.widgets;
    for state in [
        &mut w.noninteractive,
        &mut w.inactive,
        &mut w.hovered,
        &mut w.active,
        &mut w.open,
    ] {
        state.corner_radius = CornerRadius::same(8);
        state.bg_stroke = Stroke::NONE;
        state.expansion = 0.0;
    }
    // Separators are drawn with the non-interactive stroke.
    w.noninteractive.bg_stroke = Stroke::new(1.0_f32, p.line);
    w.noninteractive.fg_stroke = Stroke::new(1.0_f32, p.text);
    w.noninteractive.bg_fill = p.surface;
    w.noninteractive.weak_bg_fill = p.surface;
    // Buttons are ghosts until hovered.
    w.inactive.weak_bg_fill = Color32::TRANSPARENT;
    w.inactive.bg_fill = p.hover;
    w.inactive.fg_stroke = Stroke::new(1.0_f32, p.text);
    w.hovered.weak_bg_fill = p.hover;
    w.hovered.bg_fill = p.hover;
    w.hovered.fg_stroke = Stroke::new(1.0_f32, p.text);
    w.active.weak_bg_fill = p.line;
    w.active.bg_fill = p.line;
    w.active.fg_stroke = Stroke::new(1.0_f32, p.text);
    w.open = w.hovered;

    style.visuals = v;
}
