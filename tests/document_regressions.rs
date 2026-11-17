use std::{fs, io::Write};

use fast_code_editor::document::{Document, Position, save_snapshot};

#[test]
fn unicode_edit_undo_redo_and_atomic_save() {
    let mut file = tempfile::NamedTempFile::new().unwrap();
    write!(file, "fn main() {{\n    println!(\"Привет, мир!\");\n}}\n").unwrap();
    let mut document = Document::open(file.path(), 42).unwrap();
    let line_len = document.line_len_chars(1);
    document
        .replace(
            Position::new(1, line_len),
            Position::new(1, line_len),
            " // ✓",
        )
        .unwrap();
    assert!(document.line(1).ends_with("// ✓"));

    let snapshot = document.snapshot();
    let bytes = save_snapshot(&snapshot, file.path()).unwrap();
    document.mark_saved(bytes);
    assert!(!document.dirty);
    assert!(fs::read_to_string(file.path()).unwrap().contains("// ✓"));

    document.undo().unwrap();
    assert!(!document.line(1).contains("// ✓"));
    document.redo().unwrap();
    assert!(document.line(1).contains("// ✓"));
}

#[test]
fn multiline_selection_round_trip() {
    let mut file = tempfile::NamedTempFile::new().unwrap();
    write!(file, "alpha\nbeta\ngamma").unwrap();
    let mut document = Document::open(file.path(), 43).unwrap();
    let selected = document.text_range(Position::new(0, 2), Position::new(2, 3));
    assert_eq!(selected, "pha\nbeta\ngam");
    document
        .replace(Position::new(0, 2), Position::new(2, 3), "REPLACEMENT")
        .unwrap();
    assert_eq!(document.line(0), "alREPLACEMENTma");
    assert_eq!(document.len_lines(), 1);
}
