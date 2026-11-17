use std::{
    fs::File,
    io::{self, Write},
    sync::Arc,
};

use anyhow::Result;
use memmap2::Mmap;

type Link = Option<Box<Node>>;

#[derive(Clone)]
struct OriginalFile {
    data: Arc<Mmap>,
    line_starts: Arc<Vec<usize>>,
    line_char_lengths: Arc<Vec<usize>>,
    // Keeps compacted temporary files alive on platforms that cannot delete
    // an open mapping. On Unix the same ownership model is harmless.
    _backing: Option<Arc<tempfile::NamedTempFile>>,
}

impl OriginalFile {
    fn open(file: &File) -> Result<Self> {
        Self::open_with_backing(file, None)
    }

    fn open_with_backing(
        file: &File,
        backing: Option<Arc<tempfile::NamedTempFile>>,
    ) -> Result<Self> {
        // SAFETY: saving replaces the file atomically instead of mutating it in place.
        // The old mapping remains valid while a background snapshot is being written.
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
            line_char_lengths.push(String::from_utf8_lossy(&data[start..end]).chars().count());
        }
        Ok(Self {
            data: Arc::new(data),
            line_starts: Arc::new(line_starts),
            line_char_lengths: Arc::new(line_char_lengths),
            _backing: backing,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Source {
    Original,
    Added,
}

#[derive(Clone, Copy, Debug)]
struct Piece {
    source: Source,
    start: usize,
    len: usize,
    newlines: usize,
}

pub(crate) const COMPACTION_FRAGMENT_THRESHOLD: usize = 8_192;

#[derive(Clone)]
struct Node {
    piece: Piece,
    left: Link,
    right: Link,
    priority: u64,
    bytes: usize,
    newlines: usize,
    pieces: usize,
}

impl Node {
    fn new(piece: Piece, priority: u64) -> Self {
        Self {
            bytes: piece.len,
            newlines: piece.newlines,
            pieces: 1,
            piece,
            left: None,
            right: None,
            priority,
        }
    }

    fn update(&mut self) {
        self.bytes = self.piece.len + link_bytes(&self.left) + link_bytes(&self.right);
        self.newlines =
            self.piece.newlines + link_newlines(&self.left) + link_newlines(&self.right);
        self.pieces = 1 + link_pieces(&self.left) + link_pieces(&self.right);
    }
}

fn link_bytes(link: &Link) -> usize {
    link.as_ref().map_or(0, |node| node.bytes)
}

fn link_newlines(link: &Link) -> usize {
    link.as_ref().map_or(0, |node| node.newlines)
}

fn link_pieces(link: &Link) -> usize {
    link.as_ref().map_or(0, |node| node.pieces)
}

#[derive(Clone)]
pub struct PieceTable {
    original: OriginalFile,
    added: Arc<Vec<u8>>,
    added_line_starts: Arc<Vec<usize>>,
    root: Link,
    len_bytes: usize,
    len_lines: usize,
    random_state: u64,
}

impl PieceTable {
    pub fn open(file: &File) -> Result<Self> {
        Ok(Self::from_original(OriginalFile::open(file)?))
    }

    fn from_original(original: OriginalFile) -> Self {
        let len_bytes = original.data.len();
        let len_lines = original.line_starts.len();
        let root = if len_bytes == 0 {
            None
        } else {
            let piece = Piece {
                source: Source::Original,
                start: 0,
                len: len_bytes,
                newlines: len_lines - 1,
            };
            Some(Box::new(Node::new(piece, 0x9e37_79b9_7f4a_7c15)))
        };
        Self {
            original,
            added: Arc::new(Vec::new()),
            added_line_starts: Arc::new(vec![0]),
            root,
            len_bytes,
            len_lines,
            random_state: 0xd1b5_4a32_d192_ed03,
        }
    }

    pub fn len_bytes(&self) -> usize {
        self.len_bytes
    }

    pub fn len_lines(&self) -> usize {
        self.len_lines
    }

    pub fn fragment_count(&self) -> usize {
        link_pieces(&self.root)
    }

    pub fn needs_compaction(&self) -> bool {
        self.fragment_count() >= COMPACTION_FRAGMENT_THRESHOLD
    }

    pub fn compact(&self) -> Result<Self> {
        if self.len_bytes == 0 {
            let mut compacted = self.clone();
            compacted.root = None;
            compacted.added = Arc::new(Vec::new());
            compacted.added_line_starts = Arc::new(vec![0]);
            compacted.refresh_stats();
            return Ok(compacted);
        }

        let mut temporary = tempfile::NamedTempFile::new()?;
        {
            let mut writer = io::BufWriter::with_capacity(1024 * 1024, temporary.as_file_mut());
            self.write_to(&mut writer)?;
            writer.flush()?;
        }
        temporary.as_file().sync_all()?;
        let backing = Arc::new(temporary);
        let original =
            OriginalFile::open_with_backing(backing.as_file(), Some(Arc::clone(&backing)))?;
        Ok(Self::from_original(original))
    }

    pub fn position_to_byte(&self, line: usize, column: usize) -> usize {
        let (start, end) = self.line_content_range(line);
        self.byte_for_column(start, end, column)
    }

    pub fn replace(&mut self, start: usize, end: usize, text: &str) {
        assert!(start <= end && end <= self.len_bytes);
        if start < end {
            self.delete(start, end - start);
        }
        if !text.is_empty() {
            self.insert(start, text);
        }
        self.refresh_stats();
    }

    fn insert(&mut self, offset: usize, text: &str) {
        let added_start = self.append_added(text.as_bytes());
        let piece = make_piece(
            Source::Added,
            added_start,
            added_start + text.len(),
            &self.original,
            &self.added_line_starts,
        );
        let priority = self.next_priority();
        let root = self.root.take();
        let (left, right) = split(root, offset, &self.original, &self.added_line_starts);
        let left = merge(left, Some(Box::new(Node::new(piece, priority))));
        self.root = merge(left, right);
    }

    fn delete(&mut self, offset: usize, len: usize) {
        let root = self.root.take();
        let (left, remainder) = split(root, offset, &self.original, &self.added_line_starts);
        let (_, right) = split(remainder, len, &self.original, &self.added_line_starts);
        self.root = merge(left, right);
    }

    fn append_added(&mut self, bytes: &[u8]) -> usize {
        let start = self.added.len();
        Arc::make_mut(&mut self.added).extend_from_slice(bytes);
        let line_starts = Arc::make_mut(&mut self.added_line_starts);
        for newline in memchr::memchr_iter(b'\n', bytes) {
            line_starts.push(start + newline + 1);
        }
        start
    }

    fn next_priority(&mut self) -> u64 {
        self.random_state ^= self.random_state << 13;
        self.random_state ^= self.random_state >> 7;
        self.random_state ^= self.random_state << 17;
        self.random_state
    }

    fn refresh_stats(&mut self) {
        self.len_bytes = link_bytes(&self.root);
        self.len_lines = link_newlines(&self.root) + 1;
    }

    pub fn line(&self, line: usize) -> String {
        let (start, end) = self.line_content_range(line);
        self.text_range(start, end)
    }

    pub fn line_len_chars(&self, line: usize) -> usize {
        let (start, end) = self.line_content_range(line);
        let chunks = self.chunks(start, end);
        if chunks.is_empty() {
            return 0;
        }
        if let [chunk] = chunks.as_slice()
            && chunk.source == Source::Original
        {
            let original_line = newline_rank(&self.original.line_starts, chunk.source_start);
            let original_start = self.original.line_starts[original_line];
            let mut original_end = self
                .original
                .line_starts
                .get(original_line + 1)
                .copied()
                .unwrap_or(self.original.data.len());
            if original_end > original_start && self.original.data[original_end - 1] == b'\n' {
                original_end -= 1;
            }
            if original_end > original_start && self.original.data[original_end - 1] == b'\r' {
                original_end -= 1;
            }
            if original_start == chunk.source_start
                && original_end == chunk.source_start + chunk.bytes.len()
            {
                return self.original.line_char_lengths[original_line];
            }
        }
        chunks
            .iter()
            .map(|chunk| String::from_utf8_lossy(chunk.bytes).chars().count())
            .sum()
    }

    pub fn line_slice_chars(&self, line: usize, column_start: usize, column_end: usize) -> String {
        let (line_start, line_end) = self.line_content_range(line);
        let start = self.byte_for_column(line_start, line_end, column_start);
        let end = self.byte_for_column(line_start, line_end, column_end);
        self.text_range(start, end)
    }

    fn line_content_range(&self, line: usize) -> (usize, usize) {
        assert!(line < self.len_lines);
        let start = self.byte_of_line(line);
        let mut end = if line + 1 < self.len_lines {
            self.byte_of_line(line + 1)
        } else {
            self.len_bytes
        };
        if end > start && self.byte_at(end - 1) == Some(b'\n') {
            end -= 1;
        }
        if end > start && self.byte_at(end - 1) == Some(b'\r') {
            end -= 1;
        }
        (start, end)
    }

    fn byte_of_line(&self, line: usize) -> usize {
        if line == 0 {
            return 0;
        }
        select_line(&self.root, line, 0, &self.original, &self.added_line_starts)
    }

    fn byte_for_column(&self, start: usize, end: usize, column: usize) -> usize {
        if column == 0 {
            return start;
        }
        let mut remaining = column;
        for chunk in self.chunks(start, end) {
            let text = String::from_utf8_lossy(chunk.bytes);
            for (byte, _) in text.char_indices() {
                if remaining == 0 {
                    return chunk.doc_start + byte;
                }
                remaining -= 1;
            }
            if remaining == 0 {
                return chunk.doc_start + chunk.bytes.len();
            }
        }
        end
    }

    fn byte_at(&self, offset: usize) -> Option<u8> {
        if offset >= self.len_bytes {
            return None;
        }
        let mut node = self.root.as_deref();
        let mut base = 0;
        while let Some(current) = node {
            let left_bytes = link_bytes(&current.left);
            if offset < base + left_bytes {
                node = current.left.as_deref();
                continue;
            }
            let piece_start = base + left_bytes;
            if offset < piece_start + current.piece.len {
                let source_offset = current.piece.start + offset - piece_start;
                return Some(self.source_slice(current.piece.source)[source_offset]);
            }
            base = piece_start + current.piece.len;
            node = current.right.as_deref();
        }
        None
    }

    fn text_range(&self, start: usize, end: usize) -> String {
        if start == end {
            return String::new();
        }
        let chunks = self.chunks(start, end);
        let mut bytes = Vec::with_capacity(end - start);
        for chunk in chunks {
            bytes.extend_from_slice(chunk.bytes);
        }
        String::from_utf8_lossy(&bytes).into_owned()
    }

    fn chunks(&self, start: usize, end: usize) -> Vec<Chunk<'_>> {
        let mut chunks = Vec::new();
        if start < end {
            collect_chunks(
                &self.root,
                0,
                start,
                end,
                &self.original,
                &self.added,
                &mut chunks,
            );
        }
        chunks
    }

    fn source_slice(&self, source: Source) -> &[u8] {
        match source {
            Source::Original => &self.original.data,
            Source::Added => &self.added,
        }
    }

    pub fn write_to<W: Write>(&self, writer: &mut W) -> io::Result<()> {
        let mut stack = Vec::new();
        let mut current = self.root.as_deref();
        loop {
            while let Some(node) = current {
                stack.push(node);
                current = node.left.as_deref();
            }
            let Some(node) = stack.pop() else { break };
            let piece = node.piece;
            writer.write_all(
                &self.source_slice(piece.source)[piece.start..piece.start + piece.len],
            )?;
            current = node.right.as_deref();
        }
        Ok(())
    }
}

struct Chunk<'a> {
    doc_start: usize,
    source: Source,
    source_start: usize,
    bytes: &'a [u8],
}

fn collect_chunks<'a>(
    root: &'a Link,
    base: usize,
    start: usize,
    end: usize,
    original: &'a OriginalFile,
    added: &'a [u8],
    output: &mut Vec<Chunk<'a>>,
) {
    let Some(node) = root else { return };
    let left_bytes = link_bytes(&node.left);
    let piece_start = base + left_bytes;
    let piece_end = piece_start + node.piece.len;
    if start < piece_start && end > base {
        collect_chunks(&node.left, base, start, end, original, added, output);
    }
    if start < piece_end && end > piece_start {
        let intersection_start = start.max(piece_start);
        let intersection_end = end.min(piece_end);
        let source_start = node.piece.start + intersection_start - piece_start;
        let len = intersection_end - intersection_start;
        let source = node.piece.source;
        let data = match source {
            Source::Original => &original.data,
            Source::Added => added,
        };
        output.push(Chunk {
            doc_start: intersection_start,
            source,
            source_start,
            bytes: &data[source_start..source_start + len],
        });
    }
    if end > piece_end {
        collect_chunks(&node.right, piece_end, start, end, original, added, output);
    }
}

