use std::{
    collections::{HashMap, HashSet},
    fs,
    path::PathBuf,
    thread,
    time::{Duration, Instant},
};

use crossbeam_channel::{Receiver, Sender, unbounded};
use eframe::egui::{self, Align2, Context, Key, Modifiers, RichText, ScrollArea, TextEdit};

use crate::{
    document::{Document, DocumentSnapshot, Position, save_snapshot},
    editor::{self, EditorState},
    explorer::{ProjectExplorer, chevron_icon, file_icon, folder_icon},
    highlighter::{HighlightWorker, language_label},
    lsp::{LspEvent, LspWorker},
    project::{Project, is_probably_text},
};

struct OpenDocument {
    document: Document,
    editor: EditorState,
}

enum AppEvent {
    Loaded {
        result: Result<Document, String>,
    },
    Saved {
        document_id: u64,
        revision: u64,
        path: PathBuf,
        update_path: bool,
        result: Result<u64, String>,
    },
    Compacted {
        document_id: u64,
        revision: u64,
        result: Result<DocumentSnapshot, String>,
    },
    SearchResults {
        document_id: u64,
        revision: u64,
        query: String,
        results: Vec<Position>,
    },
}

#[derive(Default)]
struct SearchState {
    open: bool,
    document_id: Option<u64>,
    query: String,
    case_sensitive: bool,
    matches: Vec<Position>,
    current: usize,
    searching: bool,
}

#[derive(Default)]
struct QuickOpenState {
    open: bool,
    query: String,
    matches: Vec<usize>,
    selected: usize,
}

#[derive(Default)]
struct RenameState {
    open: bool,
    document_id: u64,
    revision: u64,
    position: Position,
    new_name: String,
    revisions: HashMap<u64, u64>,
}

struct StatusMessage {
    text: String,
    error: bool,
}

pub struct FastCodeEditor {
    documents: Vec<OpenDocument>,
    active: Option<usize>,
    next_document_id: u64,
    project: Option<Project>,
    project_started: Instant,
    explorer: ProjectExplorer,
    event_tx: Sender<AppEvent>,
    event_rx: Receiver<AppEvent>,
    highlighter: HighlightWorker,
    lsp: LspWorker,
    pending_loads: usize,
    saving: HashSet<u64>,
    compacting: HashSet<u64>,
    close_after_save: HashSet<u64>,
    close_requested: Option<u64>,
    show_sidebar: bool,
    diagnostics_open: bool,
    references_open: bool,
    references: Vec<lsp_types::Location>,
    rename: RenameState,
    search: SearchState,
    quick_open: QuickOpenState,
    status: Option<StatusMessage>,
    pending_navigation: Option<(PathBuf, lsp_types::Position)>,
    initial_path: Option<PathBuf>,
}

impl FastCodeEditor {
    pub fn new(creation_context: &eframe::CreationContext<'_>) -> Self {
        let (event_tx, event_rx) = unbounded();
        crate::theme::apply(&creation_context.egui_ctx);
        crate::theme::install_material_symbols(&creation_context.egui_ctx);

        Self {
            documents: Vec::new(),
            active: None,
            next_document_id: 1,
            project: None,
            project_started: Instant::now(),
            explorer: ProjectExplorer::default(),
            event_tx,
            event_rx,
            highlighter: HighlightWorker::new(&creation_context.egui_ctx),
            lsp: LspWorker::new(&creation_context.egui_ctx),
            pending_loads: 0,
            saving: HashSet::new(),
            compacting: HashSet::new(),
            close_after_save: HashSet::new(),
            close_requested: None,
            show_sidebar: true,
            diagnostics_open: false,
            references_open: false,
            references: Vec::new(),
            rename: RenameState::default(),
            search: SearchState::default(),
            quick_open: QuickOpenState::default(),
            status: None,
            pending_navigation: None,
            initial_path: None,
        }
    }

    pub fn with_initial_path(mut self, path: Option<PathBuf>) -> Self {
        self.initial_path = path;
        self
    }

    fn active_document(&self) -> Option<&OpenDocument> {
        self.active.and_then(|index| self.documents.get(index))
    }

    fn active_document_mut(&mut self) -> Option<&mut OpenDocument> {
        self.active.and_then(|index| self.documents.get_mut(index))
    }

    fn request_completion_active(&self) {
        if let Some(open) = self.active_document() {
            self.lsp
                .request_completion(&open.document, open.editor.cursor);
        }
    }

    fn request_definition_active(&self) {
        if let Some(open) = self.active_document() {
            self.lsp
                .request_definition(&open.document, open.editor.cursor);
        }
    }

    fn request_references_active(&self) {
        if let Some(open) = self.active_document() {
            self.lsp
                .request_references(&open.document, open.editor.cursor);
        }
    }

    fn start_rename_active(&mut self) {
        let Some(open) = self.active_document() else {
            return;
        };
        let position = open.editor.cursor;
        let name = identifier_at(&open.document, position);
        self.rename = RenameState {
            open: true,
            document_id: open.document.id,
            revision: open.document.revision,
            position,
            new_name: name,
            revisions: self
                .documents
                .iter()
                .map(|open| (open.document.id, open.document.revision))
                .collect(),
        };
    }

    fn submit_rename(&mut self) {
        let new_name = self.rename.new_name.trim().to_owned();
        if new_name.is_empty() {
            self.set_error("Новое имя символа не может быть пустым");
            return;
        }
        let Some(open) = self
            .documents
            .iter()
            .find(|open| open.document.id == self.rename.document_id)
        else {
            self.rename.open = false;
            return;
        };
        if open.document.revision != self.rename.revision {
            self.rename.open = false;
            self.set_error("Документ изменился. Повторите переименование");
            return;
        }
        self.lsp
            .request_rename(&open.document, self.rename.position, new_name);
        self.rename.open = false;
    }

    fn navigate_to_location(&mut self, context: &Context, location: &lsp_types::Location) {
        let Ok(path) = location.uri.to_file_path() else {
            self.set_error("LSP вернул некорректный путь");
            return;
        };
        let lsp_position = location.range.start;
        if let Some(index) = self
            .documents
            .iter()
            .position(|open| open.document.path == path)
        {
            self.active = Some(index);
            let position = self.documents[index]
                .document
                .from_lsp_position(lsp_position);
            self.documents[index].editor.go_to(position);
        } else {
            self.pending_navigation = Some((path.clone(), lsp_position));
            self.open_file(path, context);
        }
    }

    fn handle_definition(
        &mut self,
        context: &Context,
        document_id: u64,
        revision: u64,
        locations: Vec<lsp_types::Location>,
    ) {
        if !self
            .documents
            .iter()
            .any(|open| open.document.id == document_id && open.document.revision == revision)
        {
            return;
        }
        let Some(location) = locations.first() else {
            self.set_status("Определение не найдено");
            return;
        };
        let Ok(path) = location.uri.to_file_path() else {
            self.set_error("LSP вернул некорректный путь определения");
            return;
        };
        let lsp_position = location.range.start;
        if let Some(index) = self
            .documents
            .iter()
            .position(|open| open.document.path == path)
        {
            self.active = Some(index);
            let position = self.documents[index]
                .document
                .from_lsp_position(lsp_position);
            self.documents[index].editor.go_to(position);
        } else {
            self.pending_navigation = Some((path.clone(), lsp_position));
            self.open_file(path, context);
        }
        if locations.len() > 1 {
            self.set_status(format!(
                "Переход к первому определению из {}",
                locations.len()
            ));
        }
    }

