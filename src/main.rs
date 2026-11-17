use std::path::PathBuf;

use eframe::egui;
use fast_code_editor::app::FastCodeEditor;

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("FastCodeEditor")
            .with_inner_size([1440.0, 900.0])
            .with_min_inner_size([800.0, 520.0]),
        ..Default::default()
    };

    eframe::run_native(
        "FastCodeEditor",
        options,
        Box::new(|creation_context| {
            let app = FastCodeEditor::new(creation_context);
            let path = std::env::args_os().nth(1).map(PathBuf::from);
            Ok(Box::new(app.with_initial_path(path)))
        }),
    )
}
