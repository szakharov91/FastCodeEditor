use std::{
    borrow::Cow,
    cmp::{max, min},
    fs::{self, File},
    io::{BufReader, BufWriter, Read, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use ropey::Rope;

use crate::piece_table::PieceTable;

pub const LARGE_FILE_THRESHOLD: u64 = 64 * 1024 * 1024;
pub const HIGHLIGHT_FILE_LIMIT: u64 = 256 * 1024 * 1024;
pub const MAX_HIGHLIGHT_LINE_BYTES: usize = 16 * 1024;
const MAX_HISTORY: usize = 2_000;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Ord, PartialOrd)]
pub struct Position {
    pub line: usize,
    pub column: usize,
}

impl Position {
    pub const fn new(line: usize, column: usize) -> Self {
        Self { line, column }
    }
}

#[derive(Clone, Debug)]
pub struct Edit {
    pub start: Position,
    pub end: Position,
    pub inserted: String,
    pub deleted: String,
}

#[derive(Clone)]
enum Storage {
    Rope(Rope),
    Pieces(PieceTable),
}

#[derive(Clone)]
pub struct DocumentSnapshot {
    storage: Storage,
    pub path: PathBuf,
    pub revision: u64,
}

impl DocumentSnapshot {
    pub fn len_lines(&self) -> usize {
        match &self.storage {
            Storage::Rope(rope) => rope.len_lines(),
            Storage::Pieces(pieces) => pieces.len_lines(),
        }
    }

    pub fn line(&self, line: usize) -> Cow<'_, str> {
        match &self.storage {
            Storage::Rope(rope) => trim_line_ending(rope.line(line).to_string()).into(),
            Storage::Pieces(pieces) => pieces.line(line).into(),
        }
    }
}

pub struct Document {
    pub id: u64,
    pub path: PathBuf,
    storage: Storage,
    pub revision: u64,
    pub dirty: bool,
    pub read_only: bool,
    pub byte_len: u64,
    pub load_time: Duration,
    undo: Vec<Edit>,
    redo: Vec<Edit>,
    last_edit: Instant,
}

impl Document {
    pub fn open(path: &Path, id: u64) -> Result<Self> {
        Self::open_with_limit(path, id, LARGE_FILE_THRESHOLD)
    }

    fn open_with_limit(path: &Path, id: u64, large_file_threshold: u64) -> Result<Self> {
        let started = Instant::now();
        let metadata = fs::metadata(path)
            .with_context(|| format!("Не удалось прочитать метаданные {}", path.display()))?;
        if !metadata.is_file() {
            bail!("{} не является файлом", path.display());
        }

        let mut sample_file = File::open(path)?;
        let mut sample = [0_u8; 8192];
        let read = sample_file.read(&mut sample)?;
        if sample[..read].contains(&0) {
            bail!("Бинарные файлы не поддерживаются: {}", path.display());
        }
        drop(sample_file);

        let byte_len = metadata.len();
        let use_pieces = byte_len >= large_file_threshold && byte_len > 0;
        let storage = if use_pieces {
            let file = File::open(path)?;
            Storage::Pieces(PieceTable::open(&file)?)
        } else {
            let file = File::open(path)?;
            match Rope::from_reader(BufReader::with_capacity(1024 * 1024, file)) {
                Ok(rope) => Storage::Rope(rope),
                Err(error) if error.kind() == std::io::ErrorKind::InvalidData => {
                    let bytes = fs::read(path)?;
                    Storage::Rope(Rope::from_str(&String::from_utf8_lossy(&bytes)))
                }
                Err(error) => return Err(error.into()),
            }
        };

        Ok(Self {
            id,
            path: path.to_path_buf(),
            storage,
            revision: 0,
            dirty: false,
            read_only: false,
            byte_len,
            load_time: started.elapsed(),
            undo: Vec::new(),
            redo: Vec::new(),
            last_edit: Instant::now(),
        })
    }

    pub fn snapshot(&self) -> DocumentSnapshot {
        DocumentSnapshot {
            storage: self.storage.clone(),
            path: self.path.clone(),
            revision: self.revision,
        }
    }