    fn apply_rename(
        &mut self,
        document_id: u64,
        revision: u64,
        edit: Option<lsp_types::WorkspaceEdit>,
    ) {
        let stale = !self
            .documents
            .iter()
            .any(|open| open.document.id == document_id && open.document.revision == revision)
            || self.documents.iter().any(|open| {
                self.rename
                    .revisions
                    .get(&open.document.id)
                    .is_some_and(|expected| *expected != open.document.revision)
            });
        if stale {
            self.rename.revisions.clear();
            self.set_error("Документы изменились во время rename. Правки не применены");
            return;
        }
        let Some(edit) = edit else {
            self.rename.revisions.clear();
            self.set_status("Сервер не вернул изменения для rename");
            return;
        };
        let changes = match workspace_text_edits(edit) {
            Ok(changes) => changes,
            Err(error) => {
                self.rename.revisions.clear();
                self.set_error(error);
                return;
            }
        };

        let mut changed_documents = Vec::new();
        let mut errors = Vec::new();
        let mut applied = 0_usize;
        let mut changed_files = 0_usize;
        for (uri, edits) in changes {
            let Ok(path) = uri.to_file_path() else {
                errors.push("Некорректный URI в WorkspaceEdit".to_owned());
                continue;
            };
            if let Some(open) = self
                .documents
                .iter_mut()
                .find(|open| open.document.path == path)
            {
                if open.document.lsp_text().is_none() {
                    errors.push(format!("{} слишком велик для rename", path.display()));
                    continue;
                }
                let count = apply_text_edits(&mut open.document, edits);
                if count > 0 {
                    applied += count;
                    changed_files += 1;
                    changed_documents.push(open.document.id);
                }
                continue;
            }

            match Document::open(&path, u64::MAX) {
                Ok(mut document) if document.lsp_text().is_some() => {
                    let count = apply_text_edits(&mut document, edits);
                    if count > 0 {
                        match save_snapshot(&document.snapshot(), &path) {
                            Ok(_) => {
                                applied += count;
                                changed_files += 1;
                            }
                            Err(error) => errors.push(format!(
                                "Не удалось сохранить {}: {error:#}",
                                path.display()
                            )),
                        }
                    }
                }
                Ok(_) => errors.push(format!("{} слишком велик для rename", path.display())),
                Err(error) => {
                    errors.push(format!("Не удалось открыть {}: {error:#}", path.display()))
                }
            }
        }

        for document_id in changed_documents {
            if let Some(open) = self
                .documents
                .iter()
                .find(|open| open.document.id == document_id)
            {
                self.lsp.change(&open.document);
            }
        }
        self.rename.revisions.clear();
        if errors.is_empty() {
            self.set_status(format!("Rename: {applied} правок в {changed_files} файлах"));
        } else {
            self.set_error(format!(
                "Rename выполнен частично ({applied} правок): {}",
                errors.join("; ")
            ));
        }
    }

    fn sync_lsp_document(&self, document_id: u64) {
        let Some(open) = self
            .documents
            .iter()
            .find(|open| open.document.id == document_id)
        else {
            return;
        };
        self.lsp.change(&open.document);
        let cursor = open.editor.cursor;
        let prefix = open
            .document
            .line_slice_chars(cursor.line, 0, cursor.column);
        if prefix.chars().next_back().is_some_and(|character| {
            character.is_alphanumeric() || matches!(character, '_' | '.' | ':')
        }) {
            self.lsp.request_completion(&open.document, cursor);
        }
    }

    fn apply_pending_navigation(&mut self, path: &std::path::Path) {
        if !self
            .pending_navigation
            .as_ref()
            .is_some_and(|(pending_path, _)| pending_path == path)
        {
            return;
        }
        let Some((_, lsp_position)) = self.pending_navigation.take() else {
            return;
        };
        if let Some(open) = self
            .documents
            .iter_mut()
            .find(|open| open.document.path == path)
        {
            let position = open.document.from_lsp_position(lsp_position);
            open.editor.go_to(position);
        }
    }

    fn set_status(&mut self, text: impl Into<String>) {
        self.status = Some(StatusMessage {
            text: text.into(),
            error: false,
        });
    }

    fn set_error(&mut self, text: impl Into<String>) {
        self.status = Some(StatusMessage {
            text: text.into(),
            error: true,
        });
    }

    fn open_path(&mut self, path: PathBuf, context: &Context) {
        if path.is_dir() {
            self.project_started = Instant::now();
            self.explorer.reset();
            self.project = Some(Project::open(path.clone(), context));
            self.set_status(format!("Индексация {}", path.display()));
        } else {
            self.open_file(path, context);
        }
    }

    fn open_file(&mut self, path: PathBuf, context: &Context) {
        let path = fs::canonicalize(&path).unwrap_or(path);
        if let Some(index) = self
            .documents
            .iter()
            .position(|open| open.document.path == path)
        {
            self.active = Some(index);
            self.apply_pending_navigation(&path);
            return;
        }
        let id = self.next_document_id;
        self.next_document_id += 1;
        self.pending_loads += 1;
        self.set_status(format!("Открытие {}", path.display()));
        let sender = self.event_tx.clone();
        let context = context.clone();
        thread::Builder::new()
            .name("fce-file-open".into())
            .spawn(move || {
                let result = Document::open(&path, id).map_err(|error| format!("{error:#}"));
                let _ = sender.send(AppEvent::Loaded { result });
                context.request_repaint();
            })
            .expect("failed to start file loader");
    }

    fn open_file_dialog(&mut self, context: &Context) {
        if let Some(path) = rfd::FileDialog::new().pick_file() {
            self.open_file(path, context);
        }
    }

    fn open_folder_dialog(&mut self, context: &Context) {
        if let Some(path) = rfd::FileDialog::new().pick_folder() {
            self.open_path(path, context);
        }
    }

    fn save_active(&mut self, context: &Context) {
        let Some(open) = self.active_document() else {
            return;
        };
        if open.document.read_only {
            self.set_status("Большой файл открыт в режиме чтения");
            return;
        }
        let path = open.document.path.clone();
        self.save_document(open.document.id, path, false, context);
    }

    fn save_active_as(&mut self, context: &Context) {
        let Some(open) = self.active_document() else {
            return;
        };
        if open.document.read_only {
            self.set_status("Большой файл открыт в режиме чтения");
            return;
        }
        let dialog = rfd::FileDialog::new().set_file_name(open.document.file_name().into_owned());
        if let Some(path) = dialog.save_file() {
            self.save_document(open.document.id, path, true, context);
        }
    }

