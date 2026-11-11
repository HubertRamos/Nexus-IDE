//! Explorador de arquivos lateral.
//!
//! O explorador mantém apenas o modelo da árvore e o estado de navegação. A
//! renderização fica na TUI e as decisões sobre documentos abertos ficam na
//! aplicação. Operações destrutivas usam a lixeira do sistema por padrão.

#![allow(dead_code)]

use std::collections::BTreeSet;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const DEFAULT_WIDTH: u16 = 30;
pub const MIN_WIDTH: u16 = 16;
pub const MAX_WIDTH: u16 = 80;
const MAX_DEPTH: usize = 64;

pub const DEFAULT_EXCLUDED_DIRS: [&str; 7] = [
    ".git",
    "node_modules",
    "target",
    ".venv",
    "__pycache__",
    "dist",
    "build",
];

pub fn is_default_excluded_dir(name: &str) -> bool {
    DEFAULT_EXCLUDED_DIRS.contains(&name)
}

#[derive(Debug)]
pub enum ExplorerError {
    EmptyName,
    InvalidName {
        name: String,
    },
    AlreadyExists {
        path: PathBuf,
    },
    NotFound {
        path: PathBuf,
    },
    Io {
        action: &'static str,
        path: PathBuf,
        source: io::Error,
    },
    Trash {
        path: PathBuf,
        message: String,
    },
}

impl fmt::Display for ExplorerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExplorerError::EmptyName => write!(formatter, "o nome não pode ficar vazio"),
            ExplorerError::InvalidName { name } => write!(
                formatter,
                "nome inválido: `{name}`; use apenas um nome, sem separadores de caminho"
            ),
            ExplorerError::AlreadyExists { path } => {
                write!(formatter, "já existe um item em `{}`", path.display())
            }
            ExplorerError::NotFound { path } => {
                write!(formatter, "o item não existe mais: `{}`", path.display())
            }
            ExplorerError::Io {
                action,
                path,
                source,
            } => write!(
                formatter,
                "falha ao {action} `{}`: {source}",
                path.display()
            ),
            ExplorerError::Trash { path, message } => write!(
                formatter,
                "não foi possível mover `{}` para a lixeira: {message}",
                path.display()
            ),
        }
    }
}

impl std::error::Error for ExplorerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ExplorerError::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExplorerEntry {
    pub path: PathBuf,
    pub name: String,
    pub depth: usize,
    pub is_dir: bool,
    pub expanded: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExplorerActivation {
    OpenFile(PathBuf),
    ToggleDirectory(PathBuf),
}

/// Estado persistível do explorador.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExplorerSession {
    pub visible: bool,
    pub width: u16,
    pub show_hidden: bool,
    pub expanded: Vec<PathBuf>,
}

#[derive(Debug)]
pub struct Explorer {
    root: PathBuf,
    visible: bool,
    width: u16,
    show_hidden: bool,
    excludes: Vec<String>,
    expanded: BTreeSet<PathBuf>,
    entries: Vec<ExplorerEntry>,
    selected: usize,
    offset: usize,
    error: Option<String>,
}

impl Explorer {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let mut explorer = Self {
            root: root.into(),
            visible: true,
            width: DEFAULT_WIDTH,
            show_hidden: false,
            excludes: Vec::new(),
            expanded: BTreeSet::new(),
            entries: Vec::new(),
            selected: 0,
            offset: 0,
            error: None,
        };
        explorer.refresh();
        explorer
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn set_root(&mut self, root: impl Into<PathBuf>) {
        self.root = root.into();
        self.expanded.clear();
        self.selected = 0;
        self.offset = 0;
        self.refresh();
    }

    pub fn entries(&self) -> &[ExplorerEntry] {
        &self.entries
    }

    pub fn selected(&self) -> usize {
        self.selected
    }

    pub fn offset(&self) -> usize {
        self.offset
    }

    pub fn set_offset(&mut self, offset: usize) {
        self.offset = offset;
    }

    pub fn selected_entry(&self) -> Option<&ExplorerEntry> {
        self.entries.get(self.selected)
    }

