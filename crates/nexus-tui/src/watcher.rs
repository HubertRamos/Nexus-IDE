//! Detecção de alterações externas.
//!
//! A comparação usa um fingerprint do conteúdo (tamanho + hash FNV-1a), não
//! apenas timestamps. Isso evita sobrescrever silenciosamente uma versão
//! externa e permite reconhecer renomeações quando o conteúdo é preservado.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::document::Document;

/// Identidade barata e estável do conteúdo de um arquivo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileFingerprint {
    pub len: u64,
    pub hash: u64,
}

impl FileFingerprint {
    pub fn from_bytes(bytes: &[u8]) -> Self {
        let mut hash = 0xcbf29ce484222325u64;
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }

        Self {
            len: bytes.len() as u64,
            hash,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DetectedChange {
    Unchanged,
    Modified(FileFingerprint),
    Removed,
    Renamed(PathBuf),
}

/// Calcula o fingerprint atual de um caminho.
///
/// `Ok(None)` significa que o arquivo não existe. Outros erros de leitura são
/// propagados para que o chamador possa informar o usuário sem perder dados.
pub fn fingerprint(path: &Path) -> io::Result<Option<FileFingerprint>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(FileFingerprint::from_bytes(&bytes))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// Compara o estado atual do disco com a referência conhecida do documento.
///
/// Quando o arquivo desapareceu, procura na raiz um único arquivo com o mesmo
/// fingerprint. Se houver zero ou mais de um candidato, a mudança é reportada
/// como remoção; o documento nunca é alterado silenciosamente.
pub fn detect_change(document: &Document, workspace_root: Option<&Path>) -> DetectedChange {
    let Some(path) = document.path.as_deref() else {
        return DetectedChange::Unchanged;
    };

    let Some(known) = document.disk else {
        return DetectedChange::Unchanged;
    };

    match fingerprint(path) {
        Ok(Some(current)) if current == known => DetectedChange::Unchanged,
        Ok(Some(current)) => DetectedChange::Modified(current),
        Ok(None) => {
            if let Some(root) = workspace_root
                && let Some(renamed) = find_unique_by_fingerprint(root, known)
            {
                return DetectedChange::Renamed(renamed);
            }
            DetectedChange::Removed
        }
        Err(_) => DetectedChange::Removed,
    }
}

/// Procura um arquivo com o fingerprint informado, ignorando diretórios que o
/// explorador também exclui. Retorna apenas quando existe exatamente um
/// candidato, evitando renomeações ambíguas.
pub fn find_unique_by_fingerprint(root: &Path, target: FileFingerprint) -> Option<PathBuf> {
    let mut matches = Vec::new();
    let mut visited = 0usize;
    search(root, target, &mut matches, &mut visited, 0);
    if matches.len() == 1 {
        matches.pop()
    } else {
        None
    }
}

fn search(
    directory: &Path,
    target: FileFingerprint,
    matches: &mut Vec<PathBuf>,
    visited: &mut usize,
    depth: usize,
) {
    const MAX_VISITED: usize = 20_000;
    const MAX_DEPTH: usize = 32;

    if depth >= MAX_DEPTH || *visited >= MAX_VISITED || matches.len() > 1 {
        return;
    }

    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };

    for entry in entries.flatten() {
        *visited += 1;
        if *visited >= MAX_VISITED {
            return;
        }

        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };

        if file_type.is_dir() {
            if crate::explorer::is_default_excluded_dir(&name) || name.starts_with('.') {
                continue;
            }
            search(&path, target, matches, visited, depth + 1);
        } else if file_type.is_file() {
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            if metadata.len() != target.len {
                continue;
            }
            if let Ok(Some(candidate)) = fingerprint(&path)
                && candidate == target
            {
                matches.push(path);
                if matches.len() > 1 {
                    return;
                }
            }
        }
    }
}

/// Armazena o estado de monitoramento de um arquivo.
#[derive(Debug, Clone)]
pub struct FileWatcher {
    /// Caminho monitorado.
    pub path: PathBuf,
    /// Fingerprint do conteúdo no último carregamento.
    pub fingerprint: Option<FileFingerprint>,
}

#[cfg(test)]
mod tests {
    use std::fs;
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
                "nexus-watcher-{}-{counter}-{label}",
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

    #[test]
    fn fingerprint_depends_on_content_not_timestamp() {
        let directory = TempDir::new("fingerprint");
        let path = directory.join("a.txt");
        fs::write(&path, "abc").unwrap();
        let first = fingerprint(&path).unwrap().unwrap();
        fs::write(&path, "abc").unwrap();
        let second = fingerprint(&path).unwrap().unwrap();

        assert_eq!(first, second);

        fs::write(&path, "abd").unwrap();
        assert_ne!(first, fingerprint(&path).unwrap().unwrap());
    }

    #[test]
    fn detects_external_modification() {
        let directory = TempDir::new("modified");
        let path = directory.join("a.txt");
        fs::write(&path, "abc").unwrap();
        let mut document = Document::from_file(&path).unwrap();
        fs::write(&path, "externo").unwrap();

        assert!(matches!(
            detect_change(&document, Some(&directory.path)),
            DetectedChange::Modified(_)
        ));

        document.disk = fingerprint(&path).unwrap();
        assert_eq!(
            detect_change(&document, Some(&directory.path)),
            DetectedChange::Unchanged
        );
    }

    #[test]
    fn detects_removal() {
        let directory = TempDir::new("removed");
        let path = directory.join("a.txt");
        fs::write(&path, "abc").unwrap();
        let document = Document::from_file(&path).unwrap();
        fs::remove_file(&path).unwrap();

        assert_eq!(
            detect_change(&document, Some(&directory.path)),
            DetectedChange::Removed
        );
    }

    #[test]
    fn detects_unique_rename() {
        let directory = TempDir::new("renamed");
        let path = directory.join("a.txt");
        fs::write(&path, "conteudo unico").unwrap();
        let document = Document::from_file(&path).unwrap();
        let renamed = directory.join("b.txt");
        fs::rename(&path, &renamed).unwrap();

        assert_eq!(
            detect_change(&document, Some(&directory.path)),
            DetectedChange::Renamed(renamed)
        );
    }

    #[test]
    fn ambiguous_rename_is_reported_as_removal() {
        let directory = TempDir::new("ambiguous");
        let path = directory.join("a.txt");
        fs::write(&path, "igual").unwrap();
        let document = Document::from_file(&path).unwrap();
        fs::rename(&path, directory.join("b.txt")).unwrap();
        fs::write(directory.join("c.txt"), "igual").unwrap();

        assert_eq!(
            detect_change(&document, Some(&directory.path)),
            DetectedChange::Removed
        );
    }
}