    fn save_document(
        &mut self,
        document_id: u64,
        path: PathBuf,
        update_path: bool,
        context: &Context,
    ) {
        let Some(open) = self
            .documents
            .iter()
            .find(|open| open.document.id == document_id)
        else {
            return;
        };
        if self.saving.contains(&document_id) || self.compacting.contains(&document_id) {
            return;
        }
        let snapshot = open.document.snapshot();
        let revision = open.document.revision;
        self.saving.insert(document_id);
        self.set_status(format!("Сохранение {}", path.display()));
        let sender = self.event_tx.clone();
        let context = context.clone();
        thread::Builder::new()
            .name("fce-file-save".into())
            .spawn(move || {
                let result = save_snapshot(&snapshot, &path).map_err(|error| format!("{error:#}"));
                let _ = sender.send(AppEvent::Saved {
                    document_id,
                    revision,
                    path,
                    update_path,
                    result,
                });
                context.request_repaint();
            })
            .expect("failed to start file saver");
    }

    fn maybe_start_compaction(&mut self, context: &Context) -> bool {
        let mut pending = false;
        let candidates: Vec<_> = self
            .documents
            .iter()
            .filter_map(|open| {
                let document = &open.document;
                let fragmented = document.fragment_count().is_some_and(|count| {
                    count >= crate::piece_table::COMPACTION_FRAGMENT_THRESHOLD
                });
                pending |= fragmented;
                if !fragmented
                    || self.saving.contains(&document.id)
                    || self.compacting.contains(&document.id)
                {
                    return None;
                }
                document
                    .compaction_snapshot()
                    .map(|snapshot| (document.id, snapshot))
            })
            .collect();

        for (document_id, snapshot) in candidates {
            let revision = snapshot.revision;
            self.compacting.insert(document_id);
            let sender = self.event_tx.clone();
            let context = context.clone();
            thread::Builder::new()
                .name("fce-compaction".into())
                .spawn(move || {
                    let result =
                        Document::compact_snapshot(&snapshot).map_err(|error| format!("{error:#}"));
                    let _ = sender.send(AppEvent::Compacted {
                        document_id,
                        revision,
                        result,
                    });
                    context.request_repaint();
                })
                .expect("failed to start piece table compaction");
        }
        pending
    }

    fn request_close(&mut self, document_id: u64) {
        let Some(open) = self
            .documents
            .iter()
            .find(|open| open.document.id == document_id)
        else {
            return;
        };
        if open.document.dirty {
            self.close_requested = Some(document_id);
        } else {
            self.remove_document(document_id);
        }
    }

    fn remove_document(&mut self, document_id: u64) {
        let Some(index) = self
            .documents
            .iter()
            .position(|open| open.document.id == document_id)
        else {
            return;
        };
        self.lsp.close(document_id);
        self.documents.remove(index);
        self.active = match (self.active, self.documents.len()) {
            (_, 0) => None,
            (Some(active), len) if active >= len => Some(len - 1),
            (Some(active), _) if active > index => Some(active - 1),
            (active, _) => active,
        };
    }

    fn handle_shortcuts(&mut self, context: &Context) {
        if consume(context, Key::Space, Modifiers::CTRL) {
            self.request_completion_active();
        }
        if consume(context, Key::F2, Modifiers::NONE) {
            self.start_rename_active();
        }
        if consume(context, Key::F12, Modifiers::SHIFT) {
            self.request_references_active();
        }
        if consume(context, Key::F12, Modifiers::NONE) {
            self.request_definition_active();
        }
        if consume(context, Key::M, Modifiers::COMMAND | Modifiers::SHIFT) {
            self.diagnostics_open = !self.diagnostics_open;
        }
        if consume(context, Key::O, Modifiers::COMMAND) {
            self.open_file_dialog(context);
        }
        if consume(context, Key::O, Modifiers::COMMAND | Modifiers::SHIFT) {
            self.open_folder_dialog(context);
        }
        if consume(context, Key::S, Modifiers::COMMAND) {
            self.save_active(context);
        }
        if consume(context, Key::S, Modifiers::COMMAND | Modifiers::SHIFT) {
            self.save_active_as(context);
        }
        if consume(context, Key::B, Modifiers::COMMAND) {
            self.show_sidebar = !self.show_sidebar;
        }
        if consume(context, Key::P, Modifiers::COMMAND) {
            self.quick_open.open = true;
            self.quick_open.query.clear();
            self.quick_open.selected = 0;
            self.quick_open.matches = self
                .project
                .as_ref()
                .map(|project| project.fuzzy_matches("", 100))
                .unwrap_or_default();
        }
        if consume(context, Key::F, Modifiers::COMMAND) {
            self.search.open = true;
            self.search.document_id = self.active_document().map(|open| open.document.id);
            if !self.search.query.is_empty() {
                self.start_search(context);
            }
        }
        if consume(context, Key::W, Modifiers::COMMAND)
            && let Some(id) = self.active_document().map(|open| open.document.id)
        {
            self.request_close(id);
        }
    }

