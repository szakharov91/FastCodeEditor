use std::{
    ops::Range,
    path::{Path, PathBuf},
    sync::Arc,
    thread,
};

use crossbeam_channel::{Receiver, Sender, unbounded};
use eframe::egui::{self, Color32};
use syntect::{
    easy::HighlightLines,
    highlighting::{Theme, ThemeSet},
    parsing::SyntaxSet,
};

use crate::document::MAX_HIGHLIGHT_LINE_BYTES;

#[derive(Clone, Debug)]
pub struct HighlightSpan {
    pub byte_range: Range<usize>,
    pub color: Color32,
}

pub struct HighlightedLine {
    pub spans: Vec<HighlightSpan>,
}

pub struct HighlightResult {
    pub document_id: u64,
    pub revision: u64,
    pub start_line: usize,
    pub language: String,
    pub lines: Vec<HighlightedLine>,
}

struct HighlightRequest {
    document_id: u64,
    path: PathBuf,
    revision: u64,
    start_line: usize,
    lines: Vec<String>,
}

pub struct HighlightWorker {
    request_tx: Sender<HighlightRequest>,
    result_rx: Receiver<HighlightResult>,
}

impl HighlightWorker {
    pub fn new(context: &egui::Context) -> Self {
        let (request_tx, request_rx) = unbounded::<HighlightRequest>();
        let (result_tx, result_rx) = unbounded::<HighlightResult>();
        let context = context.clone();

        thread::Builder::new()
            .name("fce-syntax".into())
            .spawn(move || {
                let syntax_set = Arc::new(two_face::syntax::extra_newlines());
                let themes = ThemeSet::load_defaults();
                let theme = Arc::new(
                    themes
                        .themes
                        .get("base16-ocean.dark")
                        .or_else(|| themes.themes.values().next())
                        .expect("syntect default themes are not empty")
                        .clone(),
                );

                while let Ok(mut request) = request_rx.recv() {
                    // A fast scroll can produce many ranges. Only the newest visible
                    // range matters, so stale work is discarded before parsing.
                    while let Ok(newer) = request_rx.try_recv() {
                        request = newer;
                    }
                    let result = highlight(request, &syntax_set, &theme);
                    if result_tx.send(result).is_err() {
                        break;
                    }
                    context.request_repaint();
                }
            })
            .expect("failed to start syntax worker");

        Self {
            request_tx,
            result_rx,
        }
    }

    pub fn request(
        &self,
        document_id: u64,
        path: PathBuf,
        revision: u64,
        start_line: usize,
        lines: Vec<String>,
    ) {
        let _ = self.request_tx.send(HighlightRequest {
            document_id,
            path,
            revision,
            start_line,
            lines,
        });
    }

    pub fn try_recv(&self) -> Option<HighlightResult> {
        self.result_rx.try_recv().ok()
    }
}

fn highlight(request: HighlightRequest, syntax_set: &SyntaxSet, theme: &Theme) -> HighlightResult {
    let syntax = syntax_for_path(syntax_set, &request.path);
    let language = syntax.map_or_else(|| "Plain Text".to_owned(), |syntax| syntax.name.clone());
    let mut highlighter = syntax.map(|syntax| HighlightLines::new(syntax, theme));
    let mut lines = Vec::with_capacity(request.lines.len());

    for line in request.lines {
        let mut spans = Vec::new();
        if line.len() <= MAX_HIGHLIGHT_LINE_BYTES
            && let Some(highlighter) = &mut highlighter
        {
            let mut with_newline = line;
            with_newline.push('\n');
            if let Ok(regions) = highlighter.highlight_line(&with_newline, syntax_set) {
                let mut offset = 0;
                let visible_len = with_newline.len() - 1;
                for (style, part) in regions {
                    let start = offset;
                    offset += part.len();
                    let end = offset.min(visible_len);
                    if start < end {
                        spans.push(HighlightSpan {
                            byte_range: start..end,
                            color: Color32::from_rgb(
                                style.foreground.r,
                                style.foreground.g,
                                style.foreground.b,
                            ),
                        });
                    }
                }
            }
        }
        lines.push(HighlightedLine { spans });
    }

    HighlightResult {
        document_id: request.document_id,
        revision: request.revision,
        start_line: request.start_line,
        language,
        lines,
    }
}

fn syntax_for_path<'a>(
    syntax_set: &'a SyntaxSet,
    path: &Path,
) -> Option<&'a syntect::parsing::SyntaxReference> {
    let file_name = path.file_name()?.to_string_lossy();
    let special_extension = match file_name.as_ref() {
        "Dockerfile" | "Containerfile" => Some("Dockerfile"),
        "Makefile" | "makefile" => Some("Makefile"),
        "CMakeLists.txt" => Some("cmake"),
        _ => None,
    };
    special_extension
        .and_then(|extension| syntax_set.find_syntax_by_extension(extension))
        .or_else(|| {
            path.extension()
                .and_then(|extension| extension.to_str())
                .and_then(|extension| syntax_set.find_syntax_by_extension(extension))
        })
}

pub fn language_label(path: &Path) -> &'static str {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    if matches!(file_name, "Dockerfile" | "Containerfile") {
        return "Dockerfile";
    }
    if file_name == "CMakeLists.txt" {
        return "CMake";
    }
    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
    {
        "rs" => "Rust",
        "py" | "pyi" | "pyw" => "Python",
        "js" | "mjs" | "cjs" | "jsx" => "JavaScript",
        "ts" | "mts" | "cts" | "tsx" => "TypeScript",
        "go" => "Go",
        "c" | "h" => "C",
        "cc" | "cpp" | "cxx" | "hpp" | "hh" => "C++",
        "cs" => "C#",
        "java" => "Java",
        "kt" | "kts" => "Kotlin",
        "swift" => "Swift",
        "rb" => "Ruby",
        "php" => "PHP",
        "zig" => "Zig",
        "lua" => "Lua",
        "sh" | "bash" | "zsh" => "Shell",
        "json" | "jsonc" => "JSON",
        "toml" => "TOML",
        "yaml" | "yml" => "YAML",
        "html" | "htm" => "HTML",
        "css" => "CSS",
        "scss" | "sass" => "Sass",
        "md" | "markdown" => "Markdown",
        "sql" => "SQL",
        "xml" => "XML",
        "vue" => "Vue",
        "svelte" => "Svelte",
        _ => "Plain Text",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn detects_modern_language_names() {
        assert_eq!(language_label(&PathBuf::from("main.rs")), "Rust");
        assert_eq!(
            language_label(&PathBuf::from("component.tsx")),
            "TypeScript"
        );
        assert_eq!(language_label(&PathBuf::from("Dockerfile")), "Dockerfile");
    }

    #[test]
    fn default_syntaxes_cover_common_modern_languages() {
        let syntaxes = two_face::syntax::extra_newlines();
        for extension in [
            "rs", "py", "js", "jsx", "ts", "tsx", "go", "java", "cs", "kt", "swift", "zig", "dart",
            "vue", "svelte", "json", "toml", "yaml", "proto", "graphql",
        ] {
            assert!(
                syntaxes.find_syntax_by_extension(extension).is_some(),
                "missing {extension}"
            );
        }
    }
}
