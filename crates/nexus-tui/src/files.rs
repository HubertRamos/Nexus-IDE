use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicU64, Ordering};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);
const TEMP_ATTEMPTS: usize = 8;

/// Falhas de acesso ao sistema de arquivos.
///
/// Esta camada é intencionalmente pequena e isolada. Uma futura GUI pode
/// substituí-la por outra implementação sem duplicar a lógica do editor, pois
/// a aplicação só depende de `read_document` e `write_document`.
#[derive(Debug)]
pub enum FileError {
    EmptyPath,
    InvalidPath {
        path: PathBuf,
    },
    Io {
        action: &'static str,
        path: PathBuf,
        source: io::Error,
    },
    InvalidUtf8 {
        path: PathBuf,
    },
}

impl fmt::Display for FileError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FileError::EmptyPath => write!(formatter, "o caminho do arquivo está vazio"),
            FileError::InvalidPath { path } => {
                write!(
                    formatter,
                    "caminho de arquivo inválido: `{}`",
                    path.display()
                )
            }
            FileError::Io {
                action,
                path,
                source,
            } => write!(
                formatter,
                "falha ao {action} `{}`: {source}",
                path.display()
            ),
            FileError::InvalidUtf8 { path } => write!(
                formatter,
                "o arquivo `{}` não contém UTF-8 válido",
                path.display()
            ),
        }
    }
}

impl std::error::Error for FileError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            FileError::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// Lê um arquivo inteiro como UTF-8 válido.
///
/// Não há conversão com perda: qualquer sequência inválida é reportada como
/// [`FileError::InvalidUtf8`].
pub fn read_document(path: &Path) -> Result<String, FileError> {
    ensure_path(path)?;

    let bytes = fs::read(path).map_err(|source| FileError::Io {
        action: "ler",
        path: path.to_path_buf(),
        source,
    })?;

    String::from_utf8(bytes).map_err(|_| FileError::InvalidUtf8 {
        path: path.to_path_buf(),
    })
}

/// Grava o conteúdo em um arquivo de forma atômica sempre que possível.
///
/// O texto é escrito em um arquivo temporário no mesmo diretório e só então
/// renomeado para o destino. Assim, uma falha de escrita não trunca o arquivo
/// original. O conteúdo é gravado exatamente como recebido, preservando as
/// quebras de linha existentes.
pub fn write_document(path: &Path, contents: &str) -> Result<(), FileError> {
    ensure_path(path)?;

    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let file_name = path.file_name().ok_or_else(|| FileError::InvalidPath {
        path: path.to_path_buf(),
    })?;

    let (temporary_path, mut file) = create_temporary_file(parent, file_name, path)?;

    let write_result = (|| -> io::Result<()> {
        file.write_all(contents.as_bytes())?;
        file.flush()?;
        file.sync_all()
    })();

    drop(file);

    if let Err(source) = write_result {
        let source = with_cleanup_error(source, fs::remove_file(&temporary_path));
        return Err(FileError::Io {
            action: "escrever",
            path: path.to_path_buf(),
            source,
        });
    }

    if let Err(source) = fs::rename(&temporary_path, path) {
        let source = with_cleanup_error(source, fs::remove_file(&temporary_path));
        return Err(FileError::Io {
            action: "substituir",
            path: path.to_path_buf(),
            source,
        });
    }

    Ok(())
}

fn with_cleanup_error(source: io::Error, cleanup: io::Result<()>) -> io::Error {
    match cleanup {
        Ok(()) => source,
        Err(cleanup_error) => io::Error::new(
            source.kind(),
            format!("{source}; falha ao remover arquivo temporário: {cleanup_error}"),
        ),
    }
}

fn ensure_path(path: &Path) -> Result<(), FileError> {
    if path.as_os_str().is_empty() {
        Err(FileError::EmptyPath)
    } else {
        Ok(())
    }
}