    pub fn visible(&self) -> bool {
        self.visible
    }

    pub fn set_visible(&mut self, visible: bool) {
        self.visible = visible;
    }

    pub fn toggle_visible(&mut self) -> bool {
        self.visible = !self.visible;
        self.visible
    }

    pub fn width(&self) -> u16 {
        self.width
    }

    pub fn set_width(&mut self, width: u16) {
        self.width = width.clamp(MIN_WIDTH, MAX_WIDTH);
    }

    pub fn resize_width(&mut self, delta: i16) {
        let width = (self.width as i32 + delta as i32).clamp(MIN_WIDTH as i32, MAX_WIDTH as i32);
        self.width = width as u16;
    }

    pub fn show_hidden(&self) -> bool {
        self.show_hidden
    }

    pub fn set_show_hidden(&mut self, show_hidden: bool) {
        if self.show_hidden != show_hidden {
            self.show_hidden = show_hidden;
            self.refresh();
        }
    }

    pub fn toggle_hidden(&mut self) -> bool {
        self.set_show_hidden(!self.show_hidden);
        self.show_hidden
    }

    pub fn excludes(&self) -> &[String] {
        &self.excludes
    }

    pub fn set_excludes(&mut self, excludes: Vec<String>) {
        self.excludes = excludes;
        self.refresh();
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub fn clear_error(&mut self) {
        self.error = None;
    }

    pub fn session(&self) -> ExplorerSession {
        ExplorerSession {
            visible: self.visible,
            width: self.width,
            show_hidden: self.show_hidden,
            expanded: self.expanded.iter().cloned().collect(),
        }
    }

    pub fn restore_session(&mut self, session: &ExplorerSession) {
        self.visible = session.visible;
        self.width = session.width.clamp(MIN_WIDTH, MAX_WIDTH);
        self.show_hidden = session.show_hidden;
        self.expanded = session.expanded.iter().cloned().collect();
        self.refresh();
    }

    /// Reconstrói a lista visível a partir do disco.
    pub fn refresh(&mut self) {
        self.entries.clear();
        self.error = None;
        let mut rules = Vec::new();
        self.load_gitignore(&self.root.clone(), &mut rules);
        let root = self.root.clone();
        self.collect_entries(&root, 0, &mut rules);

        if self.entries.is_empty() {
            self.selected = 0;
            self.offset = 0;
        } else {
            self.selected = self.selected.min(self.entries.len() - 1);
            self.offset = self.offset.min(self.entries.len() - 1);
        }
    }

    fn collect_entries(&mut self, directory: &Path, depth: usize, rules: &mut Vec<IgnoreRule>) {
        if depth >= MAX_DEPTH {
            return;
        }

        let read_dir = match fs::read_dir(directory) {
            Ok(read_dir) => read_dir,
            Err(error) => {
                if depth == 0 {
                    self.error = Some(format!(
                        "não foi possível ler `{}`: {error}",
                        directory.display()
                    ));
                }
                return;
            }
        };

        let mut children: Vec<(PathBuf, String, bool)> = Vec::new();
        for entry in read_dir.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            let is_dir = entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false);
            children.push((path, name, is_dir));
        }

        children.sort_by(|left, right| {
            right
                .2
                .cmp(&left.2)
                .then_with(|| left.1.to_lowercase().cmp(&right.1.to_lowercase()))
        });

