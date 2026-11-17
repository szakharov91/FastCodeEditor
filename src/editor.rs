use std::{
    ops::Range,
    time::{Duration, Instant},
};

use eframe::egui::{
    self, Align, Align2, Color32, FontId, Key, Modifiers, Painter, Pos2, Rect, ScrollArea, Sense,
    Stroke, TextFormat, Ui, Vec2, text::LayoutJob,
};

use lsp_types::{CompletionItem, CompletionTextEdit, Diagnostic, DiagnosticSeverity};

use crate::{
    document::{Document, MAX_HIGHLIGHT_LINE_BYTES, Position, advance},
    highlighter::{HighlightResult, HighlightWorker, HighlightedLine},
    theme::{ACTIVE_LINE, CURSOR, EDITOR_MUTED, EDITOR_TEXT, SELECTION},
};

const FONT_SIZE: f32 = 14.0;
const ROW_HEIGHT: f32 = 21.0;
const TEXT_COLOR: Color32 = EDITOR_TEXT;
const MUTED_COLOR: Color32 = EDITOR_MUTED;
const CURSOR_COLOR: Color32 = CURSOR;
const SELECTION_COLOR: Color32 = SELECTION;
const ACTIVE_LINE_COLOR: Color32 = ACTIVE_LINE;

#[derive(Clone, Debug, Eq, PartialEq)]
struct HighlightKey {
    revision: u64,
    range: Range<usize>,
}

pub struct EditorState {
    pub cursor: Position,
    pub anchor: Option<Position>,
    pub visible_range: Range<usize>,
    pub scroll_to_cursor: bool,
    pub highlight_enabled: bool,
    pub diagnostics: Vec<Diagnostic>,
    highlights: Option<HighlightResult>,
    highlight_key: Option<HighlightKey>,
    preferred_column: Option<usize>,
    request_focus: bool,
    completions: Vec<CompletionItem>,
    completion_open: bool,
    completion_selected: usize,
    hover_position: Option<Position>,
    hover_since: Option<Instant>,
    hover_screen_position: Option<Pos2>,
    hover_sent: bool,
    hover_text: Option<String>,
    pending_hover_request: Option<Position>,
}

impl Default for EditorState {
    fn default() -> Self {
        Self {
            cursor: Position::default(),
            anchor: None,
            visible_range: 0..0,
            scroll_to_cursor: false,
            highlight_enabled: true,
            diagnostics: Vec::new(),
            highlights: None,
            highlight_key: None,
            preferred_column: None,
            request_focus: true,
            completions: Vec::new(),
            completion_open: false,
            completion_selected: 0,
            hover_position: None,
            hover_since: None,
            hover_screen_position: None,
            hover_sent: false,
            hover_text: None,
            pending_hover_request: None,
        }
    }
}

impl EditorState {
    pub fn selection(&self) -> Option<(Position, Position)> {
        let anchor = self.anchor?;
        if anchor == self.cursor {
            None
        } else if anchor < self.cursor {
            Some((anchor, self.cursor))
        } else {
            Some((self.cursor, anchor))
        }
    }

    pub fn go_to(&mut self, position: Position) {
        self.cursor = position;
        self.anchor = None;
        self.preferred_column = None;
        self.scroll_to_cursor = true;
        self.request_focus = true;
        self.completion_open = false;
    }

    pub fn set_completions(&mut self, items: Vec<CompletionItem>) {
        self.completion_open = !items.is_empty();
        self.completion_selected = 0;
        self.completions = items;
    }

    pub fn update_hover(&mut self, pointer: Option<(Position, Pos2)>, context: &egui::Context) {
        let Some((position, screen_position)) = pointer else {
            self.hover_position = None;
            self.hover_since = None;
            self.hover_sent = false;
            self.hover_text = None;
            self.pending_hover_request = None;
            return;
        };
        if self.hover_position != Some(position) {
            self.hover_position = Some(position);
            self.hover_since = Some(Instant::now());
            self.hover_screen_position = Some(screen_position);
            self.hover_sent = false;
            self.hover_text = None;
            self.pending_hover_request = None;
        }
        if !self.hover_sent {
            if self
                .hover_since
                .is_some_and(|started| started.elapsed() >= Duration::from_millis(450))
            {
                self.pending_hover_request = Some(position);
                self.hover_sent = true;
            } else {
                context.request_repaint_after(Duration::from_millis(100));
            }
        }
    }