    fn poll_events(&mut self, context: &Context) {
        while let Ok(event) = self.event_rx.try_recv() {
            match event {
                AppEvent::Loaded { result } => {
                    self.pending_loads = self.pending_loads.saturating_sub(1);
                    match result {
                        Ok(document) => {
                            let message = format!(
                                "{}: {} строк за {:.1} мс, {}",
                                document.file_name(),
                                document.len_lines(),
                                document.load_time.as_secs_f64() * 1000.0,
                                document.backend_label()
                            );
                            let path = document.path.clone();
                            let root = self.project.as_ref().map(|project| project.root.clone());
                            self.lsp.open(&document, root);
                            self.documents.push(OpenDocument {
                                document,
                                editor: EditorState::default(),
                            });
                            self.active = Some(self.documents.len() - 1);
                            self.apply_pending_navigation(&path);
                            self.set_status(message);
                        }
                        Err(error) => self.set_error(error),
                    }
                }
                AppEvent::Saved {
                    document_id,
                    revision,
                    path,
                    update_path,
                    result,
                } => {
                    self.saving.remove(&document_id);
                    match result {
                        Ok(byte_len) => {
                            let mut should_close = false;
                            let root = self.project.as_ref().map(|project| project.root.clone());
                            if let Some(open) = self
                                .documents
                                .iter_mut()
                                .find(|open| open.document.id == document_id)
                            {
                                if open.document.revision == revision {
                                    open.document.mark_saved(byte_len);
                                    if !update_path {
                                        self.lsp.save(&open.document);
                                    }
                                }
                                if update_path {
                                    self.lsp.close(document_id);
                                    open.document.path = path.clone();
                                    open.editor.invalidate_highlighting();
                                    self.lsp.open(&open.document, root);
                                }
                                should_close = self.close_after_save.remove(&document_id)
                                    && open.document.revision == revision;
                            }
                            self.set_status(format!("Сохранено: {}", path.display()));
                            if should_close {
                                self.remove_document(document_id);
                            }
                        }
                        Err(error) => {
                            self.close_after_save.remove(&document_id);
                            self.set_error(error);
                        }
                    }
                }
                AppEvent::Compacted {
                    document_id,
                    revision,
                    result,
                } => {
                    self.compacting.remove(&document_id);
                    match result {
                        Ok(snapshot) => {
                            let applied = self
                                .documents
                                .iter_mut()
                                .find(|open| open.document.id == document_id)
                                .is_some_and(|open| {
                                    open.document.revision == revision
                                        && open.document.apply_compaction(snapshot)
                                });
                            if applied {
                                self.set_status("Фрагменты документа оптимизированы");
                            }
                        }
                        Err(error) => {
                            self.set_error(format!("Ошибка компактификации: {error}"));
                        }
                    }
                }
                AppEvent::SearchResults {
                    document_id,
                    revision,
                    query,
                    results,
                } => {
                    if self.search.document_id == Some(document_id) && self.search.query == query {
                        self.search.searching = false;
                        self.search.matches = results;
                        self.search.current = 0;
                        if let Some(open) = self
                            .documents
                            .iter_mut()
                            .find(|open| open.document.id == document_id)
                            && open.document.revision == revision
                            && let Some(position) = self.search.matches.first().copied()
                        {
                            open.editor.go_to(position);
                        }
                    }
                }
            }
        }

        while let Some(result) = self.highlighter.try_recv() {
            if let Some(open) = self
                .documents
                .iter_mut()
                .find(|open| open.document.id == result.document_id)
            {
                open.editor.accept_highlights(result);
            }
        }

        while let Some(event) = self.lsp.try_recv() {
            match event {
                LspEvent::ServerStatus { message, error } => {
                    if error {
                        self.set_error(message);
                    } else {
                        self.set_status(message);
                    }
                }
                LspEvent::Diagnostics {
                    document_id,
                    version,
                    diagnostics,
                } => {
                    if let Some(open) = self
                        .documents
                        .iter_mut()
                        .find(|open| open.document.id == document_id)
                    {
                        let current = version.is_none_or(|version| {
                            version >= 1 && (version - 1) as u64 == open.document.revision
                        });
                        if current {
                            open.editor.diagnostics = diagnostics;
                        }
                    }
                }
                LspEvent::Completion {
                    document_id,
                    revision,
                    items,
                } => {
                    if let Some(open) = self.documents.iter_mut().find(|open| {
                        open.document.id == document_id && open.document.revision == revision
                    }) {
                        open.editor.set_completions(items);
                    }
                }
                LspEvent::Definition {
                    document_id,
                    revision,
                    locations,
                } => self.handle_definition(context, document_id, revision, locations),
                LspEvent::Hover {
                    document_id,
                    revision,
                    position,
                    hover,
                } => {
                    let text = hover.as_ref().and_then(hover_documentation);
                    if let Some(open) = self.documents.iter_mut().find(|open| {
                        open.document.id == document_id && open.document.revision == revision
                    }) {
                        open.editor.set_hover(position, text);
                    }
                }
                LspEvent::References {
                    document_id,
                    revision,
                    locations,
                } => {
                    if self.documents.iter().any(|open| {
                        open.document.id == document_id && open.document.revision == revision
                    }) {
                        self.references = locations;
                        self.references_open = true;
                    }
                }
                LspEvent::Rename {
                    document_id,
                    revision,
                    edit,
                } => self.apply_rename(document_id, revision, edit),
            }
        }

        if let Some(project) = &mut self.project
            && project.poll(self.project_started)
        {
            self.explorer.sync(project);
        }
    }

    fn start_search(&mut self, context: &Context) {
        let Some(document_id) = self.search.document_id else {
            return;
        };
        let Some(open) = self
            .documents
            .iter()
            .find(|open| open.document.id == document_id)
        else {
            return;
        };
        if self.search.query.is_empty() {
            self.search.matches.clear();
            self.search.searching = false;
            return;
        }
        let snapshot = open.document.snapshot();
        let query = self.search.query.clone();
        let case_sensitive = self.search.case_sensitive;
        self.search.searching = true;
        let sender = self.event_tx.clone();
        let context = context.clone();
        thread::Builder::new()
            .name("fce-document-search".into())
            .spawn(move || {
                let results = search_snapshot(&snapshot, &query, case_sensitive);
                let _ = sender.send(AppEvent::SearchResults {
                    document_id,
                    revision: snapshot.revision,
                    query,
                    results,
                });
                context.request_repaint();
            })
            .expect("failed to start document search");
    }

    fn go_to_match(&mut self, next: bool) {
        if self.search.matches.is_empty() {
            return;
        }
        self.search.current = if next {
            (self.search.current + 1) % self.search.matches.len()
        } else if self.search.current == 0 {
            self.search.matches.len() - 1
        } else {
            self.search.current - 1
        };
        let position = self.search.matches[self.search.current];
        if let Some(open) = self.active_document_mut() {
            open.editor.go_to(position);
        }
    }

    fn show_menu(&mut self, context: &Context) {
        egui::TopBottomPanel::top("main-menu").show(context, |ui| {
            egui::MenuBar::new().ui(ui, |ui| {
                ui.menu_button("Файл", |ui| {
                    if ui.button("Открыть файл…    Cmd/Ctrl+O").clicked() {
                        ui.close();
                        self.open_file_dialog(context);
                    }
                    if ui.button("Открыть папку…    Cmd/Ctrl+Shift+O").clicked() {
                        ui.close();
                        self.open_folder_dialog(context);
                    }
                    ui.separator();
                    if ui.button("Сохранить    Cmd/Ctrl+S").clicked() {
                        ui.close();
                        self.save_active(context);
                    }
                    if ui.button("Сохранить как…    Cmd/Ctrl+Shift+S").clicked() {
                        ui.close();
                        self.save_active_as(context);
                    }
                    if ui.button("Закрыть вкладку    Cmd/Ctrl+W").clicked() {
                        ui.close();
                        if let Some(id) = self.active_document().map(|open| open.document.id) {
                            self.request_close(id);
                        }
                    }
                });
                ui.menu_button("Переход", |ui| {
                    if ui.button("Быстрый переход    Cmd/Ctrl+P").clicked() {
                        ui.close();
                        self.quick_open.open = true;
                    }
                    if ui.button("Поиск в файле    Cmd/Ctrl+F").clicked() {
                        ui.close();
                        self.search.open = true;
                        self.search.document_id =
                            self.active_document().map(|open| open.document.id);
                    }
                    ui.separator();
                    if ui.button("Автодополнение    Ctrl+Space").clicked() {
                        ui.close();
                        self.request_completion_active();
                    }
                    if ui.button("Перейти к определению    F12").clicked() {
                        ui.close();
                        self.request_definition_active();
                    }
                    if ui.button("Найти ссылки    Shift+F12").clicked() {
                        ui.close();
                        self.request_references_active();
                    }
                    if ui.button("Переименовать символ    F2").clicked() {
                        ui.close();
                        self.start_rename_active();
                    }
                });
                ui.menu_button("Вид", |ui| {
                    if ui.button("Проводник    Cmd/Ctrl+B").clicked() {
                        ui.close();
                        self.show_sidebar = !self.show_sidebar;
                    }
                    if ui.button("Проблемы    Cmd/Ctrl+Shift+M").clicked() {
                        ui.close();
                        self.diagnostics_open = !self.diagnostics_open;
                    }
                    if let Some(open) = self.active_document_mut() {
                        let mut enabled = open.editor.highlight_enabled;
                        if ui.checkbox(&mut enabled, "Подсветка синтаксиса").changed()
                        {
                            open.editor.highlight_enabled = enabled;
                        }
                    }
                });
                ui.separator();
                ui.label(RichText::new("FastCodeEditor").strong());
                if self.pending_loads > 0 {
                    ui.spinner();
                    ui.label(format!("Открытие: {}", self.pending_loads));
                }
            });
        });
    }