        for (path, name, is_dir) in children {
            if is_dir && is_default_excluded_dir(&name) {
                continue;
            }
            if name.starts_with('.') && !self.show_hidden {
                continue;
            }
            if is_dir && !self.show_hidden && name.starts_with('.') {
                continue;
            }
            if self.is_ignored(&path, is_dir, rules) || self.is_user_excluded(&path, &name) {
                continue;
            }

            let expanded = is_dir && self.expanded.contains(&path);
            self.entries.push(ExplorerEntry {
                path: path.clone(),
                name,
                depth,
                is_dir,
                expanded,
            });

            if expanded {
                let rule_count = rules.len();
                self.load_gitignore(&path, rules);
                self.collect_entries(&path, depth + 1, rules);
                rules.truncate(rule_count);
            }
        }
    }

    fn load_gitignore(&self, directory: &Path, rules: &mut Vec<IgnoreRule>) {
        let file = directory.join(".gitignore");
        let Ok(source) = fs::read_to_string(&file) else {
            return;
        };

        for line in source.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }

            let (negate, pattern) = match line.strip_prefix('!') {
                Some(pattern) => (true, pattern),
                None => (false, line),
            };
            let pattern = pattern.trim();
            if pattern.is_empty() {
                continue;
            }

            let dir_only = pattern.ends_with('/');
            let pattern = pattern.trim_end_matches('/').to_owned();
            if pattern.is_empty() {
                continue;
            }

            rules.push(IgnoreRule {
                base: directory.to_path_buf(),
                pattern,
                negate,
                dir_only,
            });
        }
    }

    fn is_ignored(&self, path: &Path, is_dir: bool, rules: &[IgnoreRule]) -> bool {
        let mut ignored = false;
        for rule in rules {
            if rule.matches(path, is_dir) {
                ignored = !rule.negate;
            }
        }
        ignored
    }

    fn is_user_excluded(&self, path: &Path, name: &str) -> bool {
        let relative = path.strip_prefix(&self.root).unwrap_or(path);
        let relative = relative.to_string_lossy().replace('\\', "/");

        self.excludes.iter().any(|pattern| {
            let pattern = pattern.trim();
            !pattern.is_empty() && (glob_match(pattern, name) || glob_match(pattern, &relative))
        })
    }

    pub fn move_selection(&mut self, delta: isize) {
        if self.entries.is_empty() {
            self.selected = 0;
            return;
        }

        let maximum = self.entries.len() - 1;
        let selected = self.selected as isize + delta;
        self.selected = selected.clamp(0, maximum as isize) as usize;
    }

    pub fn select(&mut self, index: usize) {
        if index < self.entries.len() {
            self.selected = index;
        }
    }

    pub fn activate_selected(&mut self) -> Option<ExplorerActivation> {
        let entry = self.entries.get(self.selected)?.clone();
        if entry.is_dir {
            self.toggle_directory(&entry.path);
            Some(ExplorerActivation::ToggleDirectory(entry.path))
        } else {
            Some(ExplorerActivation::OpenFile(entry.path))
        }
    }

    pub fn toggle_directory(&mut self, path: &Path) {
        if self.expanded.contains(path) {
            self.expanded.remove(path);
        } else {
            self.expanded.insert(path.to_path_buf());
        }
        self.refresh();
        self.reveal(path);
    }

    pub fn expand_selected(&mut self) {
        let Some(entry) = self.selected_entry().cloned() else {
            return;
        };
        if entry.is_dir && !entry.expanded {
            self.expanded.insert(entry.path.clone());
            self.refresh();
            self.reveal(&entry.path);
        }
    }

    pub fn collapse_selected(&mut self) {
        let Some(entry) = self.selected_entry().cloned() else {
            return;
        };

        if entry.is_dir && entry.expanded {
            self.expanded.remove(&entry.path);
            self.refresh();
            self.reveal(&entry.path);
            return;
        }

        if entry.depth > 0
            && let Some(parent) = entry.path.parent()
        {
            self.reveal(parent);
        }
    }

    /// Expande os ancestrais e seleciona o caminho, se ele estiver visível.
    pub fn reveal(&mut self, path: &Path) {
        let mut ancestor = path.parent();
        while let Some(candidate) = ancestor {
            if candidate == self.root {
                break;
            }
            if candidate.starts_with(&self.root) {
                self.expanded.insert(candidate.to_path_buf());
            }
            ancestor = candidate.parent();
        }

        self.refresh();

        if let Some(index) = self.entries.iter().position(|entry| entry.path == path) {
            self.selected = index;
        }
    }

    pub fn create_file(&mut self, parent: &Path, name: &str) -> Result<PathBuf, ExplorerError> {
        validate_name(name)?;
        let path = parent.join(name);
        if path.exists() {
            return Err(ExplorerError::AlreadyExists { path });
        }

        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|source| ExplorerError::Io {
                action: "criar arquivo",
                path: path.clone(),
                source,
            })?;

        self.expand_to(parent);
        self.refresh();
        self.reveal(&path);
        Ok(path)
    }

    pub fn create_dir(&mut self, parent: &Path, name: &str) -> Result<PathBuf, ExplorerError> {
        validate_name(name)?;
        let path = parent.join(name);
        if path.exists() {
            return Err(ExplorerError::AlreadyExists { path });
        }

        fs::create_dir(&path).map_err(|source| ExplorerError::Io {
            action: "criar diretório",
            path: path.clone(),
            source,
        })?;

        self.expand_to(parent);
        self.refresh();
        self.reveal(&path);
        Ok(path)
    }

    pub fn rename(&mut self, path: &Path, new_name: &str) -> Result<PathBuf, ExplorerError> {
        validate_name(new_name)?;
        if !path.exists() {
            return Err(ExplorerError::NotFound {
                path: path.to_path_buf(),
            });
        }

        let parent = path.parent().unwrap_or(&self.root);
        let target = parent.join(new_name);
        if target.exists() {
            return Err(ExplorerError::AlreadyExists { path: target });
        }

        fs::rename(path, &target).map_err(|source| ExplorerError::Io {
            action: "renomear",
            path: path.to_path_buf(),
            source,
        })?;

        let old = path.to_path_buf();
        self.remap_expanded(&old, &target);
        self.refresh();
        self.reveal(&target);
        Ok(target)
    }

    /// Exclui um arquivo ou diretório.
    ///
    /// `permanent = false` usa a lixeira do sistema. O chamador é responsável
    /// por resolver documentos modificados antes de chamar esta operação.
    pub fn delete(&mut self, path: &Path, permanent: bool) -> Result<(), ExplorerError> {
        if !path.exists() {
            return Err(ExplorerError::NotFound {
                path: path.to_path_buf(),
            });
        }

        if permanent {
            let result = if path.is_dir() {
                fs::remove_dir_all(path)
            } else {
                fs::remove_file(path)
            };

            result.map_err(|source| ExplorerError::Io {
                action: "excluir permanentemente",
                path: path.to_path_buf(),
                source,
            })?;
        } else {
            trash::delete(path).map_err(|error| ExplorerError::Trash {
                path: path.to_path_buf(),
                message: error.to_string(),
            })?;
        }

        self.expanded
            .retain(|candidate| !candidate.starts_with(path));
        self.refresh();
        Ok(())
    }

    fn expand_to(&mut self, path: &Path) {
        let mut current = Some(path);
        while let Some(candidate) = current {
            if candidate.starts_with(&self.root) {
                self.expanded.insert(candidate.to_path_buf());
            }
            if candidate == self.root {
                break;
            }
            current = candidate.parent();
        }
    }

    fn remap_expanded(&mut self, old: &Path, new: &Path) {
        let remapped: BTreeSet<PathBuf> = self
            .expanded
            .iter()
            .map(|candidate| {
                candidate
                    .strip_prefix(old)
                    .map(|suffix| new.join(suffix))
                    .unwrap_or_else(|_| candidate.clone())
            })
            .collect();
        self.expanded = remapped;
    }
}