    pub fn take_hover_request(&mut self) -> Option<Position> {
        self.pending_hover_request.take()
    }

    pub fn set_hover(&mut self, position: Position, text: Option<String>) {
        if self.hover_position == Some(position) {
            self.hover_text = text;
        }
    }

    fn handle_completion_key(
        &mut self,
        document: &mut Document,
        key: Key,
        modifiers: Modifiers,
    ) -> bool {
        if !self.completion_open || modifiers.command {
            return false;
        }
        match key {
            Key::ArrowDown => {
                self.completion_selected =
                    (self.completion_selected + 1) % self.completions.len().max(1);
            }
            Key::ArrowUp => {
                self.completion_selected = self.completion_selected.saturating_sub(1);
            }
            Key::Enter | Key::Tab => self.apply_completion(document),
            Key::Escape => self.completion_open = false,
            _ => return false,
        }
        true
    }

    fn apply_completion(&mut self, document: &mut Document) {
        let Some(item) = self.completions.get(self.completion_selected).cloned() else {
            self.completion_open = false;
            return;
        };
        let inserted = match item.text_edit {
            Some(CompletionTextEdit::Edit(edit)) => edit.new_text,
            Some(CompletionTextEdit::InsertAndReplace(edit)) => edit.new_text,
            None => item.insert_text.unwrap_or(item.label),
        };
        let prefix = document.line_slice_chars(self.cursor.line, 0, self.cursor.column);
        let start_column = self.cursor.column
            - prefix
                .chars()
                .rev()
                .take_while(|character| character.is_alphanumeric() || *character == '_')
                .count();
        let start = Position::new(self.cursor.line, start_column);
        if document.replace(start, self.cursor, &inserted).is_some() {
            self.cursor = advance(start, &inserted);
            self.anchor = None;
            self.scroll_to_cursor = true;
        }
        self.completion_open = false;
        self.request_focus = true;
    }

    pub fn language(&self) -> Option<&str> {
        self.highlights
            .as_ref()
            .map(|result| result.language.as_str())
    }

    pub fn invalidate_highlighting(&mut self) {
        self.highlights = None;
        self.highlight_key = None;
    }

    pub fn accept_highlights(&mut self, result: HighlightResult) -> bool {
        if self.highlight_key.as_ref().is_some_and(|key| {
            key.revision == result.revision
                && key.range.start == result.start_line
                && key.range.len() == result.lines.len()
        }) {
            self.highlights = Some(result);
            true
        } else {
            false
        }
    }

    fn move_to(&mut self, document: &Document, position: Position, selecting: bool) {
        let position = document.clamp_position(position);
        if selecting {
            self.anchor.get_or_insert(self.cursor);
        } else {
            self.anchor = None;
        }
        self.cursor = position;
        self.scroll_to_cursor = true;
        self.completion_open = false;
    }

    fn insert(&mut self, document: &mut Document, text: &str) {
        if document.read_only || text.is_empty() {
            return;
        }
        self.completion_open = false;
        let (start, end) = self.selection().unwrap_or((self.cursor, self.cursor));
        if document.replace(start, end, text).is_some() {
            self.cursor = advance(start, text);
            self.anchor = None;
            self.preferred_column = None;
            self.scroll_to_cursor = true;
        }
    }

    fn delete_backward(&mut self, document: &mut Document) {
        if document.read_only {
            return;
        }
        let range = self.selection().or_else(|| {
            if self.cursor.column > 0 {
                Some((
                    Position::new(self.cursor.line, self.cursor.column - 1),
                    self.cursor,
                ))
            } else if self.cursor.line > 0 {
                Some((
                    Position::new(
                        self.cursor.line - 1,
                        document.line_len_chars(self.cursor.line - 1),
                    ),
                    self.cursor,
                ))
            } else {
                None
            }
        });
        if let Some((start, end)) = range
            && document.replace(start, end, "").is_some()
        {
            self.cursor = start;
            self.anchor = None;
            self.scroll_to_cursor = true;
        }
    }

    fn delete_forward(&mut self, document: &mut Document) {
        if document.read_only {
            return;
        }
        let range = self.selection().or_else(|| {
            let line_len = document.line_len_chars(self.cursor.line);
            if self.cursor.column < line_len {
                Some((
                    self.cursor,
                    Position::new(self.cursor.line, self.cursor.column + 1),
                ))
            } else if self.cursor.line + 1 < document.len_lines() {
                Some((self.cursor, Position::new(self.cursor.line + 1, 0)))
            } else {
                None
            }
        });
        if let Some((start, end)) = range
            && document.replace(start, end, "").is_some()
        {
            self.cursor = start;
            self.anchor = None;
            self.scroll_to_cursor = true;
        }
    }