fn select_line(
    root: &Link,
    line: usize,
    base: usize,
    original: &OriginalFile,
    added_line_starts: &[usize],
) -> usize {
    let node = root.as_ref().expect("line is present in the piece tree");
    let left_bytes = link_bytes(&node.left);
    let left_newlines = link_newlines(&node.left);
    if line <= left_newlines {
        return select_line(&node.left, line, base, original, added_line_starts);
    }
    let remaining = line - left_newlines;
    let piece_start = base + left_bytes;
    if remaining <= node.piece.newlines {
        let after_newline = nth_newline_after(
            node.piece.source,
            node.piece.start,
            remaining,
            original,
            added_line_starts,
        );
        piece_start + after_newline - node.piece.start
    } else {
        select_line(
            &node.right,
            remaining - node.piece.newlines,
            piece_start + node.piece.len,
            original,
            added_line_starts,
        )
    }
}

fn split(
    root: Link,
    offset: usize,
    original: &OriginalFile,
    added_line_starts: &[usize],
) -> (Link, Link) {
    let Some(mut node) = root else {
        return (None, None);
    };
    let left_bytes = link_bytes(&node.left);
    let piece_end = left_bytes + node.piece.len;
    if offset < left_bytes {
        let (left, right) = split(node.left.take(), offset, original, added_line_starts);
        node.left = right;
        node.update();
        (left, Some(node))
    } else if offset == left_bytes {
        let left = node.left.take();
        node.update();
        (left, Some(node))
    } else if offset < piece_end {
        let local = offset - left_bytes;
        let left_piece = make_piece(
            node.piece.source,
            node.piece.start,
            node.piece.start + local,
            original,
            added_line_starts,
        );
        let right_piece = make_piece(
            node.piece.source,
            node.piece.start + local,
            node.piece.start + node.piece.len,
            original,
            added_line_starts,
        );
        let mut left = Box::new(Node::new(left_piece, node.priority));
        left.left = node.left.take();
        left.update();
        let mut right = Box::new(Node::new(right_piece, node.priority));
        right.right = node.right.take();
        right.update();
        (Some(left), Some(right))
    } else if offset == piece_end {
        let right = node.right.take();
        node.update();
        (Some(node), right)
    } else {
        let (left, right) = split(
            node.right.take(),
            offset - piece_end,
            original,
            added_line_starts,
        );
        node.right = left;
        node.update();
        (Some(node), right)
    }
}