fn validate_name(name: &str) -> Result<(), ExplorerError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(ExplorerError::EmptyName);
    }
    if name == "." || name == ".." || name.contains(['/', '\\']) {
        return Err(ExplorerError::InvalidName {
            name: name.to_owned(),
        });
    }
    if Path::new(name).components().any(|component| {
        matches!(
            component,
            Component::RootDir | Component::Prefix(_) | Component::ParentDir
        )
    }) {
        return Err(ExplorerError::InvalidName {
            name: name.to_owned(),
        });
    }
    Ok(())
}

#[derive(Debug, Clone)]
struct IgnoreRule {
    base: PathBuf,
    pattern: String,
    negate: bool,
    dir_only: bool,
}

impl IgnoreRule {
    fn matches(&self, path: &Path, is_dir: bool) -> bool {
        if self.dir_only && !is_dir {
            return false;
        }

        let Ok(relative) = path.strip_prefix(&self.base) else {
            return false;
        };
        let relative = relative.to_string_lossy().replace('\\', "/");
        if relative.is_empty() {
            return false;
        }

        if self.pattern.contains('/') {
            glob_match(self.pattern.trim_start_matches('/'), &relative)
        } else {
            relative
                .split('/')
                .any(|component| glob_match(&self.pattern, component))
        }
    }
}

