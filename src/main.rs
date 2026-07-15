mod app;
mod index;
mod model;
mod native_icon;
mod platform;
mod search;

use anyhow::Context;
use app::SearchApp;

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([1040.0, 680.0])
            .with_min_inner_size([720.0, 420.0])
            .with_title("Rust Everything"),
        ..Default::default()
    };

    eframe::run_native(
        "Rust Everything",
        options,
        Box::new(|creation_context| {
            let app = SearchApp::new(creation_context)
                .context("failed to initialize the search application")?;
            Ok(Box::new(app))
        }),
    )
}