    fn copy(&self, document: &Document, context: &egui::Context) {
        let text = if let Some((start, end)) = self.selection() {
            document.text_range(start, end)
        } else {
            document.line(self.cursor.line).into_owned()
        };
        context.copy_text(text);
    }

    fn handle_input(&mut self, document: &mut Document, ui: &Ui) {
        let events = ui.input(|input| input.events.clone());
        for event in events {
            match event {
                egui::Event::Text(text) => self.insert(document, &text),
                egui::Event::Paste(text) => self.insert(document, &text),
                egui::Event::Copy => self.copy(document, ui.ctx()),
                egui::Event::Cut => {
                    self.copy(document, ui.ctx());
                    if let Some((start, end)) = self.selection()
                        && document.replace(start, end, "").is_some()
                    {
                        self.cursor = start;
                        self.anchor = None;
                    }
                }
                egui::Event::Key {
                    key,
                    pressed: true,
                    modifiers,
                    ..
                } if !self.handle_completion_key(document, key, modifiers) => {
                    self.handle_key(document, key, modifiers);
                }
                _ => {}
            }
        }
    }

    fn handle_key(&mut self, document: &mut Document, key: Key, modifiers: Modifiers) {
        let selecting = modifiers.shift;
        match key {
            Key::ArrowLeft => {
                let position = if modifiers.command {
                    Position::new(self.cursor.line, 0)
                } else if let Some((start, _)) = self.selection().filter(|_| !selecting) {
                    start
                } else if self.cursor.column > 0 {
                    Position::new(self.cursor.line, self.cursor.column - 1)
                } else if self.cursor.line > 0 {
                    Position::new(
                        self.cursor.line - 1,
                        document.line_len_chars(self.cursor.line - 1),
                    )
                } else {
                    self.cursor
                };
                self.move_to(document, position, selecting);
            }
            Key::ArrowRight => {
                let position = if modifiers.command {
                    Position::new(self.cursor.line, document.line_len_chars(self.cursor.line))
                } else if let Some((_, end)) = self.selection().filter(|_| !selecting) {
                    end
                } else if self.cursor.column < document.line_len_chars(self.cursor.line) {
                    Position::new(self.cursor.line, self.cursor.column + 1)
                } else if self.cursor.line + 1 < document.len_lines() {
                    Position::new(self.cursor.line + 1, 0)
                } else {
                    self.cursor
                };
                self.move_to(document, position, selecting);
            }
            Key::ArrowUp | Key::ArrowDown => {
                let up = key == Key::ArrowUp;
                let position = if modifiers.command {
                    if up {
                        Position::default()
                    } else {
                        document.end_position()
                    }
                } else {
                    let column = self.preferred_column.unwrap_or(self.cursor.column);
                    self.preferred_column = Some(column);
                    let line = if up {
                        self.cursor.line.saturating_sub(1)
                    } else {
                        (self.cursor.line + 1).min(document.len_lines().saturating_sub(1))
                    };
                    Position::new(line, column)
                };
                self.move_to(document, position, selecting);
            }
            Key::PageUp | Key::PageDown => {
                let distance = self.visible_range.len().saturating_sub(2).max(1);
                let line = if key == Key::PageUp {
                    self.cursor.line.saturating_sub(distance)
                } else {
                    (self.cursor.line + distance).min(document.len_lines().saturating_sub(1))
                };
                self.move_to(document, Position::new(line, self.cursor.column), selecting);
            }
            Key::Home => self.move_to(document, Position::new(self.cursor.line, 0), selecting),
            Key::End => self.move_to(
                document,
                Position::new(self.cursor.line, document.line_len_chars(self.cursor.line)),
                selecting,
            ),
            Key::Backspace => self.delete_backward(document),
            Key::Delete => self.delete_forward(document),
            Key::Enter => self.insert(document, "\n"),
            Key::Tab => self.insert(document, "    "),
            Key::Escape => self.anchor = None,
            Key::A if modifiers.command => {
                self.anchor = Some(Position::default());
                self.cursor = document.end_position();
                self.scroll_to_cursor = true;
            }
            Key::Z if modifiers.command => {
                let position = if modifiers.shift {
                    document.redo()
                } else {
                    document.undo()
                };
                if let Some(position) = position {
                    self.go_to(position);
                }
            }
            Key::Y if modifiers.command => {
                if let Some(position) = document.redo() {
                    self.go_to(position);
                }
            }
            _ => {}
        }
        if !matches!(key, Key::ArrowUp | Key::ArrowDown) {
            self.preferred_column = None;
        }
    }
}

