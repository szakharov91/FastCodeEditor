use std::io::{BufWriter, Write};

use fast_code_editor::document::{Document, LARGE_FILE_THRESHOLD};

#[test]
#[ignore = "creates a 70+ MiB load-test fixture"]
fn opens_large_file_through_mmap_with_random_access() {
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

    let document = Document::open(file.path(), 99).unwrap();
    assert!(document.read_only);
    assert_eq!(document.len_lines(), 1_000_001);
    assert!(document.line(750_000).starts_with("00750000:"));
    assert!(document.line(999_999).starts_with("00999999:"));
    assert_eq!(document.line(1_000_000), "");
}