    pub fn compaction_snapshot(&self) -> Option<DocumentSnapshot> {
        let Storage::Pieces(pieces) = &self.storage else {
            return None;
        };
        (pieces.needs_compaction() && self.last_edit.elapsed() >= Duration::from_millis(1_500))
            .then(|| self.snapshot())
    }

    pub fn compact_snapshot(snapshot: &DocumentSnapshot) -> Result<DocumentSnapshot> {
        let Storage::Pieces(pieces) = &snapshot.storage else {
            bail!("Компактификация доступна только для piece table");
        };
        Ok(DocumentSnapshot {
            storage: Storage::Pieces(pieces.compact()?),
            path: snapshot.path.clone(),
            revision: snapshot.revision,
        })
    }

    pub fn apply_compaction(&mut self, snapshot: DocumentSnapshot) -> bool {
        if snapshot.revision != self.revision || !matches!(&snapshot.storage, Storage::Pieces(_)) {
            return false;
        }
        self.storage = snapshot.storage;
        true
    }

    pub fn fragment_count(&self) -> Option<usize> {
        match &self.storage {
            Storage::Rope(_) => None,
            Storage::Pieces(pieces) => Some(pieces.fragment_count()),
        }
    }

    pub fn file_name(&self) -> Cow<'_, str> {
        self.path
            .file_name()
            .map(|name| name.to_string_lossy())
            .unwrap_or_else(|| self.path.to_string_lossy())
    }

    pub fn backend_label(&self) -> &'static str {
        match &self.storage {
            Storage::Rope(_) => "ROPE",
            Storage::Pieces(_) => "PIECE TABLE · mmap",
        }
    }

    pub fn len_lines(&self) -> usize {
        match &self.storage {
            Storage::Rope(rope) => rope.len_lines(),
            Storage::Pieces(pieces) => pieces.len_lines(),
        }
    }

    pub fn line(&self, line: usize) -> Cow<'_, str> {
        match &self.storage {
            Storage::Rope(rope) => trim_line_ending(rope.line(line).to_string()).into(),
            Storage::Pieces(pieces) => pieces.line(line).into(),
        }
    }

    pub fn line_len_chars(&self, line: usize) -> usize {
        match &self.storage {
            Storage::Rope(rope) => {
                let line = rope.line(line);
                let mut len = line.len_chars();
                if len > 0 && line.char(len - 1) == '\n' {
                    len -= 1;
                }
                if len > 0 && line.char(len - 1) == '\r' {
                    len -= 1;
                }
                len
            }
            Storage::Pieces(pieces) => pieces.line_len_chars(line),
        }
    }

    pub fn line_slice_chars(&self, line: usize, start: usize, end: usize) -> String {
        let line_len = self.line_len_chars(line);
        let start = min(start, line_len);
        let end = min(max(end, start), line_len);
        match &self.storage {
            Storage::Rope(rope) => rope.line(line).slice(start..end).to_string(),
            Storage::Pieces(pieces) => pieces.line_slice_chars(line, start, end),
        }
    }

    pub fn end_position(&self) -> Position {
        let last_line = self.len_lines().saturating_sub(1);
        Position::new(last_line, self.line_len_chars(last_line))
    }

    pub fn clamp_position(&self, position: Position) -> Position {
        let line = min(position.line, self.len_lines().saturating_sub(1));
        Position::new(line, min(position.column, self.line_len_chars(line)))
    }

    pub fn replace(&mut self, start: Position, end: Position, text: &str) -> Option<Edit> {
        if self.read_only {
            return None;
        }
        let start = self.clamp_position(start);
        let end = self.clamp_position(max(start, end));
        let deleted = self.text_range(start, end);
        if deleted == text {
            return None;
        }
        self.replace_raw(start, end, text);
        self.byte_len = self.byte_len.saturating_sub(deleted.len() as u64) + text.len() as u64;
        let edit = Edit {
            start,
            end,
            inserted: text.to_owned(),
            deleted,
        };
        self.undo.push(edit.clone());
        if self.undo.len() > MAX_HISTORY {
            self.undo.remove(0);
        }
        self.redo.clear();
        Some(edit)
    }

    fn replace_raw(&mut self, start: Position, end: Position, text: &str) {
        match &mut self.storage {
            Storage::Rope(rope) => {
                let start_char = rope.line_to_char(start.line) + start.column;
                let end_char = rope.line_to_char(end.line) + end.column;
                if end_char > start_char {
                    rope.remove(start_char..end_char);
                }
                if !text.is_empty() {
                    rope.insert(start_char, text);
                }
            }
            Storage::Pieces(pieces) => {
                let start_byte = pieces.position_to_byte(start.line, start.column);
                let end_byte = pieces.position_to_byte(end.line, end.column);
                pieces.replace(start_byte, end_byte, text);
            }
        }
        self.revision = self.revision.wrapping_add(1);
        self.dirty = true;
        self.last_edit = Instant::now();
    }

    pub fn undo(&mut self) -> Option<Position> {
        let edit = self.undo.pop()?;
        let position = edit.start;
        let inserted_end = advance(edit.start, &edit.inserted);
        self.replace_raw(edit.start, inserted_end, &edit.deleted);
        self.byte_len =
            self.byte_len.saturating_sub(edit.inserted.len() as u64) + edit.deleted.len() as u64;
        self.redo.push(edit);
        Some(position)
    }

    pub fn redo(&mut self) -> Option<Position> {
        let edit = self.redo.pop()?;
        self.replace_raw(edit.start, edit.end, &edit.inserted);
        self.byte_len =
            self.byte_len.saturating_sub(edit.deleted.len() as u64) + edit.inserted.len() as u64;
        let position = advance(edit.start, &edit.inserted);
        self.undo.push(edit);
        Some(position)
    }

    pub fn text_range(&self, start: Position, end: Position) -> String {
        let start = self.clamp_position(start);
        let end = self.clamp_position(max(start, end));
        if start == end {
            return String::new();
        }
        if start.line == end.line {
            return self.line_slice_chars(start.line, start.column, end.column);
        }

        let mut result = String::new();
        let first_len = self.line_len_chars(start.line);
        result.push_str(&self.line_slice_chars(start.line, start.column, first_len));
        result.push('\n');
        for line in start.line + 1..end.line {
            result.push_str(&self.line(line));
            result.push('\n');
        }
        result.push_str(&self.line_slice_chars(end.line, 0, end.column));
        result
    }

    pub fn mark_saved(&mut self, byte_len: u64) {
        self.dirty = false;
        self.byte_len = byte_len;
    }

    pub fn highlighting_allowed(&self) -> bool {
        self.byte_len <= HIGHLIGHT_FILE_LIMIT
    }
}