pub fn show(
    ui: &mut Ui,
    document: &mut Document,
    state: &mut EditorState,
    worker: &HighlightWorker,
) {
    let focus_id = ui.id().with(("editor-focus", document.id));
    if state.request_focus {
        ui.memory_mut(|memory| memory.request_focus(focus_id));
        state.request_focus = false;
    }
    if ui.memory(|memory| memory.has_focus(focus_id)) {
        state.handle_input(document, ui);
    }

    let font_id = FontId::monospace(FONT_SIZE);
    let glyph_width = ui.fonts(|fonts| fonts.glyph_width(&font_id, 'M')).max(1.0);
    let gutter_digits = document.len_lines().max(1).ilog10() as usize + 1;
    let gutter_width = gutter_digits as f32 * glyph_width + 34.0;
    let viewport_width = ui.available_width();
    let focused = ui.memory(|memory| memory.has_focus(focus_id));
    let mut popup_position = None;
    let mut hovered_code = None;

    ScrollArea::both()
        .id_salt(("editor-scroll", document.id))
        .auto_shrink([false, false])
        .show_rows(ui, ROW_HEIGHT, document.len_lines(), |ui, row_range| {
            ui.style_mut().spacing.item_spacing = Vec2::ZERO;
            state.visible_range = row_range.clone();
            let clip_rect = ui.clip_rect();
            let max_visible_chars = row_range
                .clone()
                .map(|line| document.line_len_chars(line))
                .max()
                .unwrap_or(1);
            let content_width =
                (gutter_width + (max_visible_chars + 8) as f32 * glyph_width).max(viewport_width);

            for line_index in row_range {
                let (rect, response) = ui.allocate_exact_size(
                    Vec2::new(content_width, ROW_HEIGHT),
                    Sense::click_and_drag(),
                );
                if line_index == state.cursor.line {
                    ui.painter().rect_filled(rect, 0.0, ACTIVE_LINE_COLOR);
                }
                paint_selection(
                    ui.painter(),
                    document,
                    state,
                    line_index,
                    rect,
                    gutter_width,
                    glyph_width,
                );

                ui.painter().text(
                    Pos2::new(rect.left() + gutter_width - 14.0, rect.center().y),
                    Align2::RIGHT_CENTER,
                    (line_index + 1).to_string(),
                    font_id.clone(),
                    if line_index == state.cursor.line {
                        TEXT_COLOR
                    } else {
                        MUTED_COLOR
                    },
                );

                let text_left = rect.left() + gutter_width;
                let first_column = ((clip_rect.left() - text_left) / glyph_width)
                    .floor()
                    .max(0.0) as usize;
                let last_column = ((clip_rect.right() - text_left) / glyph_width)
                    .ceil()
                    .max(0.0) as usize
                    + 3;
                let line_len = document.line_len_chars(line_index);
                let use_full_line =
                    first_column == 0 && last_column >= line_len && line_len <= 4096;
                let base_column = if use_full_line {
                    0
                } else {
                    first_column.min(line_len)
                };
                let end_column = if use_full_line {
                    line_len
                } else {
                    last_column.min(line_len)
                };
                let text = if use_full_line {
                    document.line(line_index).into_owned()
                } else {
                    document.line_slice_chars(line_index, base_column, end_column)
                };
                let highlighted = if use_full_line {
                    highlighted_line(state, document, line_index)
                } else {
                    None
                };
                let job = layout_job(&text, &font_id, highlighted);
                let galley = ui.painter().layout_job(job);
                ui.painter().galley(
                    Pos2::new(
                        text_left + base_column as f32 * glyph_width,
                        rect.top() + 2.0,
                    ),
                    galley,
                    TEXT_COLOR,
                );
                paint_diagnostics(
                    ui.painter(),
                    document,
                    state,
                    line_index,
                    rect,
                    text_left,
                    glyph_width,
                );

                if focused && line_index == state.cursor.line {
                    let cursor_x = text_left + state.cursor.column as f32 * glyph_width;
                    popup_position = Some(Pos2::new(cursor_x, rect.bottom() + 4.0));
                    let cursor_rect = Rect::from_min_size(
                        Pos2::new(cursor_x, rect.top() + 2.0),
                        Vec2::new(1.6, ROW_HEIGHT - 4.0),
                    );
                    ui.painter().rect_filled(cursor_rect, 0.0, CURSOR_COLOR);
                    if state.scroll_to_cursor {
                        ui.scroll_to_rect(cursor_rect, Some(Align::Center));
                    }
                }

                if response.hovered()
                    && let Some(pointer) = response.hover_pos()
                {
                    let column = ((pointer.x - text_left) / glyph_width).round().max(0.0) as usize;
                    hovered_code = Some((
                        document.clamp_position(Position::new(line_index, column)),
                        pointer,
                    ));
                }

                if response.clicked() || response.dragged() {
                    ui.memory_mut(|memory| memory.request_focus(focus_id));
                    if let Some(pointer) = response.interact_pointer_pos() {
                        let column =
                            ((pointer.x - text_left) / glyph_width).round().max(0.0) as usize;
                        let position = document.clamp_position(Position::new(line_index, column));
                        if response.dragged() {
                            state.anchor.get_or_insert(state.cursor);
                        } else {
                            state.anchor = None;
                        }
                        state.cursor = position;
                        state.preferred_column = None;
                    }
                }
            }
        });
    state.scroll_to_cursor = false;
    state.update_hover(hovered_code, ui.ctx());
    show_hover(ui, state);
    if let Some(position) = popup_position {
        show_completions(ui, document, state, position);
    }

    if state.highlight_enabled && document.highlighting_allowed() {
        let key = HighlightKey {
            revision: document.revision,
            range: state.visible_range.clone(),
        };
        if state.highlight_key.as_ref() != Some(&key) && !state.visible_range.is_empty() {
            let lines = state
                .visible_range
                .clone()
                .map(|line| {
                    if document.line_len_chars(line) > MAX_HIGHLIGHT_LINE_BYTES {
                        String::new()
                    } else {
                        document.line(line).into_owned()
                    }
                })
                .collect();
            worker.request(
                document.id,
                document.path.clone(),
                document.revision,
                key.range.start,
                lines,
            );
            state.highlight_key = Some(key);
        }
    }
}

