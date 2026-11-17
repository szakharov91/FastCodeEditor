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
#[ignore = "requires clangd"]
fn clangd_completion_definition_and_diagnostics() {
    let project = tempfile::tempdir().unwrap();
    let main_path = project.path().join("main.cpp");
    fs::write(
        &main_path,
        "int target() { return 42; }\nint main() { int result = target(); return missing; }\n",
    )
    .unwrap();
    let main_path = fs::canonicalize(main_path).unwrap();

    let document = Document::open(&main_path, 701).unwrap();
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
            worker.request_completion(&document, Position::new(1, 32));
            worker.request_definition(&document, Position::new(1, 29));
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
                        .any(|diagnostic| diagnostic.message.contains("missing"));
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

#[test]
#[ignore = "requires clangd"]
fn clangd_hover_references_and_rename() {
    let project = tempfile::tempdir().unwrap();
    let main_path = project.path().join("main.cpp");
    fs::write(
        &main_path,
        "int target() { return 42; }\nint main() { return target(); }\n",
    )
    .unwrap();
    let main_path = fs::canonicalize(main_path).unwrap();

    let document = Document::open(&main_path, 702).unwrap();
    let worker = LspWorker::new(&egui::Context::default());
    worker.open(&document, Some(project.path().to_path_buf()));

    let started = Instant::now();
    let mut last_request = None;
    let mut connected = false;
    let mut hover_found = false;
    let mut references_found = false;
    let mut rename_found = false;
    let mut statuses = Vec::new();

    while started.elapsed() < Duration::from_secs(60)
        && !(hover_found && references_found && rename_found)
    {
        if connected
            && !(hover_found && references_found && rename_found)
            && last_request.is_none_or(|time: Instant| time.elapsed() > Duration::from_secs(2))
        {
            worker.request_hover(&document, Position::new(1, 23));
            worker.request_references(&document, Position::new(1, 23));
            worker.request_rename(&document, Position::new(1, 23), "renamed_target".into());
            last_request = Some(Instant::now());
        }
        if let Some(event) = worker.try_recv() {
            match event {
                LspEvent::ServerStatus { message, error } => {
                    statuses.push(message.clone());
                    assert!(!error, "LSP error: {message}");
                    connected |= message.contains("подключён");
                }
                LspEvent::Hover { hover, .. } => {
                    hover_found |= hover.is_some();
                }
                LspEvent::References { locations, .. } => {
                    references_found |= locations.len() >= 2;
                }
                LspEvent::Rename { edit, .. } => {
                    rename_found |= edit.is_some_and(|edit| {
                        serde_json::to_string(&edit)
                            .is_ok_and(|json| json.contains("renamed_target"))
                    });
                }
                _ => {}
            }
        } else {
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    assert!(hover_found, "hover missing; statuses: {statuses:?}");
    assert!(
        references_found,
        "references missing; statuses: {statuses:?}"
    );
    assert!(rename_found, "rename missing; statuses: {statuses:?}");
    drop(worker);
}