fn merge(left: Link, right: Link) -> Link {
    match (left, right) {
        (None, right) => right,
        (left, None) => left,
        (Some(mut left), Some(mut right)) => {
            if left.priority >= right.priority {
                left.right = merge(left.right.take(), Some(right));
                left.update();
                Some(left)
            } else {
                right.left = merge(Some(left), right.left.take());
                right.update();
                Some(right)
            }
        }
    }
}

fn make_piece(
    source: Source,
    start: usize,
    end: usize,
    original: &OriginalFile,
    added_line_starts: &[usize],
) -> Piece {
    let starts = match source {
        Source::Original => original.line_starts.as_slice(),
        Source::Added => added_line_starts,
    };
    Piece {
        source,
        start,
        len: end - start,
        newlines: newline_rank(starts, end) - newline_rank(starts, start),
    }
}

fn nth_newline_after(
    source: Source,
    start: usize,
    nth: usize,
    original: &OriginalFile,
    added_line_starts: &[usize],
) -> usize {
    let starts = match source {
        Source::Original => original.line_starts.as_slice(),
        Source::Added => added_line_starts,
    };
    starts[newline_rank(starts, start) + nth]
}

fn newline_rank(starts: &[usize], byte: usize) -> usize {
    starts
        .partition_point(|start| *start <= byte)
        .saturating_sub(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(text: &str) -> (tempfile::NamedTempFile, PieceTable) {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(text.as_bytes()).unwrap();
        file.flush().unwrap();
        let table = PieceTable::open(file.as_file()).unwrap();
        (file, table)
    }

    fn contents(table: &PieceTable) -> String {
        let mut bytes = Vec::new();
        table.write_to(&mut bytes).unwrap();
        String::from_utf8(bytes).unwrap()
    }

    #[test]
    fn edits_across_piece_boundaries() {
        let (_file, mut table) = fixture("alpha\nbeta\ngamma\n");
        table.replace(6, 10, "BETA\nINSERTED");
        assert_eq!(contents(&table), "alpha\nBETA\nINSERTED\ngamma\n");
        assert_eq!(table.len_lines(), 5);
        assert_eq!(table.line(2), "INSERTED");
        assert_eq!(table.line_len_chars(1), 4);
        assert_eq!(table.line_slice_chars(2, 1, 4), "NSE");

        table.replace(0, table.len_bytes(), "single");
        assert_eq!(contents(&table), "single");
        assert_eq!(table.len_lines(), 1);
    }

    #[test]
    fn compaction_preserves_content_navigation_and_future_edits() {
        let (_file, mut table) = fixture("zero\none\ntwo\nthree\n");
        for step in 0..200 {
            let line = step % table.len_lines();
            let byte = table.position_to_byte(line, 0);
            table.replace(byte, byte, &format!("{step}:привет-"));
        }
        let expected = contents(&table);
        assert!(table.fragment_count() > 200);

        let mut compacted = table.compact().unwrap();
        assert_eq!(contents(&compacted), expected);
        assert_eq!(compacted.fragment_count(), 1);
        assert_eq!(compacted.len_lines(), table.len_lines());
        for line in 0..table.len_lines() {
            assert_eq!(compacted.line(line), table.line(line));
            assert_eq!(compacted.line_len_chars(line), table.line_len_chars(line));
            assert_eq!(
                compacted.line_slice_chars(line, 1, 8),
                table.line_slice_chars(line, 1, 8)
            );
        }

        let byte = compacted.position_to_byte(2, 3);
        compacted.replace(byte, byte, "🚀\nновая строка");
        assert!(contents(&compacted).contains("🚀\nновая строка"));
        assert_eq!(compacted.len_lines(), table.len_lines() + 1);
    }

    #[test]
    fn randomized_operations_match_string_model() {
        let (_file, mut table) = fixture("0123456789\nabcdefghij\nABCDEFGHIJ\n");
        let mut model = "0123456789\nabcdefghij\nABCDEFGHIJ\n".to_owned();
        let mut state = 17_u64;
        for step in 0..300 {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            let start = (state as usize) % (model.len() + 1);
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            let max_delete = model.len() - start;
            let delete = if max_delete == 0 {
                0
            } else {
                (state as usize) % (max_delete + 1).min(8)
            };
            let inserted = if step % 7 == 0 { "X\nY" } else { "z" };
            if start + delete <= model.len() {
                table.replace(start, start + delete, inserted);
                model.replace_range(start..start + delete, inserted);
                assert_eq!(contents(&table), model, "step {step}");
                assert_eq!(
                    table.len_lines(),
                    model.bytes().filter(|byte| *byte == b'\n').count() + 1
                );
            }
        }
    }
}