fn show_hover(ui: &mut Ui, state: &EditorState) {
    if state.completion_open {
        return;
    }
    let (Some(position), Some(text)) = (state.hover_screen_position, state.hover_text.as_ref())
    else {
        return;
    };
    egui::Area::new(ui.id().with("lsp-hover"))
        .order(egui::Order::Foreground)
        .fixed_pos(position + Vec2::new(16.0, 18.0))
        .show(ui.ctx(), |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.set_max_width(620.0);
                ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
                    ui.add(
                        egui::Label::new(egui::RichText::new(text).size(13.0))
                            .wrap()
                            .selectable(true),
                    );
                });
            });
        });
}

fn show_completions(ui: &mut Ui, document: &mut Document, state: &mut EditorState, position: Pos2) {
    if !state.completion_open || state.completions.is_empty() {
        return;
    }
    let mut clicked = None;
    egui::Area::new(egui::Id::new(("lsp-completion", document.id)))
        .order(egui::Order::Foreground)
        .fixed_pos(position)
        .show(ui.ctx(), |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.set_min_width(340.0);
                ui.set_max_width(520.0);
                ScrollArea::vertical().max_height(280.0).show_rows(
                    ui,
                    24.0,
                    state.completions.len(),
                    |ui, range| {
                        for index in range {
                            let item = &state.completions[index];
                            let detail = item.detail.as_deref().unwrap_or_default();
                            let detail: String = detail.chars().take(64).collect();
                            let text = if detail.is_empty() {
                                item.label.clone()
                            } else {
                                format!("{}  {}", item.label, detail)
                            };
                            let response =
                                ui.selectable_label(index == state.completion_selected, text);
                            if index == state.completion_selected {
                                ui.scroll_to_rect(response.rect, None);
                            }
                            if response.clicked() {
                                clicked = Some(index);
                            }
                        }
                    },
                );
                ui.label(
                    egui::RichText::new("↑↓ — выбор    Enter/Tab — вставить    Esc — закрыть")
                        .small()
                        .weak(),
                );
            });
        });
    if let Some(index) = clicked {
        state.completion_selected = index;
        state.apply_completion(document);
    }
}