pub fn advance(mut position: Position, text: &str) -> Position {
    let mut chars = text.chars().peekable();
    while let Some(character) = chars.next() {
        match character {
            '\n' => {
                position.line += 1;
                position.column = 0;
            }
            '\r' if chars.peek() == Some(&'\n') => {}
            _ => position.column += 1,
        }
    }
    position
}

pub fn save_snapshot(snapshot: &DocumentSnapshot, path: &Path) -> Result<u64> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    if let Ok(metadata) = fs::metadata(path) {
        temporary
            .as_file()
            .set_permissions(metadata.permissions())?;
    }
    {
        let mut writer = BufWriter::with_capacity(1024 * 1024, temporary.as_file_mut());
        match &snapshot.storage {
            Storage::Rope(rope) => rope.write_to(&mut writer)?,
            Storage::Pieces(pieces) => pieces.write_to(&mut writer)?,
        }
        writer.flush()?;
    }
    temporary.as_file().sync_all()?;
    let byte_len = temporary.as_file().metadata()?.len();
    temporary.persist(path).map_err(|error| error.error)?;
    Ok(byte_len)
}

fn trim_line_ending(mut text: String) -> String {
    if text.ends_with('\n') {
        text.pop();
        if text.ends_with('\r') {
            text.pop();
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn position_advance_handles_crlf() {
        assert_eq!(
            advance(Position::new(4, 2), "a\r\nb\nc"),
            Position::new(6, 1)
        );
    }

    #[test]
    fn rope_edits_and_undo() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write!(file, "first\nsecond\nthird").unwrap();
        let mut document = Document::open_with_limit(file.path(), 1, u64::MAX).unwrap();

        document
            .replace(Position::new(1, 6), Position::new(1, 6), "!")
            .unwrap();
        assert_eq!(document.line(1), "second!");
        assert_eq!(
            document.text_range(Position::new(0, 2), Position::new(1, 2)),
            "rst\nse"
        );

        document.undo().unwrap();
        assert_eq!(document.line(1), "second");
        document.redo().unwrap();
        assert_eq!(document.line(1), "second!");
    }

    #[test]
    fn piece_table_file_is_editable_and_undoable() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        for line in 0..10_000 {
            writeln!(file, "line-{line}-привет").unwrap();
        }
        file.flush().unwrap();
        let mut document = Document::open_with_limit(file.path(), 2, 1).unwrap();

        assert!(!document.read_only);
        assert_eq!(document.backend_label(), "PIECE TABLE · mmap");
        assert_eq!(document.len_lines(), 10_001);
        assert_eq!(document.line(9_999), "line-9999-привет");
        assert_eq!(document.line_len_chars(9_999), 16);
        assert_eq!(document.line_slice_chars(9_999, 5, 10), "9999-");

        document
            .replace(Position::new(9_999, 0), Position::new(9_999, 0), "EDIT: ")
            .unwrap();
        document
            .replace(Position::new(1, 4), Position::new(2, 3), "\nNEW LINE\n")
            .unwrap();
        assert_eq!(document.line(10_000), "EDIT: line-9999-привет");
        assert!(document.dirty);
        assert_eq!(document.revision, 2);

        document.undo().unwrap();
        document.undo().unwrap();
        assert_eq!(document.line(9_999), "line-9999-привет");
        document.redo().unwrap();
        document.redo().unwrap();
        assert_eq!(document.line(10_000), "EDIT: line-9999-привет");
    }

    #[test]
    fn piece_snapshot_is_stable_during_later_edits() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write!(file, "before").unwrap();
        file.flush().unwrap();
        let mut document = Document::open_with_limit(file.path(), 4, 1).unwrap();
        document
            .replace(Position::new(0, 6), Position::new(0, 6), " snapshot")
            .unwrap();
        let snapshot = document.snapshot();
        document
            .replace(Position::new(0, 15), Position::new(0, 15), " later")
            .unwrap();

        let output = tempfile::NamedTempFile::new().unwrap();
        save_snapshot(&snapshot, output.path()).unwrap();
        assert_eq!(
            fs::read_to_string(output.path()).unwrap(),
            "before snapshot"
        );
        assert_eq!(document.line(0), "before snapshot later");
    }

    #[test]
    fn compaction_preserves_history_and_dirty_state() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write!(file, "before").unwrap();
        file.flush().unwrap();
        let mut document = Document::open_with_limit(file.path(), 5, 1).unwrap();
        document
            .replace(Position::new(0, 6), Position::new(0, 6), " first")
            .unwrap();
        document
            .replace(Position::new(0, 12), Position::new(0, 12), " second")
            .unwrap();

        let compacted = Document::compact_snapshot(&document.snapshot()).unwrap();
        assert!(document.apply_compaction(compacted));
        assert_eq!(document.fragment_count(), Some(1));
        assert_eq!(document.line(0), "before first second");
        assert!(document.dirty);

        document.undo().unwrap();
        assert_eq!(document.line(0), "before first");
        document.redo().unwrap();
        assert_eq!(document.line(0), "before first second");
    }

    #[test]
    fn stale_compaction_cannot_overwrite_newer_edits() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write!(file, "before").unwrap();
        file.flush().unwrap();
        let mut document = Document::open_with_limit(file.path(), 6, 1).unwrap();
        document
            .replace(Position::new(0, 6), Position::new(0, 6), " stable")
            .unwrap();
        let compacted = Document::compact_snapshot(&document.snapshot()).unwrap();
        document
            .replace(Position::new(0, 13), Position::new(0, 13), " newer")
            .unwrap();

        assert!(!document.apply_compaction(compacted));
        assert_eq!(document.line(0), "before stable newer");
        assert_eq!(document.revision, 2);
        assert!(document.dirty);
    }

    #[test]
    fn binary_files_are_rejected() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(b"abc\0def").unwrap();
        assert!(Document::open(file.path(), 3).is_err());
    }
}