    fn show_sidebar(&mut self, context: &Context) {
        if !self.show_sidebar {
            return;
        }
        if let Some(project) = &self.project {
            self.explorer.sync(project);
        }
        let active_path = self
            .active_document()
            .map(|open| open.document.path.clone());
        let mut open_path = None;
        let mut open_folder = false;
        let mut toggle_directory = None;
        let mut filter_changed = false;
        egui::SidePanel::left("project-panel")
            .resizable(true)
            .default_width(285.0)
            .width_range(210.0..=480.0)
            .show(context, |ui| {
                ui.horizontal(|ui| {
                    ui.label(folder_icon(true));
                    ui.heading("EXPLORER");
                    if ui.small_button("Открыть").clicked() {
                        open_folder = true;
                    }
                });
                ui.separator();
                let Some(project) = &mut self.project else {
                    ui.label("Папка проекта не открыта");
                    return;
                };
                let root_name = project
                    .root
                    .file_name()
                    .map(|name| name.to_string_lossy())
                    .unwrap_or_else(|| project.root.to_string_lossy());
                ui.label(RichText::new(root_name).strong());
                let mut filter = project.filter().to_owned();
                if ui
                    .add(TextEdit::singleline(&mut filter).hint_text("Фильтр файлов"))
                    .changed()
                {
                    project.set_filter(filter);
                    filter_changed = true;
                }
                ui.label(
                    RichText::new(if project.scanning {
                        format!("Индексация: {} файлов", project.files.len())
                    } else {
                        format!("{} файлов", project.files.len())
                    })
                    .small()
                    .weak(),
                );
                ui.separator();
                let rows = self.explorer.rows();
                ScrollArea::vertical().show_rows(ui, 24.0, rows.len(), |ui, range| {
                    ui.spacing_mut().button_padding = egui::Vec2::new(5.0, 2.0);
                    ui.spacing_mut().item_spacing = egui::Vec2::new(4.0, 2.0);
                    for row_index in range {
                        let row = &rows[row_index];
                        ui.horizontal(|ui| {
                            ui.add_space(row.depth as f32 * 14.0);
                            if row.is_directory() {
                                ui.label(chevron_icon(row.expanded));
                                ui.label(folder_icon(row.expanded));
                                if ui.selectable_label(false, &row.name).clicked() {
                                    toggle_directory = Some(row.relative.clone());
                                }
                            } else if let Some(file_index) = row.file_index {
                                let file = &project.files[file_index];
                                ui.add_space(16.0);
                                ui.label(file_icon(&file.relative));
                                if ui
                                    .selectable_label(
                                        active_path.as_ref() == Some(&file.path),
                                        &row.name,
                                    )
                                    .on_hover_text(file.path.display().to_string())
                                    .clicked()
                                {
                                    open_path = Some(file.path.clone());
                                }
                            }
                        });
                    }
                });
            });
        if let Some(project) = &self.project {
            if filter_changed {
                self.explorer.rebuild(project);
            }
            if let Some(relative) = toggle_directory {
                self.explorer.toggle_directory(&relative, project);
            }
        }
        if open_folder {
            self.open_folder_dialog(context);
        }
        if let Some(path) = open_path {
            if is_probably_text(&path) {
                self.open_file(path, context);
            } else {
                self.set_error("Это расширение выглядит как бинарный файл");
            }
        }
    }

    fn show_search(&mut self, context: &Context) {
        if !self.search.open {
            return;
        }
        let mut close = false;
        let mut next = false;
        let mut previous = false;
        egui::TopBottomPanel::top("document-search").show(context, |ui| {
            ui.horizontal(|ui| {
                ui.label("Поиск:");
                let response = ui.add(
                    TextEdit::singleline(&mut self.search.query)
                        .hint_text("Найти в текущем файле")
                        .desired_width(280.0),
                );
                if response.changed() {
                    self.start_search(context);
                }
                if response.has_focus() && ui.input(|input| input.key_pressed(Key::Enter)) {
                    next = true;
                }
                if ui
                    .button("↑")
                    .on_hover_text("Предыдущее совпадение")
                    .clicked()
                {
                    previous = true;
                }
                if ui
                    .button("↓")
                    .on_hover_text("Следующее совпадение")
                    .clicked()
                {
                    next = true;
                }
                if ui
                    .checkbox(&mut self.search.case_sensitive, "Aa")
                    .on_hover_text("Учитывать регистр (ASCII при нечувствительном поиске)")
                    .changed()
                {
                    self.start_search(context);
                }
                if self.search.searching {
                    ui.spinner();
                }
                ui.label(if self.search.matches.is_empty() {
                    "0 совпадений".to_owned()
                } else {
                    format!(
                        "{} / {}",
                        self.search.current + 1,
                        self.search.matches.len()
                    )
                });
                if ui.button("×").clicked() {
                    close = true;
                }
            });
        });
        if next {
            self.go_to_match(true);
        }
        if previous {
            self.go_to_match(false);
        }
        if close {
            self.search.open = false;
        }
    }

    fn show_tabs(&mut self, context: &Context) {
        if self.documents.is_empty() {
            return;
        }
        let mut activate = None;
        let mut close = None;
        egui::TopBottomPanel::top("document-tabs").show(context, |ui| {
            ui.horizontal_wrapped(|ui| {
                for (index, open) in self.documents.iter().enumerate() {
                    let dirty = if open.document.dirty { " •" } else { "" };
                    let title = format!("{}{dirty}", open.document.file_name());
                    if ui
                        .selectable_label(self.active == Some(index), title)
                        .clicked()
                    {
                        activate = Some(index);
                    }
                    if ui.small_button("×").on_hover_text("Закрыть").clicked() {
                        close = Some(open.document.id);
                    }
                    ui.separator();
                }
            });
        });
        if let Some(index) = activate {
            self.active = Some(index);
            self.search.document_id = self.active_document().map(|open| open.document.id);
            self.search.matches.clear();
        }
        if let Some(id) = close {
            self.request_close(id);
        }
    }

