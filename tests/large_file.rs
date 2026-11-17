use std::{
    fs,
    io::{BufWriter, Write},
};

use fast_code_editor::document::{Document, LARGE_FILE_THRESHOLD, Position, save_snapshot};

#[test]
#[ignore = "creates a 70+ MiB load-test fixture"]
fn edits_and_saves_large_piece_table_file_with_random_access() {
    let file = tempfile::NamedTempFile::new().unwrap();
    {
        let mut writer = BufWriter::with_capacity(1024 * 1024, file.as_file());
        for line in 0..1_000_000 {
            writeln!(
                writer,
                "{line:08}: The quick brown fox jumps over the lazy dog — 0123456789abcdef"
            )
            .unwrap();
        }
        writer.flush().unwrap();
    }
    assert!(file.as_file().metadata().unwrap().len() > LARGE_FILE_THRESHOLD);

    let mut document = Document::open(file.path(), 99).unwrap();
    assert!(!document.read_only);
    assert_eq!(document.backend_label(), "PIECE TABLE · mmap");
    assert_eq!(document.len_lines(), 1_000_001);
    assert!(document.line(750_000).starts_with("00750000:"));
    assert!(document.line(999_999).starts_with("00999999:"));
    assert_eq!(document.line(1_000_000), "");

    document
        .replace(
            Position::new(750_000, 0),
            Position::new(750_000, 0),
            "EDITED ✓ ",
        )
        .unwrap();
    document
        .replace(
            Position::new(999_999, 0),
            Position::new(1_000_000, 0),
            "LAST LINE\nNEW EOF",
        )
        .unwrap();
    assert!(document.line(750_000).starts_with("EDITED ✓ 00750000:"));
    assert_eq!(document.line(999_999), "LAST LINE");
    assert_eq!(document.line(1_000_000), "NEW EOF");

    let snapshot = document.snapshot();
    save_snapshot(&snapshot, file.path()).unwrap();
    let saved = fs::read_to_string(file.path()).unwrap();
    assert!(saved.contains("EDITED ✓ 00750000:"));
    assert!(saved.ends_with("LAST LINE\nNEW EOF"));
}
