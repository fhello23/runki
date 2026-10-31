use std::path::PathBuf;

use runki::app::RunkiApp;
use runki::theme;

fn main() -> eframe::Result<()> {
    let initial_deck = std::env::args().nth(1).map(PathBuf::from);
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([900.0, 650.0])
            .with_min_inner_size([520.0, 400.0])
            // macOS: content runs under a transparent title bar.
            .with_fullsize_content_view(true)
            .with_titlebar_shown(false)
            .with_title_shown(false)
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
            theme::install(&cc.egui_ctx);
            let choice = theme::load(cc.storage);
            cc.egui_ctx.set_theme(choice);
            Ok(Box::new(RunkiApp::new(initial_deck).with_theme(choice)))
        }),
    )
}
