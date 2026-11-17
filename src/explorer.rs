use std::collections::{BTreeMap, HashSet};

use eframe::egui::{Color32, RichText};
use egui_material_icons::icons::{
    ICON_CHEVRON_RIGHT, ICON_CODE, ICON_DATA_OBJECT, ICON_DESCRIPTION, ICON_EXPAND_MORE,
    ICON_FOLDER, ICON_FOLDER_OPEN, ICON_IMAGE, ICON_INSERT_DRIVE_FILE,
};

use crate::{project::Project, theme};

#[derive(Default)]
struct Directory {
    directories: BTreeMap<String, Directory>,
    files: BTreeMap<String, usize>,
}

impl Directory {
    fn insert(&mut self, relative: &str, file_index: usize) {
        let mut components = relative.split('/').peekable();
        let mut current = self;
        while let Some(component) = components.next() {
            if components.peek().is_none() {
                current.files.insert(component.to_owned(), file_index);
            } else {
                current = current.directories.entry(component.to_owned()).or_default();
            }
        }
    }
}

pub struct ExplorerRow {
    pub name: String,
    pub relative: String,
    pub depth: usize,
    pub expanded: bool,
    pub file_index: Option<usize>,
}

impl ExplorerRow {
    pub fn is_directory(&self) -> bool {
        self.file_index.is_none()
    }
}

#[derive(Default)]
pub struct ProjectExplorer {
    root: Directory,
    expanded: HashSet<String>,
    rows: Vec<ExplorerRow>,
    indexed_files: usize,
}

impl ProjectExplorer {
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    pub fn sync(&mut self, project: &Project) {
        if self.indexed_files > project.files.len() {
            self.reset();
        }
        let previous_len = self.indexed_files;
        while self.indexed_files < project.files.len() {
            let file = &project.files[self.indexed_files];
            self.root.insert(&file.relative, self.indexed_files);
            self.indexed_files += 1;
        }
        if previous_len != self.indexed_files {
            self.rebuild(project);
        }
    }

    pub fn rebuild(&mut self, project: &Project) {
        self.rows.clear();
        if !project.filter().trim().is_empty() {
            self.rows
                .extend(project.visible_indices().iter().map(|&index| {
                    let file = &project.files[index];
                    ExplorerRow {
                        name: file.relative.clone(),
                        relative: file.relative.clone(),
                        depth: 0,
                        expanded: false,
                        file_index: Some(index),
                    }
                }));
            return;
        }
        append_rows(&self.root, "", project, &self.expanded, 0, &mut self.rows);
    }

    pub fn toggle_directory(&mut self, relative: &str, project: &Project) {
        if !self.expanded.remove(relative) {
            self.expanded.insert(relative.to_owned());
        }
        self.rebuild(project);
    }

    pub fn rows(&self) -> &[ExplorerRow] {
        &self.rows
    }
}

fn append_rows(
    directory: &Directory,
    parent: &str,
    project: &Project,
    expanded: &HashSet<String>,
    depth: usize,
    rows: &mut Vec<ExplorerRow>,
) {
    for (name, child) in &directory.directories {
        let relative = if parent.is_empty() {
            name.clone()
        } else {
            format!("{parent}/{name}")
        };
        let is_expanded = expanded.contains(&relative);
        rows.push(ExplorerRow {
            name: name.clone(),
            relative: relative.clone(),
            depth,
            expanded: is_expanded,
            file_index: None,
        });
        if is_expanded {
            append_rows(child, &relative, project, expanded, depth + 1, rows);
        }
    }
    for (name, &file_index) in &directory.files {
        rows.push(ExplorerRow {
            name: name.clone(),
            relative: project.files[file_index].relative.clone(),
            depth,
            expanded: false,
            file_index: Some(file_index),
        });
    }
}

pub fn folder_icon(expanded: bool) -> RichText {
    RichText::new(if expanded {
        ICON_FOLDER_OPEN
    } else {
        ICON_FOLDER
    })
    .color(theme::INDIGO)
}

pub fn chevron_icon(expanded: bool) -> RichText {
    RichText::new(if expanded {
        ICON_EXPAND_MORE
    } else {
        ICON_CHEVRON_RIGHT
    })
    .color(theme::MUTED)
}

pub fn file_icon(path: &str) -> RichText {
    let extension = path.rsplit('.').next().unwrap_or_default();
    let (icon, color) = match extension {
        "rs" | "py" | "js" | "jsx" | "ts" | "tsx" | "go" | "c" | "h" | "cc" | "cpp" | "cs"
        | "java" | "kt" | "swift" | "rb" | "php" | "zig" | "lua" | "sh" => (ICON_CODE, theme::CYAN),
        "json" | "jsonc" | "toml" | "yaml" | "yml" | "xml" | "html" | "css" => {
            (ICON_DATA_OBJECT, theme::VIOLET)
        }
        "md" | "markdown" | "txt" => (ICON_DESCRIPTION, theme::MUTED),
        "png" | "jpg" | "jpeg" | "gif" | "svg" | "webp" => {
            (ICON_IMAGE, Color32::from_rgb(52, 211, 153))
        }
        _ => (ICON_INSERT_DRIVE_FILE, theme::MUTED),
    };
    RichText::new(icon).color(color)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directories_expand_without_losing_file_order() {
        let mut root = Directory::default();
        root.insert("src/main.rs", 0);
        root.insert("src/domain/editor.rs", 1);
        root.insert("Cargo.toml", 2);
        assert_eq!(root.directories.len(), 1);
        assert_eq!(root.files.len(), 1);
        let src = &root.directories["src"];
        assert_eq!(src.files["main.rs"], 0);
        assert_eq!(src.directories["domain"].files["editor.rs"], 1);
    }

    #[test]
    fn file_icons_cover_code_and_documents() {
        assert_eq!(file_icon("main.rs").text(), ICON_CODE);
        assert_eq!(file_icon("README.md").text(), ICON_DESCRIPTION);
        assert_eq!(file_icon("Cargo.toml").text(), ICON_DATA_OBJECT);
    }
}