fn create_temporary_file(
    parent: &Path,
    file_name: &std::ffi::OsStr,
    destination: &Path,
) -> Result<(PathBuf, fs::File), FileError> {
    for _ in 0..TEMP_ATTEMPTS {
        let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let name = format!(
            ".{}.nexus-{}-{}.tmp",
            file_name.to_string_lossy(),
            process::id(),
            counter
        );
        let candidate = parent.join(name);

        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => return Ok((candidate, file)),
            Err(source) if source.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(source) => {
                return Err(FileError::Io {
                    action: "criar arquivo temporário para",
                    path: destination.to_path_buf(),
                    source,
                });
            }
        }
    }

    Err(FileError::Io {
        action: "criar arquivo temporário para",
        path: destination.to_path_buf(),
        source: io::Error::new(
            io::ErrorKind::AlreadyExists,
            "não foi possível obter um nome temporário exclusivo",
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(label: &str) -> Self {
            let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "nexus-tui-files-{}-{}-{label}",
                process::id(),
                counter
            ));

            fs::create_dir_all(&path).expect("criar diretório temporário de teste");

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
    fn reads_valid_utf8_file() {
        let directory = TempDir::new("read-valid");
        let path = directory.join("document.txt");
        fs::write(&path, "café\n界\r\n").unwrap();

        assert_eq!(read_document(&path).unwrap(), "café\n界\r\n");
    }

    #[test]
    fn reads_empty_file() {
        let directory = TempDir::new("read-empty");
        let path = directory.join("empty.txt");
        fs::write(&path, "").unwrap();

        assert_eq!(read_document(&path).unwrap(), "");
    }

    #[test]
    fn reports_missing_file() {
        let directory = TempDir::new("missing");
        let path = directory.join("missing.txt");

        assert!(matches!(
            read_document(&path),
            Err(FileError::Io {
                action: "ler",
                source,
                ..
            }) if source.kind() == io::ErrorKind::NotFound
        ));
    }

    #[test]
    fn reports_read_failure_for_directory() {
        let directory = TempDir::new("read-directory");

        assert!(matches!(
            read_document(&directory.path),
            Err(FileError::Io { action: "ler", .. })
        ));
    }

    #[test]
    fn rejects_invalid_utf8_without_lossy_conversion() {
        let directory = TempDir::new("invalid-utf8");
        let path = directory.join("invalid.bin");
        fs::write(&path, [0xFF, 0xFE, b'a']).unwrap();

        assert!(matches!(
            read_document(&path),
            Err(FileError::InvalidUtf8 { .. })
        ));
    }

    #[test]
    fn rejects_empty_path() {
        assert!(matches!(
            read_document(Path::new("")),
            Err(FileError::EmptyPath)
        ));
        assert!(matches!(
            write_document(Path::new(""), "text"),
            Err(FileError::EmptyPath)
        ));
    }

    #[test]
    fn writes_and_reads_back_exact_content() {
        let directory = TempDir::new("write");
        let path = directory.join("document.txt");

        write_document(&path, "linha 1\r\nlinha 2\ncafé ☕\n").unwrap();

        assert_eq!(
            read_document(&path).unwrap(),
            "linha 1\r\nlinha 2\ncafé ☕\n"
        );
    }

    #[test]
    fn overwrites_existing_file_without_partial_content() {
        let directory = TempDir::new("overwrite");
        let path = directory.join("document.txt");
        fs::write(&path, "conteúdo antigo").unwrap();

        write_document(&path, "conteúdo novo").unwrap();

        assert_eq!(read_document(&path).unwrap(), "conteúdo novo");
    }

    #[test]
    fn reports_write_failure_without_creating_destination() {
        let directory = TempDir::new("write-missing-parent");
        let path = directory.join("missing").join("document.txt");

        assert!(matches!(
            write_document(&path, "texto"),
            Err(FileError::Io {
                action: "criar arquivo temporário para",
                ..
            })
        ));
        assert!(!path.exists());
    }

    #[test]
    fn reports_write_failure_when_destination_is_directory() {
        let directory = TempDir::new("write-directory");
        let path = directory.join("destination");
        fs::create_dir(&path).unwrap();

        assert!(matches!(
            write_document(&path, "texto"),
            Err(FileError::Io {
                action: "substituir",
                ..
            })
        ));
        assert!(path.is_dir());

        let temporary_files = fs::read_dir(&directory.path)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().contains(".nexus-"))
            .count();
        assert_eq!(temporary_files, 0);
    }
}
