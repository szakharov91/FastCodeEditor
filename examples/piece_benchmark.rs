use std::{path::PathBuf, thread, time::Instant};

use fast_code_editor::document::{Document, Position, save_snapshot};

fn main() {
    let mut arguments = std::env::args_os().skip(1);
    let input = arguments
        .next()
        .map(PathBuf::from)
        .expect("usage: cargo run --release --example piece_benchmark -- <input> <output>");
    let output = arguments
        .next()
        .map(PathBuf::from)
        .expect("an output path is required; the input file is never modified");
    let mut document = Document::open(&input, 1).expect("failed to open benchmark file");
    println!("backend: {}", document.backend_label());
    println!("bytes: {}", document.byte_len);
    println!("lines: {}", document.len_lines());
    println!("open_ms: {:.3}", document.load_time.as_secs_f64() * 1000.0);

    let started = Instant::now();
    for step in 0_usize..5_000 {
        let line = step.wrapping_mul(1_000_003) % (document.len_lines() - 1);
        document
            .replace(Position::new(line, 0), Position::new(line, 0), "x")
            .unwrap();
    }
    println!(
        "edits_5000_ms: {:.3}",
        started.elapsed().as_secs_f64() * 1000.0
    );

    let snapshot_started = Instant::now();
    let snapshot = document.snapshot();
    println!(
        "snapshot_ms: {:.3}",
        snapshot_started.elapsed().as_secs_f64() * 1000.0
    );
    let save = thread::spawn(move || {
        let started = Instant::now();
        let result = save_snapshot(&snapshot, &output);
        (result, started.elapsed())
    });

    let concurrent_started = Instant::now();
    for line in 0_usize..100 {
        document
            .replace(
                Position::new(line, 0),
                Position::new(line, 0),
                "concurrent ",
            )
            .unwrap();
    }
    println!(
        "concurrent_edits_100_ms: {:.3}",
        concurrent_started.elapsed().as_secs_f64() * 1000.0
    );
    let (result, save_time) = save.join().expect("save thread panicked");
    let bytes = result.expect("failed to save benchmark output");
    println!(
        "background_save_ms: {:.3}",
        save_time.as_secs_f64() * 1000.0
    );
    println!("saved_bytes: {bytes}");
    println!("current_document_bytes: {}", document.byte_len);

    println!("fragments_before: {:?}", document.fragment_count());
    let compaction_started = Instant::now();
    let compacted = Document::compact_snapshot(&document.snapshot())
        .expect("failed to compact benchmark document");
    println!(
        "compaction_ms: {:.3}",
        compaction_started.elapsed().as_secs_f64() * 1000.0
    );
    assert!(document.apply_compaction(compacted));
    println!("fragments_after: {:?}", document.fragment_count());
}
