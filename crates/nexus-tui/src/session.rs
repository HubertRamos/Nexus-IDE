//! Persistência da sessão de trabalho.
//!
//! O arquivo é pequeno e gravado de forma atômica. Conteúdo não salvo não é
//! colocado aqui: isso pertence ao Recovery Manager, que preserva documentos
//! modificados independentemente do arquivo original.

#![allow(dead_code)]

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::explorer::ExplorerSession;
use crate::files::{self, FileError};

pub const SESSION_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionDocument {
    pub path: Option<PathBuf>,
    #[serde(default)]
    pub cursor: usize,
    #[serde(default)]
    pub anchor: Option<usize>,
    #[serde(default)]
    pub top_line: usize,
    #[serde(default)]
    pub left_col: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub workspace: Option<PathBuf>,
    #[serde(default)]
    pub documents: Vec<SessionDocument>,
    #[serde(default)]
    pub active: usize,
    #[serde(default)]
    pub explorer: ExplorerSession,
}

impl Default for Session {
    fn default() -> Self {
        Self {
            version: SESSION_VERSION,
            workspace: None,
            documents: Vec::new(),
            active: 0,
            explorer: ExplorerSession {
                visible: true,
                width: crate::explorer::DEFAULT_WIDTH,
                show_hidden: false,
                expanded: Vec::new(),
            },
        }
    }
}

fn default_version() -> u32 {
    SESSION_VERSION
}

#[derive(Debug)]
pub enum SessionError {
    Io { path: PathBuf, message: String },
    Invalid { path: PathBuf, message: String },
    UnsupportedVersion { found: u32, supported: u32 },
    File(FileError),
}

impl fmt::Display for SessionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SessionError::Io { path, message } => {
                write!(
                    formatter,
                    "falha de sessão em `{}`: {message}",
                    path.display()
                )
            }
            SessionError::Invalid { path, message } => write!(
                formatter,
                "sessão inválida em `{}`: {message}",
                path.display()
            ),
            SessionError::UnsupportedVersion { found, supported } => write!(
                formatter,
                "sessão usa a versão {found}; esta versão suporta {supported}"
            ),
            SessionError::File(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for SessionError {}

impl From<FileError> for SessionError {
    fn from(error: FileError) -> Self {
        SessionError::File(error)
    }
}

/// Armazena e carrega a sessão em um caminho fixo.
#[derive(Debug, Clone)]
pub struct SessionStore {
    path: PathBuf,
}

impl SessionStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Carrega a sessão. Arquivo ausente é um estado válido.
    pub fn load(&self) -> Result<Option<Session>, SessionError> {
        let source = match fs::read_to_string(&self.path) {
            Ok(source) => source,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(SessionError::Io {
                    path: self.path.clone(),
                    message: error.to_string(),
                });
            }
        };

        let session: Session = toml::from_str(&source).map_err(|error| SessionError::Invalid {
            path: self.path.clone(),
            message: error.to_string(),
        })?;

        if session.version > SESSION_VERSION {
            return Err(SessionError::UnsupportedVersion {
                found: session.version,
                supported: SESSION_VERSION,
            });
        }

        Ok(Some(session))
    }

    /// Grava a sessão atomicamente, preservando a anterior em caso de falha.
    pub fn save(&self, session: &Session) -> Result<(), SessionError> {
        let source = toml::to_string_pretty(session).map_err(|error| SessionError::Invalid {
            path: self.path.clone(),
            message: error.to_string(),
        })?;

        if let Some(parent) = self.path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent).map_err(|error| SessionError::Io {
                path: parent.to_path_buf(),
                message: error.to_string(),
            })?;
        }

        files::write_document(&self.path, &source)?;
        Ok(())
    }

    pub fn remove(&self) -> Result<(), SessionError> {
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(SessionError::Io {
                path: self.path.clone(),
                message: error.to_string(),
            }),
        }
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
                "nexus-session-{}-{counter}-{label}",
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

    fn sample() -> Session {
        Session {
            version: SESSION_VERSION,
            workspace: Some(PathBuf::from("/projeto")),
            documents: vec![
                SessionDocument {
                    path: Some(PathBuf::from("/projeto/main.rs")),
                    cursor: 12,
                    anchor: Some(3),
                    top_line: 2,
                    left_col: 1,
                },
                SessionDocument {
                    path: None,
                    cursor: 0,
                    anchor: None,
                    top_line: 0,
                    left_col: 0,
                },
            ],
            active: 1,
            explorer: ExplorerSession {
                visible: false,
                width: 42,
                show_hidden: true,
                expanded: vec![PathBuf::from("/projeto/src")],
            },
        }
    }

    #[test]
    fn round_trip_preserves_documents_and_explorer() {
        let directory = TempDir::new("round-trip");
        let store = SessionStore::new(directory.path.join("session.toml"));
        let session = sample();

        store.save(&session).unwrap();
        let loaded = store.load().unwrap().unwrap();

        assert_eq!(loaded, session);
    }

    #[test]
    fn missing_file_is_not_an_error() {
        let directory = TempDir::new("missing");
        let store = SessionStore::new(directory.path.join("session.toml"));

        assert!(store.load().unwrap().is_none());
    }

    #[test]
    fn corrupted_file_reports_error_without_panicking() {
        let directory = TempDir::new("corrupted");
        let path = directory.path.join("session.toml");
        fs::write(&path, "isto = [").unwrap();
        let store = SessionStore::new(&path);

        assert!(matches!(store.load(), Err(SessionError::Invalid { .. })));
        assert_eq!(fs::read_to_string(&path).unwrap(), "isto = [");
    }

    #[test]
    fn future_version_is_rejected() {
        let directory = TempDir::new("version");
        let path = directory.path.join("session.toml");
        fs::write(&path, "version = 99\n").unwrap();
        let store = SessionStore::new(&path);

        assert!(matches!(
            store.load(),
            Err(SessionError::UnsupportedVersion { .. })
        ));
    }

    #[test]
    fn failed_save_preserves_previous_file() {
        let directory = TempDir::new("save-failure");
        let store = SessionStore::new(directory.path.join("session.toml"));
        let session = sample();
        store.save(&session).unwrap();
        let previous = fs::read_to_string(store.path()).unwrap();

        // Um diretório no lugar do arquivo faz a gravação falhar.
        let broken_path = directory.path.join("broken").join("session.toml");
        fs::create_dir_all(broken_path.parent().unwrap()).unwrap();
        fs::create_dir(&broken_path).unwrap();
        let broken = SessionStore::new(&broken_path);
        assert!(broken.save(&session).is_err());
        assert_eq!(fs::read_to_string(store.path()).unwrap(), previous);
    }
}
