use std::path::PathBuf;

use runki::app::RunkiApp;

fn main() -> eframe::Result<()> {
    let initial_deck = std::env::args().nth(1).map(PathBuf::from);
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([900.0, 650.0])
            .with_min_inner_size([520.0, 400.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Runki",
        options,
        Box::new(move |_cc| Ok(Box::new(RunkiApp::new(initial_deck)))),
    )
}