/// Correspondência de glob simples com `*`, `?` e `**`.
fn glob_match(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    glob_match_at(&pattern, &text)
}

fn glob_match_at(pattern: &[char], text: &[char]) -> bool {
    if pattern.is_empty() {
        return text.is_empty();
    }

    if pattern[0] == '*' {
        if pattern.get(1) == Some(&'*') {
            let rest = pattern.get(2..).unwrap_or(&[]);
            if rest.first() == Some(&'/') {
                let rest = &rest[1..];
                for index in 0..=text.len() {
                    if glob_match_at(rest, &text[index..]) {
                        return true;
                    }
                }
                return false;
            }
            for index in 0..=text.len() {
                if glob_match_at(rest, &text[index..]) {
                    return true;
                }
            }
            return false;
        }

        for index in 0..=text.len() {
            if glob_match_at(&pattern[1..], &text[index..]) {
                return true;
            }
        }
        return false;
    }

    if text.is_empty() {
        return false;
    }

    if pattern[0] == '?' || pattern[0] == text[0] {
        return glob_match_at(&pattern[1..], &text[1..]);
    }

    false
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(label: &str) -> Self {
            let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "nexus-explorer-{}-{counter}-{label}",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
            Self { path }
        }

        fn join(&self, name: &str) -> PathBuf {
            self.path.join(name)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn names(explorer: &Explorer) -> Vec<&str> {
        explorer
            .entries()
            .iter()
            .map(|entry| entry.name.as_str())
            .collect()
    }

    #[test]
    fn lists_directories_before_files() {
        let directory = TempDir::new("order");
        fs::write(directory.join("b.txt"), "").unwrap();
        fs::create_dir(directory.join("src")).unwrap();
        let explorer = Explorer::new(&directory.path);

        assert_eq!(names(&explorer), vec!["src", "b.txt"]);
    }

    #[test]
    fn hides_default_excluded_directories() {
        let directory = TempDir::new("excluded");
        fs::create_dir(directory.join("node_modules")).unwrap();
        fs::create_dir(directory.join("src")).unwrap();
        let explorer = Explorer::new(&directory.path);

        assert_eq!(names(&explorer), vec!["src"]);
    }

    #[test]
    fn hides_hidden_files_but_can_show_them() {
        let directory = TempDir::new("hidden");
        fs::write(directory.join(".env"), "").unwrap();
        fs::write(directory.join("visivel.txt"), "").unwrap();
        let mut explorer = Explorer::new(&directory.path);

        assert_eq!(names(&explorer), vec!["visivel.txt"]);

        explorer.set_show_hidden(true);
        assert!(names(&explorer).contains(&".env"));
    }

    #[test]
    fn respects_gitignore_patterns() {
        let directory = TempDir::new("gitignore");
        fs::write(directory.join(".gitignore"), "*.log\nsegredo/\n").unwrap();
        fs::write(directory.join("app.log"), "").unwrap();
        fs::write(directory.join("app.txt"), "").unwrap();
        fs::create_dir(directory.join("segredo")).unwrap();
        let explorer = Explorer::new(&directory.path);

        assert_eq!(names(&explorer), vec!["app.txt"]);
    }

    #[test]
    fn respects_additional_excludes() {
        let directory = TempDir::new("custom-exclude");
        fs::write(directory.join("app.tmp"), "").unwrap();
        fs::write(directory.join("app.txt"), "").unwrap();
        let mut explorer = Explorer::new(&directory.path);
        explorer.set_excludes(vec!["*.tmp".to_owned()]);

        assert_eq!(names(&explorer), vec!["app.txt"]);
    }

    #[test]
    fn expands_and_collapses_directories() {
        let directory = TempDir::new("expand");
        fs::create_dir(directory.join("src")).unwrap();
        fs::write(directory.join("src").join("main.rs"), "").unwrap();
        let mut explorer = Explorer::new(&directory.path);

        explorer.toggle_directory(&directory.join("src"));
        assert_eq!(names(&explorer), vec!["src", "main.rs"]);

        explorer.toggle_directory(&directory.join("src"));
        assert_eq!(names(&explorer), vec!["src"]);
    }

    #[test]
    fn creates_and_renames_files() {
        let directory = TempDir::new("create-rename");
        let mut explorer = Explorer::new(&directory.path);

        let created = explorer.create_file(&directory.path, "novo.txt").unwrap();
        assert!(created.is_file());

        let renamed = explorer.rename(&created, "renomeado.txt").unwrap();
        assert!(!created.exists());
        assert!(renamed.is_file());
    }

    #[test]
    fn failed_rename_preserves_original() {
        let directory = TempDir::new("rename-failure");
        fs::write(directory.join("a.txt"), "a").unwrap();
        fs::write(directory.join("b.txt"), "b").unwrap();
        let mut explorer = Explorer::new(&directory.path);

        let result = explorer.rename(&directory.join("a.txt"), "b.txt");

        assert!(result.is_err());
        assert!(directory.join("a.txt").exists());
        assert_eq!(fs::read_to_string(directory.join("b.txt")).unwrap(), "b");
    }

    #[test]
    fn deletes_permanently_only_when_requested() {
        let directory = TempDir::new("delete");
        let file = directory.join("a.txt");
        fs::write(&file, "a").unwrap();
        let mut explorer = Explorer::new(&directory.path);

        explorer.delete(&file, true).unwrap();
        assert!(!file.exists());
    }

    #[test]
    fn creates_and_deletes_directories() {
        let directory = TempDir::new("dirs");
        let mut explorer = Explorer::new(&directory.path);
        let created = explorer.create_dir(&directory.path, "src").unwrap();
        assert!(created.is_dir());

        explorer.delete(&created, true).unwrap();
        assert!(!created.exists());
    }

    #[test]
    fn rejects_names_with_separators() {
        let directory = TempDir::new("invalid-name");
        let mut explorer = Explorer::new(&directory.path);

        assert!(matches!(
            explorer.create_file(&directory.path, "../fora.txt"),
            Err(ExplorerError::InvalidName { .. })
        ));
    }

    #[test]
    fn width_is_clamped() {
        let directory = TempDir::new("width");
        let mut explorer = Explorer::new(&directory.path);

        explorer.set_width(1);
        assert_eq!(explorer.width(), MIN_WIDTH);
        explorer.set_width(u16::MAX);
        assert_eq!(explorer.width(), MAX_WIDTH);
    }

    #[test]
    fn glob_matches_simple_patterns() {
        assert!(glob_match("*.rs", "main.rs"));
        assert!(glob_match("**/main.rs", "src/main.rs"));
        assert!(glob_match("src/*.rs", "src/main.rs"));
        assert!(!glob_match("src/*.rs", "tests/main.rs"));
    }

    #[test]
    fn session_round_trip_restores_settings() {
        let directory = TempDir::new("session");
        fs::create_dir(directory.join("src")).unwrap();
        let mut explorer = Explorer::new(&directory.path);
        explorer.set_visible(false);
        explorer.set_width(42);
        explorer.set_show_hidden(true);
        explorer.toggle_directory(&directory.join("src"));

        let session = explorer.session();
        let mut restored = Explorer::new(&directory.path);
        restored.restore_session(&session);

        assert!(!restored.visible());
        assert_eq!(restored.width(), 42);
        assert!(restored.show_hidden());
        assert!(
            restored
                .entries()
                .iter()
                .any(|entry| entry.name == "src" && entry.expanded)
        );
    }
}
