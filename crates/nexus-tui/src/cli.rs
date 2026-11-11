//! Interpretação dos argumentos de linha de comando.
//!
//! A CLI é deliberadamente pequena: um Workspace explícito ou inferido e uma
//! lista de arquivos para abrir. A tela inicial é representada por
//! [`CliAction::Start`] e decidida pelo chamador conforme a sessão persistida.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use crate::workspace::{WorkspaceError, absolute_path, parent_directory};

const PROJECT_MARKERS: [&str; 6] = [
    ".git",
    "Cargo.toml",
    "package.json",
    "pyproject.toml",
    "go.mod",
    "Makefile",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliAction {
    Start,
    Open {
        workspace: PathBuf,
        files: Vec<PathBuf>,
    },
    Help,
    Version,
}

#[derive(Debug)]
pub enum CliError {
    EmptyArgument,
    NotFound { argument: String },
    NotAccessible { argument: String, message: String },
    MultipleWorkspaces,
    OutsideWorkspace { file: PathBuf, workspace: PathBuf },
    IncompatibleWorkspaces { files: Vec<PathBuf> },
}

impl fmt::Display for CliError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CliError::EmptyArgument => write!(formatter, "argumento de caminho vazio"),
            CliError::NotFound { argument } => {
                write!(formatter, "caminho não encontrado: `{argument}`")
            }
            CliError::NotAccessible { argument, message } => {
                write!(
                    formatter,
                    "não foi possível acessar `{argument}`: {message}"
                )
            }
            CliError::MultipleWorkspaces => write!(
                formatter,
                "apenas um diretório de Workspace pode ser aberto por vez"
            ),
            CliError::OutsideWorkspace { file, workspace } => write!(
                formatter,
                "o arquivo `{}` não pertence ao Workspace `{}`",
                file.display(),
                workspace.display()
            ),
            CliError::IncompatibleWorkspaces { files } => write!(
                formatter,
                "os arquivos informados pertencem a projetos diferentes e não podem ser abertos no mesmo Workspace: {}",
                files
                    .iter()
                    .map(|path| format!("`{}`", path.display()))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }
}

impl std::error::Error for CliError {}

impl From<WorkspaceError> for CliError {
    fn from(error: WorkspaceError) -> Self {
        CliError::NotAccessible {
            argument: String::new(),
            message: error.to_string(),
        }
    }
}

/// Interpreta os argumentos (sem o nome do executável).
pub fn parse<I, S>(arguments: I, cwd: &Path) -> Result<CliAction, CliError>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let mut paths = Vec::new();
    let mut options_ended = false;

    for argument in arguments {
        let argument = argument.into();

        if !options_ended {
            match argument.as_str() {
                "--help" | "-h" => return Ok(CliAction::Help),
                "--version" | "-V" => return Ok(CliAction::Version),
                "--" => {
                    options_ended = true;
                    continue;
                }
                _ if argument.starts_with('-') && argument != "-" => {
                    return Err(CliError::NotAccessible {
                        argument,
                        message: "opção desconhecida".to_owned(),
                    });
                }
                _ => {}
            }
        }

        if argument.is_empty() {
            return Err(CliError::EmptyArgument);
        }
        paths.push(argument);
    }

    if paths.is_empty() {
        return Ok(CliAction::Start);
    }

    let mut directories = Vec::new();
    let mut files = Vec::new();

    for argument in paths {
        let resolved =
            absolute_path(&cwd.join(&argument)).map_err(|error| CliError::NotAccessible {
                argument: argument.clone(),
                message: error.to_string(),
            })?;

        let metadata = fs::metadata(&resolved).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                CliError::NotFound {
                    argument: argument.clone(),
                }
            } else {
                CliError::NotAccessible {
                    argument: argument.clone(),
                    message: error.to_string(),
                }
            }
        })?;

        let canonical = fs::canonicalize(&resolved).map_err(|error| CliError::NotAccessible {
            argument: argument.clone(),
            message: error.to_string(),
        })?;

        if metadata.is_dir() {
            directories.push(canonical);
        } else if metadata.is_file() {
            files.push(canonical);
        } else {
            return Err(CliError::NotAccessible {
                argument,
                message: "o caminho não é um arquivo nem um diretório".to_owned(),
            });
        }
    }

    if directories.len() > 1 {
        return Err(CliError::MultipleWorkspaces);
    }

    let workspace = if let Some(directory) = directories.pop() {
        for file in &files {
            if !file.starts_with(&directory) {
                return Err(CliError::OutsideWorkspace {
                    file: file.clone(),
                    workspace: directory,
                });
            }
        }
        directory
    } else {
        infer_workspace(&files)?
    };

    let files = deduplicate(files);

    Ok(CliAction::Open { workspace, files })
}

