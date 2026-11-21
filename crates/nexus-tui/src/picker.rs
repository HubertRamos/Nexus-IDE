//! Seleção nativa de arquivos e diretórios.
//!
//! A abstração [`FilePicker`] não depende da TUI. Ela devolve caminhos ou
//! cancelamento, permitindo que uma futura interface gráfica reutilize a mesma
//! lógica. A implementação padrão usa diálogos nativos: XDG Desktop Portal no
//! Linux e os controles do Windows no Windows.
//!
//! O terminal não é manipulado aqui. A suspensão e a retomada ficam na camada
//! de terminal, para que o seletor e a interface permaneçam desacoplados.

use std::fmt;
use std::path::{Path, PathBuf};

/// Tipo de seleção solicitada.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerKind {
    Files,
    Directory,
}

/// Pedido de seleção, incluindo o diretório inicial quando conhecido.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickerRequest {
    pub kind: PickerKind,
    pub start_directory: Option<PathBuf>,
}

impl PickerRequest {
    pub fn new(kind: PickerKind, start_directory: Option<PathBuf>) -> Self {
        Self {
            kind,
            start_directory,
        }
    }
}

/// Resultado de uma seleção concluída.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PickerSelection {
    Files(Vec<PathBuf>),
    Directory(PathBuf),
}

/// Falhas ao abrir ou executar o seletor nativo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilePickerError {
    Unavailable(String),
    Failed(String),
}

impl fmt::Display for FilePickerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FilePickerError::Unavailable(message) => write!(formatter, "{message}"),
            FilePickerError::Failed(message) => write!(formatter, "{message}"),
        }
    }
}

impl std::error::Error for FilePickerError {}

/// Abstração independente da interface para seleção de arquivos e pastas.
///
/// `Ok(None)` representa cancelamento; erros representam impossibilidade de
/// abrir ou executar o diálogo. Um ou mais arquivos são devolvidos por
/// [`FilePicker::pick_files`] e uma pasta por [`FilePicker::pick_folder`].
pub trait FilePicker {
    fn pick_files(
        &self,
        start_directory: Option<&Path>,
    ) -> Result<Option<Vec<PathBuf>>, FilePickerError>;

    fn pick_folder(
        &self,
        start_directory: Option<&Path>,
    ) -> Result<Option<PathBuf>, FilePickerError>;
}

/// Seletor nativo do sistema.
#[derive(Debug, Default, Clone, Copy)]
pub struct NativeFilePicker;

impl NativeFilePicker {
    pub fn new() -> Self {
        Self
    }

    /// Indica se há um ambiente onde um diálogo gráfico pode ser iniciado.
    ///
    /// A verificação é deliberadamente conservadora no Linux: sem `DISPLAY` ou
    /// `WAYLAND_DISPLAY`, o chamador deve usar a entrada manual de caminho em
    /// vez de tentar abrir um diálogo que não apareceria. No Windows os
    /// diálogos nativos estão sempre disponíveis para o processo.
    pub fn interactive_available() -> bool {
        #[cfg(target_os = "linux")]
        {
            std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some()
        }

        #[cfg(not(target_os = "linux"))]
        {
            true
        }
    }

    fn ensure_available() -> Result<(), FilePickerError> {
        if Self::interactive_available() {
            Ok(())
        } else {
            Err(FilePickerError::Unavailable(
                "nenhum ambiente gráfico detectado".to_owned(),
            ))
        }
    }
}

impl FilePicker for NativeFilePicker {
    fn pick_files(
        &self,
        start_directory: Option<&Path>,
    ) -> Result<Option<Vec<PathBuf>>, FilePickerError> {
        Self::ensure_available()?;

        let mut dialog = rfd::FileDialog::new().set_title("Abrir arquivos no Nexus");
        if let Some(directory) = start_directory {
            dialog = dialog.set_directory(directory);
        }

        Ok(dialog.pick_files())
    }

    fn pick_folder(
        &self,
        start_directory: Option<&Path>,
    ) -> Result<Option<PathBuf>, FilePickerError> {
        Self::ensure_available()?;

        let mut dialog = rfd::FileDialog::new().set_title("Abrir projeto no Nexus");
        if let Some(directory) = start_directory {
            dialog = dialog.set_directory(directory);
        }

        Ok(dialog.pick_folder())
    }
}