fn paint_diagnostics(
    painter: &Painter,
    document: &Document,
    state: &EditorState,
    line: usize,
    rect: Rect,
    text_left: f32,
    glyph_width: f32,
) {
    for diagnostic in &state.diagnostics {
        let start_line = diagnostic.range.start.line as usize;
        let end_line = diagnostic.range.end.line as usize;
        if line < start_line || line > end_line {
            continue;
        }
        let start = if line == start_line {
            document.from_lsp_position(diagnostic.range.start)
        } else {
            Position::new(line, 0)
        };
        let end = if line == end_line {
            document.from_lsp_position(diagnostic.range.end)
        } else {
            Position::new(line, document.line_len_chars(line))
        };
        let color = match diagnostic.severity {
            Some(severity) if severity == DiagnosticSeverity::WARNING => crate::theme::WARNING,
            Some(severity)
                if severity == DiagnosticSeverity::INFORMATION
                    || severity == DiagnosticSeverity::HINT =>
            {
                crate::theme::CYAN
            }
            _ => crate::theme::ERROR,
        };
        let left = text_left + start.column as f32 * glyph_width;
        let right = (text_left + end.column as f32 * glyph_width).max(left + glyph_width);
        let y = rect.bottom() - 1.5;
        painter.line_segment(
            [Pos2::new(left, y), Pos2::new(right, y)],
            Stroke::new(1.4_f32, color),
        );
    }
}

fn highlighted_line<'a>(
    state: &'a EditorState,
    document: &Document,
    line: usize,
) -> Option<&'a HighlightedLine> {
    let highlights = state.highlights.as_ref()?;
    if highlights.revision != document.revision {
        return None;
    }
    let index = line.checked_sub(highlights.start_line)?;
    highlights.lines.get(index)
}

fn layout_job(text: &str, font_id: &FontId, highlighted: Option<&HighlightedLine>) -> LayoutJob {
    let mut job = LayoutJob::default();
    job.wrap.max_width = f32::INFINITY;
    let format = |color| TextFormat {
        font_id: font_id.clone(),
        color,
        ..Default::default()
    };

    if let Some(highlighted) = highlighted {
        let mut offset = 0;
        for span in &highlighted.spans {
            if span.byte_range.start > offset
                && span.byte_range.start <= text.len()
                && text.is_char_boundary(span.byte_range.start)
            {
                job.append(
                    &text[offset..span.byte_range.start],
                    0.0,
                    format(TEXT_COLOR),
                );
            }
            if span.byte_range.end <= text.len()
                && span.byte_range.start < span.byte_range.end
                && text.is_char_boundary(span.byte_range.start)
                && text.is_char_boundary(span.byte_range.end)
            {
                job.append(&text[span.byte_range.clone()], 0.0, format(span.color));
                offset = span.byte_range.end;
            }
        }
        if offset < text.len() {
            job.append(&text[offset..], 0.0, format(TEXT_COLOR));
        }
    } else {
        job.append(text, 0.0, format(TEXT_COLOR));
    }
    job
}

fn paint_selection(
    painter: &Painter,
    document: &Document,
    state: &EditorState,
    line: usize,
    rect: Rect,
    gutter_width: f32,
    glyph_width: f32,
) {
    let Some((start, end)) = state.selection() else {
        return;
    };
    if line < start.line || line > end.line {
        return;
    }
    let line_len = document.line_len_chars(line);
    let from = if line == start.line { start.column } else { 0 };
    let to = if line == end.line {
        end.column
    } else {
        line_len + 1
    };
    let left = rect.left() + gutter_width + from as f32 * glyph_width;
    let right = rect.left() + gutter_width + to as f32 * glyph_width;
    painter.rect(
        Rect::from_min_max(Pos2::new(left, rect.top()), Pos2::new(right, rect.bottom())),
        0.0,
        SELECTION_COLOR,
        Stroke::NONE,
        egui::epaint::StrokeKind::Inside,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    #[test]
    fn completion_replaces_identifier_prefix() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(b"targ").unwrap();
        file.flush().unwrap();
        let mut document = Document::open(file.path(), 800).unwrap();
        let mut state = EditorState {
            cursor: Position::new(0, 4),
            ..EditorState::default()
        };
        state.set_completions(vec![CompletionItem {
            label: "target()".into(),
            insert_text: Some("target".into()),
            ..CompletionItem::default()
        }]);

        state.apply_completion(&mut document);
        assert_eq!(document.line(0), "target");
        assert_eq!(state.cursor, Position::new(0, 6));
        assert!(!state.completion_open);
    }
}
