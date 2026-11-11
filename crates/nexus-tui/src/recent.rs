//! Lista persistente de projetos recentes.
//!
//! A lista é ordenada pelo último acesso, limitada a dez entradas e nunca
//! remove arquivos do disco ao remover uma entrada.

#![allow(dead_code)]

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::files::{self, FileError};

pub const MAX_RECENT: usize = 10;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecentEntry {
    pub name: String,
    pub path: PathBuf,
    #[serde(default)]
    pub last_access: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecentProjects {
    #[serde(default)]
    pub entries: Vec<RecentEntry>,
}

impl RecentProjects {
    pub fn record(&mut self, name: impl Into<String>, path: impl Into<PathBuf>) {
        let path = path.into();
        self.entries.retain(|entry| entry.path != path);
        self.entries.insert(
            0,
            RecentEntry {
                name: name.into(),
                path,
                last_access: now_seconds(),
            },
        );
        self.sort_and_truncate();
    }

    pub fn record_now(&mut self, name: impl Into<String>, path: impl Into<PathBuf>, at: u64) {
        let path = path.into();
        self.entries.retain(|entry| entry.path != path);
        self.entries.push(RecentEntry {
            name: name.into(),
            path,
            last_access: at,
        });
        self.sort_and_truncate();
    }

    pub fn remove(&mut self, path: &Path) -> bool {
        let before = self.entries.len();
        self.entries.retain(|entry| entry.path != path);
        before != self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    fn sort_and_truncate(&mut self) {
        self.entries
            .sort_by_key(|entry| std::cmp::Reverse(entry.last_access));
        self.entries.truncate(MAX_RECENT);
    }
}

pub fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

#[derive(Debug)]
pub enum RecentError {
    Io { path: PathBuf, message: String },
    Invalid { path: PathBuf, message: String },
    File(FileError),
}

impl fmt::Display for RecentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RecentError::Io { path, message } => {
                write!(
                    formatter,
                    "falha em recentes `{}`: {message}",
                    path.display()
                )
            }
            RecentError::Invalid { path, message } => {
                write!(
                    formatter,
                    "lista de recentes inválida em `{}`: {message}",
                    path.display()
                )
            }
            RecentError::File(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for RecentError {}

impl From<FileError> for RecentError {
    fn from(error: FileError) -> Self {
        RecentError::File(error)
    }
}

#[derive(Debug, Clone)]
pub struct RecentStore {
    path: PathBuf,
}

impl RecentStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn load(&self) -> Result<RecentProjects, RecentError> {
        let source = match fs::read_to_string(&self.path) {
            Ok(source) => source,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(RecentProjects::default());
            }
            Err(error) => {
                return Err(RecentError::Io {
                    path: self.path.clone(),
                    message: error.to_string(),
                });
            }
        };

        toml::from_str(&source).map_err(|error| RecentError::Invalid {
            path: self.path.clone(),
            message: error.to_string(),
        })
    }

    pub fn save(&self, projects: &RecentProjects) -> Result<(), RecentError> {
        let source = toml::to_string_pretty(projects).map_err(|error| RecentError::Invalid {
            path: self.path.clone(),
            message: error.to_string(),
        })?;

        if let Some(parent) = self.path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent).map_err(|error| RecentError::Io {
                path: parent.to_path_buf(),
                message: error.to_string(),
            })?;
        }

        files::write_document(&self.path, &source)?;
        Ok(())
    }
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
                "nexus-recent-{}-{counter}-{label}",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
            Self { path }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn records_most_recent_first_and_deduplicates() {
        let mut projects = RecentProjects::default();
        projects.record_now("a", "/a", 1);
        projects.record_now("b", "/b", 2);
        projects.record_now("a", "/a", 3);

        assert_eq!(projects.len(), 2);
        assert_eq!(projects.entries[0].path, PathBuf::from("/a"));
        assert_eq!(projects.entries[1].path, PathBuf::from("/b"));
    }

    #[test]
    fn limits_to_ten_entries() {
        let mut projects = RecentProjects::default();
        for index in 0..15 {
            projects.record_now(format!("p{index}"), format!("/p{index}"), index);
        }

        assert_eq!(projects.len(), MAX_RECENT);
        assert_eq!(projects.entries[0].path, PathBuf::from("/p14"));
    }

    #[test]
    fn removes_entry_without_touching_disk() {
        let directory = TempDir::new("remove");
        let project = directory.path.join("projeto");
        fs::create_dir(&project).unwrap();
        let mut projects = RecentProjects::default();
        projects.record("projeto", &project);

        assert!(projects.remove(&project));
        assert!(project.is_dir());
        assert!(projects.is_empty());
    }

    #[test]
    fn round_trip_persists_entries() {
        let directory = TempDir::new("round-trip");
        let store = RecentStore::new(directory.path.join("recent.toml"));
        let mut projects = RecentProjects::default();
        projects.record_now("a", "/a", 10);

        store.save(&projects).unwrap();
        let loaded = store.load().unwrap();

        assert_eq!(loaded, projects);
    }

    #[test]
    fn corrupted_file_is_reported() {
        let directory = TempDir::new("corrupted");
        let path = directory.path.join("recent.toml");
        fs::write(&path, "[[entries]\n").unwrap();
        let store = RecentStore::new(&path);

        assert!(matches!(store.load(), Err(RecentError::Invalid { .. })));
    }
}