/// Executa um [`PickerRequest`] com qualquer implementação de [`FilePicker`].
///
/// Converte o resultado da implementação para o vocabulário da aplicação e
/// preserva cancelamento (`Ok(None)`) e erros sem abrir diálogos adicionais.
pub fn run_picker<P: FilePicker>(
    picker: &P,
    request: &PickerRequest,
) -> Result<Option<PickerSelection>, FilePickerError> {
    match request.kind {
        PickerKind::Files => match picker.pick_files(request.start_directory.as_deref())? {
            None => Ok(None),
            Some(paths) if paths.is_empty() => Ok(None),
            Some(paths) if paths.iter().any(|path| path.as_os_str().is_empty()) => Err(
                FilePickerError::Failed("o seletor retornou um caminho vazio".to_owned()),
            ),
            Some(paths) => Ok(Some(PickerSelection::Files(paths))),
        },
        PickerKind::Directory => match picker.pick_folder(request.start_directory.as_deref())? {
            None => Ok(None),
            Some(path) if path.as_os_str().is_empty() => Err(FilePickerError::Failed(
                "o seletor retornou um caminho vazio".to_owned(),
            )),
            Some(path) => Ok(Some(PickerSelection::Directory(path))),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MockPicker {
        files: Option<Result<Option<Vec<PathBuf>>, FilePickerError>>,
        folder: Option<Result<Option<PathBuf>, FilePickerError>>,
    }

    impl MockPicker {
        fn files(result: Result<Option<Vec<PathBuf>>, FilePickerError>) -> Self {
            Self {
                files: Some(result),
                folder: None,
            }
        }

        fn folder(result: Result<Option<PathBuf>, FilePickerError>) -> Self {
            Self {
                files: None,
                folder: Some(result),
            }
        }
    }

    impl FilePicker for MockPicker {
        fn pick_files(
            &self,
            _start_directory: Option<&Path>,
        ) -> Result<Option<Vec<PathBuf>>, FilePickerError> {
            self.files.clone().expect("chamada inesperada a pick_files")
        }

        fn pick_folder(
            &self,
            _start_directory: Option<&Path>,
        ) -> Result<Option<PathBuf>, FilePickerError> {
            self.folder
                .clone()
                .expect("chamada inesperada a pick_folder")
        }
    }

    fn request(kind: PickerKind) -> PickerRequest {
        PickerRequest::new(kind, Some(PathBuf::from("/projeto")))
    }

    #[test]
    fn returns_a_single_selected_file() {
        let path = PathBuf::from("/projeto/main.rs");
        let picker = MockPicker::files(Ok(Some(vec![path.clone()])));

        let selection = run_picker(&picker, &request(PickerKind::Files)).unwrap();

        assert_eq!(selection, Some(PickerSelection::Files(vec![path])));
    }

    #[test]
    fn returns_multiple_selected_files_in_order() {
        let paths = vec![
            PathBuf::from("/projeto/a.rs"),
            PathBuf::from("/projeto/b.rs"),
            PathBuf::from("/projeto/c.rs"),
        ];
        let picker = MockPicker::files(Ok(Some(paths.clone())));

        let selection = run_picker(&picker, &request(PickerKind::Files)).unwrap();

        assert_eq!(selection, Some(PickerSelection::Files(paths)));
    }

    #[test]
    fn returns_a_selected_directory() {
        let path = PathBuf::from("/projeto");
        let picker = MockPicker::folder(Ok(Some(path.clone())));

        let selection = run_picker(&picker, &request(PickerKind::Directory)).unwrap();

        assert_eq!(selection, Some(PickerSelection::Directory(path)));
    }

    #[test]
    fn cancellation_is_not_an_error() {
        let picker = MockPicker::files(Ok(None));

        assert_eq!(
            run_picker(&picker, &request(PickerKind::Files)).unwrap(),
            None
        );

        let picker = MockPicker::folder(Ok(None));
        assert_eq!(
            run_picker(&picker, &request(PickerKind::Directory)).unwrap(),
            None
        );
    }

    #[test]
    fn empty_file_list_is_treated_as_cancellation() {
        let picker = MockPicker::files(Ok(Some(Vec::new())));

        assert_eq!(
            run_picker(&picker, &request(PickerKind::Files)).unwrap(),
            None
        );
    }

    #[test]
    fn empty_selected_path_is_reported_as_an_error() {
        let picker = MockPicker::files(Ok(Some(vec![PathBuf::new()])));
        assert!(matches!(
            run_picker(&picker, &request(PickerKind::Files)),
            Err(FilePickerError::Failed(message)) if message.contains("vazio")
        ));

        let picker = MockPicker::folder(Ok(Some(PathBuf::new())));
        assert!(matches!(
            run_picker(&picker, &request(PickerKind::Directory)),
            Err(FilePickerError::Failed(message)) if message.contains("vazio")
        ));
    }

    #[test]
    fn selection_errors_are_propagated() {
        let picker = MockPicker::files(Err(FilePickerError::Failed(
            "falha ao abrir o diálogo".to_owned(),
        )));

        assert!(matches!(
            run_picker(&picker, &request(PickerKind::Files)),
            Err(FilePickerError::Failed(message)) if message.contains("diálogo")
        ));
    }

    #[test]
    fn unicode_and_spaces_in_paths_are_preserved() {
        let paths = vec![
            PathBuf::from("/tmp/projeto com espaço/arquivo café.rs"),
            PathBuf::from("/tmp/projeto com espaço/界.txt"),
        ];
        let picker = MockPicker::files(Ok(Some(paths.clone())));

        let selection = run_picker(&picker, &request(PickerKind::Files)).unwrap();

        assert_eq!(selection, Some(PickerSelection::Files(paths)));
    }

    #[test]
    fn directory_with_spaces_and_unicode_is_preserved() {
        let path = PathBuf::from("/tmp/projeto café 界");
        let picker = MockPicker::folder(Ok(Some(path.clone())));

        let selection = run_picker(&picker, &request(PickerKind::Directory)).unwrap();

        assert_eq!(selection, Some(PickerSelection::Directory(path)));
    }
}
