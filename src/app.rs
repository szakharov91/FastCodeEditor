use std::{
    collections::HashSet,
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
    highlighter::{HighlightWorker, language_label},
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
    event_tx: Sender<AppEvent>,
    event_rx: Receiver<AppEvent>,
    highlighter: HighlightWorker,
    pending_loads: usize,
    saving: HashSet<u64>,
    compacting: HashSet<u64>,
    close_after_save: HashSet<u64>,
    close_requested: Option<u64>,
    show_sidebar: bool,
    search: SearchState,
    quick_open: QuickOpenState,
    status: Option<StatusMessage>,
    initial_path: Option<PathBuf>,
}

impl FastCodeEditor {
    pub fn new(creation_context: &eframe::CreationContext<'_>) -> Self {
        let (event_tx, event_rx) = unbounded();
        crate::theme::apply(&creation_context.egui_ctx);

        Self {
            documents: Vec::new(),
            active: None,
            next_document_id: 1,
            project: None,
            project_started: Instant::now(),
            event_tx,
            event_rx,
            highlighter: HighlightWorker::new(&creation_context.egui_ctx),
            pending_loads: 0,
            saving: HashSet::new(),
            compacting: HashSet::new(),
            close_after_save: HashSet::new(),
            close_requested: None,
            show_sidebar: true,
            search: SearchState::default(),
            quick_open: QuickOpenState::default(),
            status: None,
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
        self.documents.remove(index);
        self.active = match (self.active, self.documents.len()) {
            (_, 0) => None,
            (Some(active), len) if active >= len => Some(len - 1),
            (Some(active), _) if active > index => Some(active - 1),
            (active, _) => active,
        };
    }

    fn handle_shortcuts(&mut self, context: &Context) {
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

    fn poll_events(&mut self) {
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
                            self.documents.push(OpenDocument {
                                document,
                                editor: EditorState::default(),
                            });
                            self.active = Some(self.documents.len() - 1);
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
                            if let Some(open) = self
                                .documents
                                .iter_mut()
                                .find(|open| open.document.id == document_id)
                            {
                                if open.document.revision == revision {
                                    open.document.mark_saved(byte_len);
                                }
                                if update_path {
                                    open.document.path = path.clone();
                                    open.editor.invalidate_highlighting();
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

        if let Some(project) = &mut self.project {
            project.poll(self.project_started);
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
                });
                ui.menu_button("Вид", |ui| {
                    if ui.button("Проводник    Cmd/Ctrl+B").clicked() {
                        ui.close();
                        self.show_sidebar = !self.show_sidebar;
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
        let mut open_path = None;
        let mut open_folder = false;
        egui::SidePanel::left("project-panel")
            .resizable(true)
            .default_width(265.0)
            .width_range(180.0..=480.0)
            .show(context, |ui| {
                ui.horizontal(|ui| {
                    ui.heading("Проект");
                    if ui.small_button("Открыть").clicked() {
                        open_folder = true;
                    }
                });
                ui.separator();
                let Some(project) = &mut self.project else {
                    ui.label("Папка проекта не открыта");
                    return;
                };
                ui.label(RichText::new(project.root.display().to_string()).small());
                let mut filter = project.filter().to_owned();
                if ui
                    .add(TextEdit::singleline(&mut filter).hint_text("Фильтр файлов"))
                    .changed()
                {
                    project.set_filter(filter);
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
                let visible = project.visible_indices();
                ScrollArea::vertical().show_rows(ui, 22.0, visible.len(), |ui, range| {
                    for row in range {
                        let file_index = visible[row];
                        let file = &project.files[file_index];
                        if ui
                            .selectable_label(false, &file.relative)
                            .on_hover_text(file.path.display().to_string())
                            .clicked()
                        {
                            open_path = Some(file.path.clone());
                        }
                    }
                });
            });
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

    fn show_status(&self, context: &Context) {
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
        self.poll_events();
        self.handle_shortcuts(context);
        self.show_menu(context);
        self.show_search(context);
        self.show_sidebar(context);
        self.show_status(context);
        self.show_tabs(context);

        egui::CentralPanel::default().show(context, |ui| {
            if let Some(active) = self.active {
                let open = &mut self.documents[active];
                editor::show(ui, &mut open.document, &mut open.editor, &self.highlighter);
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

        self.show_quick_open(context);
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
