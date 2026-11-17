use std::{
    path::{Path, PathBuf},
    thread,
    time::Instant,
};

use crossbeam_channel::{Receiver, unbounded};
use eframe::egui;
use ignore::WalkBuilder;

const BATCH_SIZE: usize = 512;
const MAX_PROJECT_FILES: usize = 500_000;

pub struct ProjectFile {
    pub path: PathBuf,
    pub relative: String,
    folded: String,
}

enum ScanEvent {
    Batch(Vec<ProjectFile>),
    Finished { truncated: bool },
}

pub struct Project {
    pub root: PathBuf,
    pub files: Vec<ProjectFile>,
    pub scanning: bool,
    pub truncated: bool,
    pub elapsed: Option<std::time::Duration>,
    filter: String,
    filtered: Vec<usize>,
    scan_rx: Receiver<ScanEvent>,
}

impl Project {
    pub fn open(root: PathBuf, context: &egui::Context) -> Self {
        let (scan_tx, scan_rx) = unbounded();
        let context = context.clone();
        let scan_root = root.clone();
        thread::Builder::new()
            .name("fce-project-scan".into())
            .spawn(move || {
                let walker = WalkBuilder::new(&scan_root)
                    .hidden(true)
                    .git_ignore(true)
                    .git_global(true)
                    .git_exclude(true)
                    .follow_links(false)
                    .filter_entry(|entry| {
                        !matches!(
                            entry.file_name().to_str(),
                            Some(".git" | "target" | "node_modules" | ".idea" | ".next" | "dist")
                        )
                    })
                    .build();
                let mut batch = Vec::with_capacity(BATCH_SIZE);
                let mut total = 0_usize;
                let mut truncated = false;

                for entry in walker {
                    let Ok(entry) = entry else { continue };
                    if !entry.file_type().is_some_and(|kind| kind.is_file()) {
                        continue;
                    }
                    if total == MAX_PROJECT_FILES {
                        truncated = true;
                        break;
                    }
                    let path = entry.into_path();
                    let relative_path = path.strip_prefix(&scan_root).unwrap_or(&path);
                    let relative = relative_path.to_string_lossy().replace('\\', "/");
                    batch.push(ProjectFile {
                        folded: relative.to_lowercase(),
                        path,
                        relative,
                    });
                    total += 1;
                    if batch.len() == BATCH_SIZE {
                        if scan_tx
                            .send(ScanEvent::Batch(std::mem::take(&mut batch)))
                            .is_err()
                        {
                            return;
                        }
                        batch = Vec::with_capacity(BATCH_SIZE);
                        context.request_repaint();
                    }
                }
                if !batch.is_empty() {
                    let _ = scan_tx.send(ScanEvent::Batch(batch));
                }
                let _ = scan_tx.send(ScanEvent::Finished { truncated });
                context.request_repaint();
            })
            .expect("failed to start project scanner");

        Self {
            root,
            files: Vec::new(),
            scanning: true,
            truncated: false,
            elapsed: None,
            filter: String::new(),
            filtered: Vec::new(),
            scan_rx,
        }
    }

    pub fn poll(&mut self, started: Instant) -> bool {
        let mut changed = false;
        while let Ok(event) = self.scan_rx.try_recv() {
            match event {
                ScanEvent::Batch(batch) => {
                    self.files.extend(batch);
                    changed = true;
                }
                ScanEvent::Finished { truncated } => {
                    self.scanning = false;
                    self.truncated = truncated;
                    self.elapsed = Some(started.elapsed());
                    changed = true;
                }
            }
        }
        if changed {
            self.rebuild_filter();
        }
        changed
    }

    pub fn set_filter(&mut self, filter: String) {
        if self.filter != filter {
            self.filter = filter;
            self.rebuild_filter();
        }
    }

    pub fn filter(&self) -> &str {
        &self.filter
    }

    pub fn visible_indices(&self) -> &[usize] {
        &self.filtered
    }

    fn rebuild_filter(&mut self) {
        let query = self.filter.trim().to_lowercase();
        self.filtered = self
            .files
            .iter()
            .enumerate()
            .filter(|(_, file)| query.is_empty() || file.folded.contains(&query))
            .map(|(index, _)| index)
            .collect();
    }

    pub fn fuzzy_matches(&self, query: &str, limit: usize) -> Vec<usize> {
        let query = query.trim().to_lowercase();
        if query.is_empty() {
            return (0..self.files.len().min(limit)).collect();
        }
        let mut scored: Vec<(i32, usize)> = self
            .files
            .iter()
            .enumerate()
            .filter_map(|(index, file)| {
                fuzzy_score(&file.folded, &query).map(|score| (score, index))
            })
            .collect();
        scored.sort_unstable_by(|left, right| {
            right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1))
        });
        scored.truncate(limit);
        scored.into_iter().map(|(_, index)| index).collect()
    }
}

fn fuzzy_score(candidate: &str, query: &str) -> Option<i32> {
    let mut query_chars = query.chars();
    let mut current = query_chars.next()?;
    let mut score = 0_i32;
    let mut previous_match = None;

    for (index, character) in candidate.char_indices() {
        if character != current {
            continue;
        }
        score += 8;
        if previous_match.is_some_and(|previous| previous + 1 == index) {
            score += 6;
        }
        if index == 0 || candidate[..index].ends_with('/') {
            score += 10;
        }
        previous_match = Some(index);
        match query_chars.next() {
            Some(next) => current = next,
            None => {
                score -= (candidate.len().saturating_sub(query.len()) / 16) as i32;
                return Some(score);
            }
        }
    }
    None
}

pub fn is_probably_text(path: &Path) -> bool {
    !matches!(
        path.extension().and_then(|extension| extension.to_str()),
        Some(
            "png"
                | "jpg"
                | "jpeg"
                | "gif"
                | "webp"
                | "ico"
                | "pdf"
                | "zip"
                | "gz"
                | "xz"
                | "7z"
                | "exe"
                | "dll"
                | "so"
                | "dylib"
                | "class"
                | "jar"
                | "woff"
                | "woff2"
                | "ttf"
                | "otf"
        )
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzzy_search_prefers_path_component_start() {
        let file_start = fuzzy_score("src/main.rs", "main").unwrap();
        let middle = fuzzy_score("src/domain.rs", "main").unwrap();
        assert!(file_start > middle);
    }
}