    fn show_references(&mut self, context: &Context) {
        if !self.references_open {
            return;
        }
        let mut navigate = None;
        let mut close = false;
        egui::TopBottomPanel::bottom("references-panel")
            .resizable(true)
            .default_height(190.0)
            .min_height(100.0)
            .show(context, |ui| {
                ui.horizontal(|ui| {
                    ui.heading("Ссылки");
                    ui.label(RichText::new(format!("{} найдено", self.references.len())).weak());
                    if ui.small_button("×").on_hover_text("Закрыть").clicked() {
                        close = true;
                    }
                });
                ui.separator();
                if self.references.is_empty() {
                    ui.label("Ссылки не найдены");
                    return;
                }
                ScrollArea::vertical().show_rows(ui, 24.0, self.references.len(), |ui, range| {
                    for index in range {
                        let location = &self.references[index];
                        let path = location
                            .uri
                            .to_file_path()
                            .unwrap_or_else(|_| PathBuf::from(location.uri.as_str()));
                        let preview = self
                            .documents
                            .iter()
                            .find(|open| open.document.path == path)
                            .map(|open| {
                                open.document
                                    .line(location.range.start.line as usize)
                                    .trim()
                                    .chars()
                                    .take(100)
                                    .collect::<String>()
                            })
                            .unwrap_or_default();
                        let text = format!(
                            "{}:{}:{}  {}",
                            path.display(),
                            location.range.start.line + 1,
                            location.range.start.character + 1,
                            preview
                        );
                        if ui.selectable_label(false, text).clicked() {
                            navigate = Some(location.clone());
                        }
                    }
                });
            });
        if let Some(location) = navigate {
            self.navigate_to_location(context, &location);
        }
        if close {
            self.references_open = false;
        }
    }

    fn show_rename(&mut self, context: &Context) {
        if !self.rename.open {
            return;
        }
        let mut open = true;
        let mut submit = false;
        let mut cancel = false;
        egui::Window::new("Переименование символа")
            .open(&mut open)
            .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
            .collapsible(false)
            .resizable(false)
            .default_width(420.0)
            .show(context, |ui| {
                ui.label("Новое имя символа:");
                let response = ui.add(
                    TextEdit::singleline(&mut self.rename.new_name)
                        .desired_width(380.0)
                        .lock_focus(true),
                );
                response.request_focus();
                if response.has_focus() && ui.input(|input| input.key_pressed(Key::Enter)) {
                    submit = true;
                }
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    if ui.button("Переименовать").clicked() {
                        submit = true;
                    }
                    if ui.button("Отмена").clicked() {
                        cancel = true;
                    }
                });
            });
        self.rename.open = open;
        if submit {
            self.submit_rename();
        } else if cancel || !open {
            self.rename.open = false;
            self.rename.revisions.clear();
        }
    }

    fn show_diagnostics(&mut self, context: &Context) {
        if !self.diagnostics_open {
            return;
        }
        let Some(open) = self.active_document() else {
            return;
        };
        let (errors, warnings) = diagnostic_counts(&open.editor.diagnostics);
        let mut navigate = None;
        let mut close = false;
        egui::TopBottomPanel::bottom("diagnostics-panel")
            .resizable(true)
            .default_height(170.0)
            .min_height(90.0)
            .show(context, |ui| {
                ui.horizontal(|ui| {
                    ui.heading("Проблемы");
                    ui.label(
                        RichText::new(format!("{errors} ошибок, {warnings} предупреждений")).weak(),
                    );
                    if ui.small_button("×").on_hover_text("Закрыть").clicked() {
                        close = true;
                    }
                });
                ui.separator();
                if open.editor.diagnostics.is_empty() {
                    ui.label("Языковой сервер не сообщил о проблемах");
                    return;
                }
                ScrollArea::vertical().show_rows(
                    ui,
                    24.0,
                    open.editor.diagnostics.len(),
                    |ui, range| {
                        for index in range {
                            let diagnostic = &open.editor.diagnostics[index];
                            let (icon, color) = diagnostic_icon(diagnostic);
                            let message: String = diagnostic
                                .message
                                .lines()
                                .next()
                                .unwrap_or_default()
                                .chars()
                                .take(160)
                                .collect();
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(icon).color(color));
                                let location = format!(
                                    "{}:{}",
                                    diagnostic.range.start.line + 1,
                                    diagnostic.range.start.character + 1
                                );
                                if ui
                                    .selectable_label(false, format!("{location}  {message}"))
                                    .on_hover_text(&diagnostic.message)
                                    .clicked()
                                {
                                    navigate = Some(diagnostic.range.start);
                                }
                            });
                        }
                    },
                );
            });
        if let Some(position) = navigate
            && let Some(open) = self.active_document_mut()
        {
            let position = open.document.from_lsp_position(position);
            open.editor.go_to(position);
        }
        if close {
            self.diagnostics_open = false;
        }
    }

    fn show_status(&mut self, context: &Context) {
        let mut toggle_diagnostics = false;
        egui::TopBottomPanel::bottom("status-bar").show(context, |ui| {
            ui.horizontal(|ui| {
                if let Some(open) = self.active_document() {
                    let document = &open.document;
                    ui.label(
                        open.editor
                            .language()
                            .unwrap_or_else(|| language_label(&document.path)),
                    );
                    ui.separator();
                    let (errors, warnings) = diagnostic_counts(&open.editor.diagnostics);
                    if ui
                        .selectable_label(
                            self.diagnostics_open,
                            format!("Ошибки: {errors}  Предупреждения: {warnings}"),
                        )
                        .on_hover_text("Показать панель проблем (Cmd/Ctrl+Shift+M)")
                        .clicked()
                    {
                        toggle_diagnostics = true;
                    }
                    ui.separator();
                    ui.label(format!(
                        "Строка {}, колонка {}",
                        open.editor.cursor.line + 1,
                        open.editor.cursor.column + 1
                    ));
                    ui.separator();
                    ui.label(format!("{} строк", document.len_lines()));
                    ui.separator();
                    ui.label(format_size(document.byte_len));
                    ui.separator();
                    ui.label(RichText::new(document.backend_label()).color(crate::theme::CYAN));
                    if self.saving.contains(&document.id) {
                        ui.separator();
                        ui.spinner();
                        ui.label("Сохранение");
                    }
                    if self.compacting.contains(&document.id) {
                        ui.separator();
                        ui.spinner();
                        ui.label("Оптимизация фрагментов");
                    }
                }
                if let Some(status) = &self.status {
                    ui.separator();
                    ui.label(RichText::new(&status.text).color(if status.error {
                        crate::theme::ERROR
                    } else {
                        crate::theme::MUTED
                    }));
                }
            });
        });
        if toggle_diagnostics {
            self.diagnostics_open = !self.diagnostics_open;
        }
    }

    fn show_quick_open(&mut self, context: &Context) {
        if !self.quick_open.open {
            return;
        }
        let mut open = self.quick_open.open;
        let mut selected_path = None;
        let mut refresh = self.quick_open.query.is_empty()
            && self.quick_open.matches.is_empty()
            && self
                .project
                .as_ref()
                .is_some_and(|project| !project.files.is_empty());
        egui::Window::new("Быстрый переход")
            .open(&mut open)
            .anchor(Align2::CENTER_TOP, [0.0, 90.0])
            .collapsible(false)
            .resizable(false)
            .default_width(620.0)
            .show(context, |ui| {
                let response = ui.add(
                    TextEdit::singleline(&mut self.quick_open.query)
                        .hint_text("Имя файла или путь…")
                        .desired_width(580.0)
                        .lock_focus(true),
                );
                response.request_focus();
                if response.changed() {
                    refresh = true;
                    self.quick_open.selected = 0;
                }
                if response.has_focus() {
                    if ui.input(|input| input.key_pressed(Key::ArrowDown)) {
                        self.quick_open.selected = (self.quick_open.selected + 1)
                            .min(self.quick_open.matches.len().saturating_sub(1));
                    }
                    if ui.input(|input| input.key_pressed(Key::ArrowUp)) {
                        self.quick_open.selected = self.quick_open.selected.saturating_sub(1);
                    }
                    if ui.input(|input| input.key_pressed(Key::Enter))
                        && let Some(project) = &self.project
                        && let Some(index) = self.quick_open.matches.get(self.quick_open.selected)
                    {
                        selected_path = Some(project.files[*index].path.clone());
                    }
                }
                ui.separator();
                if let Some(project) = &self.project {
                    ScrollArea::vertical().max_height(390.0).show(ui, |ui| {
                        for (row, file_index) in self.quick_open.matches.iter().copied().enumerate()
                        {
                            let file = &project.files[file_index];
                            if ui
                                .selectable_label(row == self.quick_open.selected, &file.relative)
                                .clicked()
                            {
                                selected_path = Some(file.path.clone());
                            }
                        }
                    });
                    if project.files.is_empty() {
                        ui.label(if project.scanning {
                            "Индексация проекта…"
                        } else {
                            "Файлы не найдены"
                        });
                    }
                } else {
                    ui.label("Сначала откройте папку проекта");
                }
            });
        if refresh {
            self.quick_open.matches = self
                .project
                .as_ref()
                .map(|project| project.fuzzy_matches(&self.quick_open.query, 100))
                .unwrap_or_default();
        }
        if selected_path.is_some() {
            open = false;
        }
        self.quick_open.open = open;
        if let Some(path) = selected_path {
            self.open_file(path, context);
        }
    }

    fn show_close_confirmation(&mut self, context: &Context) {
        let Some(document_id) = self.close_requested else {
            return;
        };
        let Some(open) = self
            .documents
            .iter()
            .find(|open| open.document.id == document_id)
        else {
            self.close_requested = None;
            return;
        };
        let file_name = open.document.file_name().into_owned();
        let mut save = false;
        let mut discard = false;
        let mut cancel = false;
        egui::Window::new("Несохранённые изменения")
            .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
            .collapsible(false)
            .resizable(false)
            .show(context, |ui| {
                ui.label(format!("Сохранить изменения в {file_name}?"));
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    if ui.button("Сохранить").clicked() {
                        save = true;
                    }
                    if ui.button("Не сохранять").clicked() {
                        discard = true;
                    }
                    if ui.button("Отмена").clicked() {
                        cancel = true;
                    }
                });
            });
        if save {
            let path = self
                .documents
                .iter()
                .find(|open| open.document.id == document_id)
                .map(|open| open.document.path.clone());
            if let Some(path) = path {
                self.close_after_save.insert(document_id);
                self.save_document(document_id, path, false, context);
            }
            self.close_requested = None;
        } else if discard {
            self.remove_document(document_id);
            self.close_requested = None;
        } else if cancel {
            self.close_requested = None;
        }
    }
}

