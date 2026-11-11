//! Recuperação automática de documentos não salvos.
//!
//! As cópias são independentes do arquivo original e do modelo de persistência
//! do Core. Cada gravação usa o mesmo mecanismo atômico de `files`, portanto
//! uma falha preserva a última cópia válida. A recuperação nunca escreve no
//! arquivo original.

#![allow(dead_code)]

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::files::{self, FileError};

pub const RECOVERY_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryRecord {
    #[serde(default = "default_version")]
    pub version: u32,
    pub id: String,
    pub path: Option<PathBuf>,
    pub name: String,
    pub text: String,
    pub saved_at: u64,
}

fn default_version() -> u32 {
    RECOVERY_VERSION
}

#[derive(Debug)]
pub enum RecoveryError {
    Io { path: PathBuf, message: String },
    File(FileError),
    Serialize(String),
    ForcedFailureForTests,
}

impl fmt::Display for RecoveryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RecoveryError::Io { path, message } => {
                write!(
                    formatter,
                    "falha de recuperação em `{}`: {message}",
                    path.display()
                )
            }
            RecoveryError::File(error) => write!(formatter, "{error}"),
            RecoveryError::Serialize(message) => {
                write!(
                    formatter,
                    "não foi possível serializar a recuperação: {message}"
                )
            }
            RecoveryError::ForcedFailureForTests => {
                write!(formatter, "falha de recuperação simulada para testes")
            }
        }
    }
}

impl std::error::Error for RecoveryError {}

impl From<FileError> for RecoveryError {
    fn from(error: FileError) -> Self {
        RecoveryError::File(error)
    }
}

/// Gerenciador de cópias de recuperação em um diretório dedicado.
#[derive(Debug)]
pub struct RecoveryManager {
    directory: PathBuf,
    #[cfg(test)]
    fail_next_write: bool,
}

impl RecoveryManager {
    pub fn new(directory: impl Into<PathBuf>) -> Self {
        Self {
            directory: directory.into(),
            #[cfg(test)]
            fail_next_write: false,
        }
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// Identificador estável para um caminho de arquivo.
    pub fn id_for_path(path: &Path) -> String {
        let canonical = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let mut hash = 0xcbf29ce484222325u64;
        for byte in canonical.to_string_lossy().as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
        format!("file-{hash:016x}")
    }

    pub fn new_untitled_id(sequence: u64) -> String {
        format!("untitled-{}-{sequence}", std::process::id())
    }

    pub fn save(&mut self, record: &RecoveryRecord) -> Result<(), RecoveryError> {
        #[cfg(test)]
        if self.fail_next_write {
            self.fail_next_write = false;
            return Err(RecoveryError::ForcedFailureForTests);
        }

        fs::create_dir_all(&self.directory).map_err(|error| RecoveryError::Io {
            path: self.directory.clone(),
            message: error.to_string(),
        })?;

        let source = toml::to_string_pretty(record)
            .map_err(|error| RecoveryError::Serialize(error.to_string()))?;
        let path = self.record_path(&record.id);
        files::write_document(&path, &source)?;
        Ok(())
    }

    pub fn remove(&self, id: &str) -> Result<(), RecoveryError> {
        let path = self.record_path(id);
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(RecoveryError::Io {
                path,
                message: error.to_string(),
            }),
        }
    }

    pub fn load_all(&self) -> Vec<RecoveryRecord> {
        let Ok(entries) = fs::read_dir(&self.directory) else {
            return Vec::new();
        };

        let mut records = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("toml") {
                continue;
            }

            let Ok(source) = fs::read_to_string(&path) else {
                continue;
            };
            let Ok(record) = toml::from_str::<RecoveryRecord>(&source) else {
                continue;
            };
            if record.version <= RECOVERY_VERSION {
                records.push(record);
            }
        }

        records.sort_by_key(|record| record.saved_at);
        records
    }

    pub fn has_copies(&self) -> bool {
        !self.load_all().is_empty()
    }

    fn record_path(&self, id: &str) -> PathBuf {
        self.directory.join(format!("{}.toml", sanitize_id(id)))
    }

    #[cfg(test)]
    pub fn force_next_failure(&mut self) {
        self.fail_next_write = true;
    }
}

fn sanitize_id(id: &str) -> String {
    let sanitized: String = id
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '-' || character == '_' {
                character
            } else {
                '_'
            }
        })
        .collect();

    if sanitized.is_empty() {
        "recovery".to_owned()
    } else {
        sanitized
    }
}

pub fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
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
                "nexus-recovery-{}-{counter}-{label}",
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

    fn record(id: &str, text: &str) -> RecoveryRecord {
        RecoveryRecord {
            version: RECOVERY_VERSION,
            id: id.to_owned(),
            path: Some(PathBuf::from("/projeto/main.rs")),
            name: "main.rs".to_owned(),
            text: text.to_owned(),
            saved_at: now_seconds(),
        }
    }

    #[test]
    fn creates_and_loads_recovery_copy() {
        let directory = TempDir::new("create");
        let mut manager = RecoveryManager::new(&directory.path);

        manager.save(&record("file-1", "não salvo")).unwrap();
        let records = manager.load_all();

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].text, "não salvo");
    }

    #[test]
    fn updates_existing_copy() {
        let directory = TempDir::new("update");
        let mut manager = RecoveryManager::new(&directory.path);

        manager.save(&record("file-1", "primeiro")).unwrap();
        manager.save(&record("file-1", "segundo")).unwrap();

        let records = manager.load_all();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].text, "segundo");
    }

    #[test]
    fn removes_copy_after_save() {
        let directory = TempDir::new("remove");
        let mut manager = RecoveryManager::new(&directory.path);

        manager.save(&record("file-1", "x")).unwrap();
        manager.remove("file-1").unwrap();

        assert!(manager.load_all().is_empty());
    }

    #[test]
    fn failure_preserves_last_valid_copy() {
        let directory = TempDir::new("failure");
        let mut manager = RecoveryManager::new(&directory.path);
        manager.save(&record("file-1", "válido")).unwrap();

        manager.force_next_failure();
        assert!(manager.save(&record("file-1", "perdido")).is_err());

        let records = manager.load_all();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].text, "válido");
    }

    #[test]
    fn recovery_never_touches_original_file() {
        let directory = TempDir::new("original");
        let original = directory.path.join("main.rs");
        fs::write(&original, "original").unwrap();
        let mut manager = RecoveryManager::new(directory.path.join("recovery"));

        manager
            .save(&RecoveryRecord {
                version: RECOVERY_VERSION,
                id: RecoveryManager::id_for_path(&original),
                path: Some(original.clone()),
                name: "main.rs".to_owned(),
                text: "conteúdo recuperado".to_owned(),
                saved_at: now_seconds(),
            })
            .unwrap();

        assert_eq!(fs::read_to_string(&original).unwrap(), "original");
    }

    #[test]
    fn ids_are_stable_for_same_path() {
        let directory = TempDir::new("ids");
        let file = directory.path.join("a.rs");
        fs::write(&file, "").unwrap();

        assert_eq!(
            RecoveryManager::id_for_path(&file),
            RecoveryManager::id_for_path(&file)
        );
    }
}
