use std::path::PathBuf;

#[cfg(target_os = "macos")]
mod about_macos;

use runki::app::{Config, RunkiApp, CONFIG_KEY};

fn main() -> eframe::Result<()> {
    let initial_deck = std::env::args().nth(1).map(PathBuf::from);
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_app_id("runki")
            .with_inner_size([900.0, 650.0])
            .with_min_inner_size([520.0, 400.0])
            .with_icon(
                eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon.png"))
                    .expect("assets/icon.png is a valid PNG"),
            ),
        ..Default::default()
    };
    eframe::run_native(
        "Runki",
        options,
        Box::new(move |cc| {
            #[cfg(target_os = "macos")]
            about_macos::install();
            // A CLI deck (initial_deck) wins over the saved config:
            // it alone opens straight into the study screen.
            let config: Config = cc
                .storage
                .and_then(|storage| eframe::get_value(storage, CONFIG_KEY))
                .unwrap_or_default();
            Ok(Box::new(RunkiApp::new(config, initial_deck)))
        }),
    )
}