/// Infere a raiz a partir dos arquivos, recusando projetos incompatíveis.
fn infer_workspace(files: &[PathBuf]) -> Result<PathBuf, CliError> {
    if files.is_empty() {
        return Ok(std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    }

    let parents: Vec<PathBuf> = files.iter().map(|file| parent_directory(file)).collect();

    let nearest_roots: Vec<Option<PathBuf>> = parents
        .iter()
        .map(|parent| nearest_project_root(parent))
        .collect();

    if let Some(first) = nearest_roots.first().and_then(|root| root.clone()) {
        let all_same = nearest_roots
            .iter()
            .all(|root| root.as_deref() == Some(first.as_path()));
        if !all_same {
            return Err(CliError::IncompatibleWorkspaces {
                files: files.to_vec(),
            });
        }
        return Ok(first);
    }

    let mut common = parents[0].clone();
    for parent in &parents[1..] {
        common = common_ancestor(&common, parent);
    }

    if let Some(root) = nearest_project_root(&common) {
        return Ok(root);
    }

    Ok(common)
}

fn nearest_project_root(start: &Path) -> Option<PathBuf> {
    let mut current = Some(start);
    while let Some(directory) = current {
        if PROJECT_MARKERS
            .iter()
            .any(|marker| directory.join(marker).exists())
        {
            return Some(directory.to_path_buf());
        }
        current = directory.parent();
    }
    None
}

fn common_ancestor(left: &Path, right: &Path) -> PathBuf {
    let left_components: Vec<_> = left.components().collect();
    let right_components: Vec<_> = right.components().collect();

    let mut common = PathBuf::new();
    for (left, right) in left_components.iter().zip(right_components.iter()) {
        if left == right {
            common.push(left.as_os_str());
        } else {
            break;
        }
    }

    if common.as_os_str().is_empty() {
        PathBuf::from(std::path::MAIN_SEPARATOR.to_string())
    } else {
        common
    }
}

fn deduplicate(files: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut unique = Vec::new();
    for file in files {
        if !unique.contains(&file) {
            unique.push(file);
        }
    }
    unique
}

pub fn help_text() -> String {
    format!(
        "Nexus {version}\n\
         Editor de código terminal-first.\n\
         \n\
         USO:\n\
         \x20 nexus                     abre a tela inicial\n\
         \x20 nexus .                   abre o diretório atual como Workspace\n\
         \x20 nexus ./projeto           abre um diretório como Workspace\n\
         \x20 nexus arquivo.rs          abre um arquivo e infere o Workspace\n\
         \x20 nexus projeto/main.rs     abre o projeto e o arquivo\n\
         \x20 nexus a.rs b.rs           abre vários arquivos no mesmo Workspace\n\
         \n\
         OPÇÕES:\n\
         \x20 -h, --help                mostra esta ajuda\n\
         \x20 -V, --version             mostra a versão\n\
         \n\
         ATALHOS:\n\
         \x20 Ctrl+O abrir arquivo      Ctrl+S salvar      Ctrl+Shift+S salvar como\n\
         \x20 Ctrl+B explorador          Ctrl+W fechar aba  Ctrl+PageUp/PageDown abas\n\
         \x20 Ctrl+H / F1 ajuda          Ctrl+Q sair\n",
        version = env!("CARGO_PKG_VERSION")
    )
}

pub fn version_text() -> String {
    format!("Nexus {}", env!("CARGO_PKG_VERSION"))
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
                "nexus-cli-{}-{counter}-{label}",
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
    fn no_arguments_starts_screen() {
        let directory = TempDir::new("none");
        assert_eq!(
            parse(Vec::<String>::new(), &directory.path).unwrap(),
            CliAction::Start
        );
    }

    #[test]
    fn help_and_version_are_recognized() {
        let directory = TempDir::new("help");
        assert_eq!(parse(["--help"], &directory.path).unwrap(), CliAction::Help);
        assert_eq!(parse(["-h"], &directory.path).unwrap(), CliAction::Help);
        assert_eq!(
            parse(["--version"], &directory.path).unwrap(),
            CliAction::Version
        );
        assert_eq!(parse(["-V"], &directory.path).unwrap(), CliAction::Version);
    }

    #[test]
    fn opens_directory_as_workspace() {
        let directory = TempDir::new("directory");
        let project = directory.join("projeto");
        fs::create_dir(&project).unwrap();

        let action = parse(["projeto"], &directory.path).unwrap();
        let CliAction::Open { workspace, files } = action else {
            panic!("esperava abrir workspace");
        };

        assert_eq!(workspace, fs::canonicalize(&project).unwrap());
        assert!(files.is_empty());
    }

    #[test]
    fn opens_file_and_infers_parent_workspace() {
        let directory = TempDir::new("file");
        let file = directory.join("main.rs");
        fs::write(&file, "fn main() {}").unwrap();

        let action = parse(["main.rs"], &directory.path).unwrap();
        let CliAction::Open { workspace, files } = action else {
            panic!("esperava abrir arquivo");
        };

        assert_eq!(workspace, fs::canonicalize(&directory.path).unwrap());
        assert_eq!(files, vec![fs::canonicalize(&file).unwrap()]);
    }

    #[test]
    fn opens_multiple_files_in_same_workspace() {
        let directory = TempDir::new("multiple");
        let first = directory.join("a.rs");
        let second = directory.join("b.rs");
        fs::write(&first, "").unwrap();
        fs::write(&second, "").unwrap();

        let action = parse(["a.rs", "b.rs"], &directory.path).unwrap();
        let CliAction::Open { files, .. } = action else {
            panic!("esperava abrir arquivos");
        };

        assert_eq!(files.len(), 2);
    }

    #[test]
    fn accepts_absolute_paths() {
        let directory = TempDir::new("absolute");
        let file = directory.join("main.rs");
        fs::write(&file, "").unwrap();

        let action = parse([file.to_string_lossy().into_owned()], Path::new("/")).unwrap();
        let CliAction::Open { files, .. } = action else {
            panic!("esperava abrir arquivo");
        };

        assert_eq!(files, vec![fs::canonicalize(&file).unwrap()]);
    }

    #[test]
    fn rejects_invalid_path() {
        let directory = TempDir::new("invalid");

        assert!(matches!(
            parse(["nao-existe.rs"], &directory.path),
            Err(CliError::NotFound { .. })
        ));
    }

    #[test]
    fn rejects_file_outside_explicit_workspace() {
        let directory = TempDir::new("outside");
        let project = directory.join("projeto");
        fs::create_dir(&project).unwrap();
        let outside = directory.join("fora.rs");
        fs::write(&outside, "").unwrap();

        assert!(matches!(
            parse(
                ["projeto".to_owned(), outside.to_string_lossy().into_owned()],
                &directory.path
            ),
            Err(CliError::OutsideWorkspace { .. })
        ));
    }

    #[test]
    fn rejects_multiple_workspaces() {
        let directory = TempDir::new("multi-workspace");
        fs::create_dir(directory.join("a")).unwrap();
        fs::create_dir(directory.join("b")).unwrap();

        assert!(matches!(
            parse(["a", "b"], &directory.path),
            Err(CliError::MultipleWorkspaces)
        ));
    }

    #[test]
    fn infers_project_root_from_marker() {
        let directory = TempDir::new("marker");
        let project = directory.join("projeto");
        let nested = project.join("src");
        fs::create_dir_all(&nested).unwrap();
        fs::write(project.join("Cargo.toml"), "[package]").unwrap();
        let file = nested.join("main.rs");
        fs::write(&file, "").unwrap();

        let action = parse([file.to_string_lossy().into_owned()], &directory.path).unwrap();
        let CliAction::Open { workspace, .. } = action else {
            panic!("esperava abrir arquivo");
        };

        assert_eq!(workspace, fs::canonicalize(&project).unwrap());
    }

    #[test]
    fn rejects_files_from_incompatible_projects() {
        let directory = TempDir::new("incompatible");
        let first = directory.join("um");
        let second = directory.join("dois");
        fs::create_dir_all(&first).unwrap();
        fs::create_dir_all(&second).unwrap();
        fs::write(first.join("Cargo.toml"), "").unwrap();
        fs::write(second.join("package.json"), "{}").unwrap();
        let a = first.join("a.rs");
        let b = second.join("b.js");
        fs::write(&a, "").unwrap();
        fs::write(&b, "").unwrap();

        assert!(matches!(
            parse(
                [
                    a.to_string_lossy().into_owned(),
                    b.to_string_lossy().into_owned()
                ],
                &directory.path
            ),
            Err(CliError::IncompatibleWorkspaces { .. })
        ));
    }

    #[test]
    fn deduplicates_repeated_files() {
        let directory = TempDir::new("dedupe");
        let file = directory.join("a.rs");
        fs::write(&file, "").unwrap();

        let action = parse(["a.rs", "./a.rs"], &directory.path).unwrap();
        let CliAction::Open { files, .. } = action else {
            panic!("esperava abrir arquivo");
        };

        assert_eq!(files.len(), 1);
    }
}
