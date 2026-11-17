use std::{
    fs,
    time::{Duration, Instant},
};

use eframe::egui;
use fast_code_editor::{
    document::{Document, Position},
    lsp::{LspEvent, LspWorker},
};

#[test]
#[ignore = "requires rust-analyzer and cargo check"]
fn rust_analyzer_completion_definition_and_diagnostics() {
    let project = tempfile::tempdir().unwrap();
    fs::create_dir(project.path().join("src")).unwrap();
    fs::write(
        project.path().join("Cargo.toml"),
        "[package]\nname = \"lsp-fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    let main_path = project.path().join("src/main.rs");
    fs::write(
        &main_path,
        "fn target() -> i32 { 42 }\nfn main() {\n    let unused = target();\n}\n",
    )
    .unwrap();
    let main_path = fs::canonicalize(main_path).unwrap();

    let document = Document::open(&main_path, 700).unwrap();
    let worker = LspWorker::new(&egui::Context::default());
    worker.open(&document, Some(project.path().to_path_buf()));

    let started = Instant::now();
    let mut last_request = None;
    let mut connected = false;
    let mut completion_found = false;
    let mut definition_found = false;
    let mut diagnostic_found = false;
    let mut statuses = Vec::new();

    while started.elapsed() < Duration::from_secs(60)
        && !(completion_found && definition_found && diagnostic_found)
    {
        if connected
            && !(completion_found && definition_found)
            && last_request.is_none_or(|time: Instant| time.elapsed() > Duration::from_secs(2))
        {
            worker.request_completion(&document, Position::new(2, 23));
            worker.request_definition(&document, Position::new(2, 19));
            last_request = Some(Instant::now());
        }
        if let Some(event) = worker.try_recv() {
            match event {
                LspEvent::ServerStatus { message, error } => {
                    statuses.push(message.clone());
                    assert!(!error, "LSP error: {message}");
                    connected |= message.contains("подключён");
                }
                LspEvent::Completion { items, .. } => {
                    completion_found |= items
                        .iter()
                        .any(|item| item.label.trim_start().starts_with("target"));
                }
                LspEvent::Definition { locations, .. } => {
                    definition_found |= locations.iter().any(|location| {
                        location
                            .uri
                            .to_file_path()
                            .is_ok_and(|path| path == main_path)
                            && location.range.start.line == 0
                    });
                }
                LspEvent::Diagnostics { diagnostics, .. } => {
                    diagnostic_found |= diagnostics
                        .iter()
                        .any(|diagnostic| diagnostic.message.contains("unused"));
                }
                _ => {}
            }
        } else {
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    assert!(
        completion_found,
        "completion missing; statuses: {statuses:?}"
    );
    assert!(
        definition_found,
        "definition missing; statuses: {statuses:?}"
    );
    assert!(
        diagnostic_found,
        "diagnostics missing; statuses: {statuses:?}"
    );
    drop(worker);
}
