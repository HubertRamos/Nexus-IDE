//! Raiz de trabalho do editor.
//!
//! Um Workspace é apenas um diretório existente escolhido pelo usuário. Não há
//! arquivo de projeto obrigatório. Este módulo mantém a identidade da raiz e as
//! regras comuns de contenção de caminhos; explorador, documentos e sessão
//! dependem dele, mas não o contrário.

#![allow(dead_code)]

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub enum WorkspaceError {
    EmptyPath,
    NotFound { path: PathBuf },
    NotDirectory { path: PathBuf },
    Io { path: PathBuf, source: io::Error },
}

impl fmt::Display for WorkspaceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WorkspaceError::EmptyPath => write!(formatter, "o caminho do workspace está vazio"),
            WorkspaceError::NotFound { path } => write!(
                formatter,
                "o diretório do workspace não existe: `{}`",
                path.display()
            ),
            WorkspaceError::NotDirectory { path } => write!(
                formatter,
                "o caminho do workspace não é um diretório: `{}`",
                path.display()
            ),
            WorkspaceError::Io { path, source } => write!(
                formatter,
                "falha ao acessar o workspace `{}`: {source}",
                path.display()
            ),
        }
    }
}

impl std::error::Error for WorkspaceError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            WorkspaceError::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// Diretório raiz de um Workspace, sempre absoluto e canônico.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workspace {
    root: PathBuf,
}

impl Workspace {
    /// Abre um diretório existente como raiz.
    pub fn open(path: &Path) -> Result<Self, WorkspaceError> {
        if path.as_os_str().is_empty() {
            return Err(WorkspaceError::EmptyPath);
        }

        let absolute = absolute_path(path).map_err(|source| WorkspaceError::Io {
            path: path.to_path_buf(),
            source,
        })?;

        let metadata = fs::metadata(&absolute).map_err(|source| {
            if source.kind() == io::ErrorKind::NotFound {
                WorkspaceError::NotFound {
                    path: path.to_path_buf(),
                }
            } else {
                WorkspaceError::Io {
                    path: path.to_path_buf(),
                    source,
                }
            }
        })?;

        if !metadata.is_dir() {
            return Err(WorkspaceError::NotDirectory {
                path: path.to_path_buf(),
            });
        }

        let root = fs::canonicalize(&absolute).map_err(|source| WorkspaceError::Io {
            path: path.to_path_buf(),
            source,
        })?;

        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Nome exibido em listas de projetos recentes.
    pub fn name(&self) -> String {
        workspace_name(&self.root)
    }

    /// Verifica se um caminho existente pertence ao Workspace.
    ///
    /// O caminho é canonicalizado, portanto links simbólicos que escapam da
    /// raiz não são aceitos silenciosamente.
    pub fn contains_existing(&self, path: &Path) -> bool {
        fs::canonicalize(path)
            .map(|canonical| canonical.starts_with(&self.root))
            .unwrap_or(false)
    }

    /// Resolve um caminho relativo à raiz do Workspace.
    pub fn resolve(&self, path: &Path) -> PathBuf {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.root.join(path)
        }
    }
}

/// Nome de exibição de um diretório, com fallback para a raiz do sistema.
pub fn workspace_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

/// Converte um caminho em absoluto sem exigir que ele exista.
///
/// Componentes `.` e `..` são resolvidos lexicalmente. O caminho resultante
/// não é canônico; use [`Workspace::open`] ou `fs::canonicalize` quando a
/// existência importar.
pub fn absolute_path(path: &Path) -> io::Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };

    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        use std::path::Component;
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }

    Ok(normalized)
}

/// Diretório que contém um caminho de arquivo.
pub fn parent_directory(path: &Path) -> PathBuf {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf()
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
                "nexus-workspace-{}-{counter}-{label}",
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
    fn opens_directory_and_uses_canonical_root() {
        let directory = TempDir::new("open");
        let workspace = Workspace::open(&directory.path).unwrap();

        assert_eq!(workspace.root(), fs::canonicalize(&directory.path).unwrap());
    }

    #[test]
    fn rejects_missing_path() {
        let directory = TempDir::new("missing");
        let missing = directory.path.join("nao-existe");

        assert!(matches!(
            Workspace::open(&missing),
            Err(WorkspaceError::NotFound { .. })
        ));
    }

    #[test]
    fn rejects_file_as_workspace() {
        let directory = TempDir::new("file");
        let file = directory.path.join("arquivo.txt");
        fs::write(&file, "x").unwrap();

        assert!(matches!(
            Workspace::open(&file),
            Err(WorkspaceError::NotDirectory { .. })
        ));
    }

    #[test]
    fn contains_existing_file_inside_root() {
        let directory = TempDir::new("contains");
        let file = directory.path.join("src").join("main.rs");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, "fn main() {}").unwrap();

        let workspace = Workspace::open(&directory.path).unwrap();

        assert!(workspace.contains_existing(&file));
        assert!(!workspace.contains_existing(&directory.path.join("..")));
    }

    #[test]
    fn absolute_path_normalizes_relative_components() {
        let path = absolute_path(Path::new("a/./b/../c")).unwrap();

        assert!(path.is_absolute());
        assert!(path.ends_with("a/c"));
    }
}