impl eframe::App for FastCodeEditor {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0; 4]
    }

    fn update(&mut self, context: &Context, _frame: &mut eframe::Frame) {
        if let Some(path) = self.initial_path.take() {
            self.open_path(path, context);
        }
        self.poll_events(context);
        self.handle_shortcuts(context);
        self.show_menu(context);
        self.show_search(context);
        self.show_sidebar(context);
        self.show_references(context);
        self.show_diagnostics(context);
        self.show_status(context);
        self.show_tabs(context);

        let mut edited_document = None;
        let mut hover_request = None;
        egui::CentralPanel::default().show(context, |ui| {
            if let Some(active) = self.active {
                let open = &mut self.documents[active];
                let revision = open.document.revision;
                editor::show(ui, &mut open.document, &mut open.editor, &self.highlighter);
                if open.document.revision != revision {
                    edited_document = Some(open.document.id);
                }
                if let Some(position) = open.editor.take_hover_request() {
                    hover_request = Some((open.document.id, position));
                }
            } else {
                ui.vertical_centered(|ui| {
                    ui.add_space(120.0);
                    ui.heading(RichText::new("FastCodeEditor").size(34.0));
                    ui.add_space(8.0);
                    ui.label(
                        RichText::new("Большие проекты. Огромные файлы. Только видимые строки.")
                            .weak(),
                    );
                    ui.add_space(28.0);
                    if ui.button("Открыть файл    Cmd/Ctrl+O").clicked() {
                        self.open_file_dialog(context);
                    }
                    if ui.button("Открыть проект    Cmd/Ctrl+Shift+O").clicked() {
                        self.open_folder_dialog(context);
                    }
                    ui.add_space(24.0);
                    ui.label("Файлы от 64 МиБ редактируются через piece table поверх mmap");
                });
            }
        });

        if let Some((document_id, position)) = hover_request
            && let Some(open) = self
                .documents
                .iter()
                .find(|open| open.document.id == document_id)
        {
            self.lsp.request_hover(&open.document, position);
        }
        if let Some(document_id) = edited_document {
            self.sync_lsp_document(document_id);
        }
        self.show_quick_open(context);
        self.show_rename(context);
        self.show_close_confirmation(context);
        let compaction_pending = self.maybe_start_compaction(context);
        if self.pending_loads > 0
            || !self.saving.is_empty()
            || !self.compacting.is_empty()
            || self.search.searching
            || self
                .project
                .as_ref()
                .is_some_and(|project| project.scanning)
        {
            context.request_repaint_after(Duration::from_millis(32));
        } else if compaction_pending {
            context.request_repaint_after(Duration::from_millis(250));
        }
    }
}

fn consume(context: &Context, key: Key, modifiers: Modifiers) -> bool {
    context.input_mut(|input| input.consume_key(modifiers, key))
}

fn search_snapshot(
    snapshot: &DocumentSnapshot,
    query: &str,
    case_sensitive: bool,
) -> Vec<Position> {
    const MAX_RESULTS: usize = 10_000;
    let folded_query = query.to_ascii_lowercase();
    let mut results = Vec::new();
    for line_index in 0..snapshot.len_lines() {
        let line = snapshot.line(line_index);
        let searched;
        let haystack = if case_sensitive {
            line.as_ref()
        } else {
            searched = line.to_ascii_lowercase();
            &searched
        };
        let needle = if case_sensitive { query } else { &folded_query };
        for (byte, _) in haystack.match_indices(needle) {
            let column = line[..byte].chars().count();
            results.push(Position::new(line_index, column));
            if results.len() == MAX_RESULTS {
                return results;
            }
        }
    }
    results
}

