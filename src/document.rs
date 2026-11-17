use std::{
    borrow::Cow,
    cmp::{max, min},
    fs::{self, File},
    io::{BufReader, BufWriter, Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use memmap2::Mmap;
use ropey::Rope;

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
struct MappedFile {
    data: Arc<Mmap>,
    line_starts: Arc<Vec<usize>>,
    line_char_lengths: Arc<Vec<usize>>,
}

impl MappedFile {
    fn open(file: &File) -> Result<Self> {
        // SAFETY: the editor never mutates or truncates an open file itself. External
        // truncation while a mapping is active is a platform-level I/O hazard, so the
        // file watcher/reload workflow must replace the mapping before further reads.
        let data = unsafe { Mmap::map(file)? };
        let mut line_starts = Vec::with_capacity(data.len() / 48 + 1);
        line_starts.push(0);
        for newline in memchr::memchr_iter(b'\n', &data) {
            line_starts.push(newline + 1);
        }

        let mut line_char_lengths = Vec::with_capacity(line_starts.len());
        for line in 0..line_starts.len() {
            let start = line_starts[line];
            let mut end = line_starts.get(line + 1).copied().unwrap_or(data.len());
            if end > start && data[end - 1] == b'\n' {
                end -= 1;
            }
            if end > start && data[end - 1] == b'\r' {
                end -= 1;
            }
            let text = String::from_utf8_lossy(&data[start..end]);
            line_char_lengths.push(text.chars().count());
        }

        Ok(Self {
            data: Arc::new(data),
            line_starts: Arc::new(line_starts),
            line_char_lengths: Arc::new(line_char_lengths),
        })
    }

    fn len_lines(&self) -> usize {
        self.line_starts.len()
    }

    fn line(&self, line: usize) -> Cow<'_, str> {
        let start = self.line_starts[line];
        let mut end = self
            .line_starts
            .get(line + 1)
            .copied()
            .unwrap_or(self.data.len());
        if end > start && self.data[end - 1] == b'\n' {
            end -= 1;
        }
        if end > start && self.data[end - 1] == b'\r' {
            end -= 1;
        }
        String::from_utf8_lossy(&self.data[start..end])
    }
}

#[derive(Clone)]
enum Storage {
    Rope(Rope),
    Mapped(MappedFile),
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
            Storage::Mapped(mapped) => mapped.len_lines(),
        }
    }

    pub fn line(&self, line: usize) -> Cow<'_, str> {
        match &self.storage {
            Storage::Rope(rope) => trim_line_ending(rope.line(line).to_string()).into(),
            Storage::Mapped(mapped) => mapped.line(line),
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
        let use_mapping = byte_len >= large_file_threshold && byte_len > 0;
        let storage = if use_mapping {
            let file = File::open(path)?;
            Storage::Mapped(MappedFile::open(&file)?)
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
            read_only: use_mapping,
            byte_len,
            load_time: started.elapsed(),
            undo: Vec::new(),
            redo: Vec::new(),
        })
    }

    pub fn snapshot(&self) -> DocumentSnapshot {
        DocumentSnapshot {
            storage: self.storage.clone(),
            path: self.path.clone(),
            revision: self.revision,
        }
    }

    pub fn file_name(&self) -> Cow<'_, str> {
        self.path
            .file_name()
            .map(|name| name.to_string_lossy())
            .unwrap_or_else(|| self.path.to_string_lossy())
    }

    pub fn len_lines(&self) -> usize {
        match &self.storage {
            Storage::Rope(rope) => rope.len_lines(),
            Storage::Mapped(mapped) => mapped.len_lines(),
        }
    }

    pub fn line(&self, line: usize) -> Cow<'_, str> {
        match &self.storage {
            Storage::Rope(rope) => trim_line_ending(rope.line(line).to_string()).into(),
            Storage::Mapped(mapped) => mapped.line(line),
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
            Storage::Mapped(mapped) => mapped.line_char_lengths[line],
        }
    }

    pub fn line_slice_chars(&self, line: usize, start: usize, end: usize) -> String {
        let line_len = self.line_len_chars(line);
        let start = min(start, line_len);
        let end = min(max(end, start), line_len);
        match &self.storage {
            Storage::Rope(rope) => rope.line(line).slice(start..end).to_string(),
            Storage::Mapped(_) => self
                .line(line)
                .chars()
                .skip(start)
                .take(end - start)
                .collect(),
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
        let Storage::Rope(rope) = &mut self.storage else {
            return;
        };
        let start_char = rope.line_to_char(start.line) + start.column;
        let end_char = rope.line_to_char(end.line) + end.column;
        if end_char > start_char {
            rope.remove(start_char..end_char);
        }
        if !text.is_empty() {
            rope.insert(start_char, text);
        }
        self.revision = self.revision.wrapping_add(1);
        self.dirty = true;
    }

    pub fn undo(&mut self) -> Option<Position> {
        let edit = self.undo.pop()?;
        let position = edit.start;
        let inserted_end = advance(edit.start, &edit.inserted);
        self.replace_raw(edit.start, inserted_end, &edit.deleted);
        self.redo.push(edit);
        Some(position)
    }

    pub fn redo(&mut self) -> Option<Position> {
        let edit = self.redo.pop()?;
        self.replace_raw(edit.start, edit.end, &edit.inserted);
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

pub fn save_rope(snapshot: &DocumentSnapshot, path: &Path) -> Result<u64> {
    let Storage::Rope(rope) = &snapshot.storage else {
        bail!("Файл в режиме memory mapping доступен только для чтения");
    };
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    if let Ok(metadata) = fs::metadata(path) {
        temporary
            .as_file()
            .set_permissions(metadata.permissions())?;
    }
    {
        let mut writer = BufWriter::with_capacity(1024 * 1024, temporary.as_file_mut());
        rope.write_to(&mut writer)?;
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
    fn mapped_file_is_indexed_without_rope() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        for line in 0..10_000 {
            writeln!(file, "line-{line}-привет").unwrap();
        }
        file.flush().unwrap();
        let document = Document::open_with_limit(file.path(), 2, 1).unwrap();

        assert!(document.read_only);
        assert_eq!(document.len_lines(), 10_001);
        assert_eq!(document.line(9_999), "line-9999-привет");
        assert_eq!(document.line_len_chars(9_999), 16);
        assert_eq!(document.line_slice_chars(9_999, 5, 10), "9999-");
    }

    #[test]
    fn binary_files_are_rejected() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(b"abc\0def").unwrap();
        assert!(Document::open(file.path(), 3).is_err());
    }
}
