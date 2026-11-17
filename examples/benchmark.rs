use std::{path::PathBuf, time::Instant};

use fast_code_editor::document::Document;

fn main() {
    let path = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .expect("usage: cargo run --release --example benchmark -- <text-file>");
    let document = Document::open(&path, 1).expect("failed to open benchmark file");
    println!("file: {}", path.display());
    println!("bytes: {}", document.byte_len);
    println!("lines: {}", document.len_lines());
    println!("open_ms: {:.3}", document.load_time.as_secs_f64() * 1000.0);
    println!("backend: {}", document.backend_label());
    println!("read_only: {}", document.read_only);

    let started = Instant::now();
    let mut checksum = 0_usize;
    for step in 0_usize..10_000 {
        let line = (step.wrapping_mul(1_000_003)) % document.len_lines();
        checksum = checksum.wrapping_add(document.line_len_chars(line));
        if step % 100 == 0 {
            checksum = checksum.wrapping_add(document.line(line).len());
        }
    }
    println!(
        "random_access_ms: {:.3}",
        started.elapsed().as_secs_f64() * 1000.0
    );
    println!("checksum: {checksum}");
}