fn identifier_at(document: &Document, position: Position) -> String {
    let characters: Vec<char> = document.line(position.line).chars().collect();
    let cursor = position.column.min(characters.len());
    let is_identifier = |character: char| character.is_alphanumeric() || character == '_';
    let mut start = cursor;
    while start > 0 && is_identifier(characters[start - 1]) {
        start -= 1;
    }
    let mut end = cursor;
    while end < characters.len() && is_identifier(characters[end]) {
        end += 1;
    }
    characters[start..end].iter().collect()
}

fn workspace_text_edits(
    edit: lsp_types::WorkspaceEdit,
) -> Result<HashMap<url::Url, Vec<lsp_types::TextEdit>>, String> {
    fn append_document_edit(
        result: &mut HashMap<url::Url, Vec<lsp_types::TextEdit>>,
        edit: lsp_types::TextDocumentEdit,
    ) {
        let edits = edit.edits.into_iter().map(|edit| match edit {
            lsp_types::OneOf::Left(edit) => edit,
            lsp_types::OneOf::Right(edit) => edit.text_edit,
        });
        result
            .entry(edit.text_document.uri)
            .or_default()
            .extend(edits);
    }

    let mut result = HashMap::new();
    if let Some(changes) = edit.changes {
        for (uri, edits) in changes {
            result.entry(uri).or_insert_with(Vec::new).extend(edits);
        }
    }
    if let Some(changes) = edit.document_changes {
        match changes {
            lsp_types::DocumentChanges::Edits(edits) => {
                for edit in edits {
                    append_document_edit(&mut result, edit);
                }
            }
            lsp_types::DocumentChanges::Operations(operations) => {
                for operation in operations {
                    if let lsp_types::DocumentChangeOperation::Edit(edit) = operation {
                        append_document_edit(&mut result, edit);
                    } else {
                        return Err(
                            "WorkspaceEdit с файловыми операциями пока не поддерживается"
                                .to_owned(),
                        );
                    }
                }
            }
        }
    }
    Ok(result)
}

fn apply_text_edits(document: &mut Document, mut edits: Vec<lsp_types::TextEdit>) -> usize {
    edits.sort_by(|left, right| {
        right
            .range
            .start
            .line
            .cmp(&left.range.start.line)
            .then_with(|| right.range.start.character.cmp(&left.range.start.character))
    });
    let mut applied = 0;
    for edit in edits {
        let start = document.from_lsp_position(edit.range.start);
        let end = document.from_lsp_position(edit.range.end);
        if start <= end && document.replace(start, end, &edit.new_text).is_some() {
            applied += 1;
        }
    }
    applied
}

fn hover_documentation(hover: &lsp_types::Hover) -> Option<String> {
    fn marked_text(value: &lsp_types::MarkedString) -> &str {
        match value {
            lsp_types::MarkedString::String(text) => text,
            lsp_types::MarkedString::LanguageString(code) => &code.value,
        }
    }

    let sections: Vec<&str> = match &hover.contents {
        lsp_types::HoverContents::Markup(content) => vec![content.value.as_str()],
        lsp_types::HoverContents::Scalar(value) => vec![marked_text(value)],
        lsp_types::HoverContents::Array(values) => values.iter().map(marked_text).collect(),
    };
    let text = sections
        .into_iter()
        .filter(|section| !section.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    (!text.trim().is_empty()).then_some(text)
}

fn diagnostic_counts(diagnostics: &[lsp_types::Diagnostic]) -> (usize, usize) {
    let errors = diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic
                .severity
                .is_none_or(|severity| severity == lsp_types::DiagnosticSeverity::ERROR)
        })
        .count();
    let warnings = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.severity == Some(lsp_types::DiagnosticSeverity::WARNING))
        .count();
    (errors, warnings)
}

fn diagnostic_icon(diagnostic: &lsp_types::Diagnostic) -> (&'static str, egui::Color32) {
    match diagnostic.severity {
        Some(severity) if severity == lsp_types::DiagnosticSeverity::WARNING => (
            egui_material_icons::icons::ICON_WARNING,
            crate::theme::WARNING,
        ),
        Some(severity)
            if severity == lsp_types::DiagnosticSeverity::INFORMATION
                || severity == lsp_types::DiagnosticSeverity::HINT =>
        {
            (egui_material_icons::icons::ICON_INFO, crate::theme::CYAN)
        }
        _ => (egui_material_icons::icons::ICON_ERROR, crate::theme::ERROR),
    }
}

fn format_size(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;
    if bytes as f64 >= GIB {
        format!("{:.2} ГиБ", bytes as f64 / GIB)
    } else if bytes as f64 >= MIB {
        format!("{:.1} МиБ", bytes as f64 / MIB)
    } else if bytes as f64 >= KIB {
        format!("{:.1} КиБ", bytes as f64 / KIB)
    } else {
        format!("{bytes} Б")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_large_file_sizes() {
        assert_eq!(format_size(512), "512 Б");
        assert_eq!(format_size(1024), "1.0 КиБ");
        assert_eq!(format_size(1024 * 1024), "1.0 МиБ");
    }

    #[test]
    fn workspace_rename_applies_edits_without_shifting_ranges() {
        use std::io::Write;

        let mut file = tempfile::NamedTempFile::new().unwrap();
        write!(file, "fn target() {{}}\nfn main() {{ target(); }}").unwrap();
        file.flush().unwrap();
        let mut document = Document::open(file.path(), 78).unwrap();
        let uri = url::Url::from_file_path(file.path()).unwrap();
        let edits = vec![
            lsp_types::TextEdit::new(
                lsp_types::Range::new(
                    lsp_types::Position::new(0, 3),
                    lsp_types::Position::new(0, 9),
                ),
                "renamed".into(),
            ),
            lsp_types::TextEdit::new(
                lsp_types::Range::new(
                    lsp_types::Position::new(1, 12),
                    lsp_types::Position::new(1, 18),
                ),
                "renamed".into(),
            ),
        ];
        let edit = lsp_types::WorkspaceEdit {
            changes: Some(HashMap::from([(uri, edits)])),
            ..lsp_types::WorkspaceEdit::default()
        };
        let changes = workspace_text_edits(edit).unwrap();
        let edits = changes.into_values().next().unwrap();

        assert_eq!(apply_text_edits(&mut document, edits), 2);
        assert_eq!(document.line(0), "fn renamed() {}");
        assert_eq!(document.line(1), "fn main() { renamed(); }");
    }

    #[test]
    fn search_honors_ascii_case_mode() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        use std::io::Write;
        write!(file, "Rust\nrust\nRUST\nother").unwrap();
        let document = Document::open(file.path(), 77).unwrap();
        let snapshot = document.snapshot();

        let insensitive = search_snapshot(&snapshot, "rust", false);
        let sensitive = search_snapshot(&snapshot, "rust", true);
        assert_eq!(insensitive.len(), 3);
        assert_eq!(sensitive, vec![Position::new(1, 0)]);
    }
}
