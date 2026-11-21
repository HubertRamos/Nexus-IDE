mod app;
mod cli;
mod diagnostics;
mod document;
mod document_sync;
mod event;
mod explorer;
mod files;
mod input;
mod lsp_sink;
mod navigation;
mod paths;
mod picker;
mod recent;
mod recovery;
mod session;
mod terminal;
mod theme;
mod ui;
mod view;
mod watcher;
mod workspace;

use std::io;
use std::process::ExitCode;

fn main() -> ExitCode {
    terminal::install_panic_hook();

    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("nexus-tui: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let cwd = std::env::current_dir()?;
    let arguments: Vec<String> = std::env::args().skip(1).collect();

    let action = match cli::parse(arguments, &cwd) {
        Ok(action) => action,
        Err(error) => {
            print!("{}", cli::help_text());
            return Err(io::Error::new(io::ErrorKind::InvalidInput, error.to_string()).into());
        }
    };

    match action {
        cli::CliAction::Help => {
            print!("{}", cli::help_text());
            return Ok(());
        }
        cli::CliAction::Version => {
            println!("{}", cli::version_text());
            return Ok(());
        }
        cli::CliAction::Start | cli::CliAction::Open { .. } => {}
    }

    // A thread do LSP é independente do laço da interface. Ela é criada antes
    // do terminal para que uma falha de spawn não deixe a tela alternativa
    // ativa. Nenhum processo de servidor é iniciado sem um documento.
    let lsp_service = nexus_lsp::service::LspService::start()?;

    let mut session = terminal::init()?;
    let (width, height) = session.size()?;
    let mut app = app::App::with_environment(width, height);

    // Instalar o sink antes de abrir workspace/arquivos garante que os
    // primeiros documentos já sejam sincronizados.
    app.set_sync_sink(Box::new(lsp_sink::LspSyncSink::new(lsp_service)));

    match action {
        cli::CliAction::Start => {
            if app.should_show_start_screen() {
                app.show_start_screen();
            }
        }
        cli::CliAction::Open { workspace, files } => {
            if let Err(error) = app.open_workspace(&workspace) {
                app.report_error(error.to_string());
            }

            for file in files {
                if let Err(error) = app.open_document_in_new_tab(&file) {
                    app.report_error(error.to_string());
                }
            }
        }
        cli::CliAction::Help | cli::CliAction::Version => unreachable!(),
    }

    let run_result = event_loop(&mut session, &mut app);
    app.shutdown_lsp();
    let restore_result = session.shutdown();

    match (run_result, restore_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error.into()),
        (Err(run_error), Err(restore_error)) => Err(io::Error::new(
            run_error.kind(),
            format!("{run_error}; falha adicional ao restaurar o terminal: {restore_error}"),
        )
        .into()),
    }
}

fn event_loop(session: &mut terminal::TerminalSession, app: &mut app::App) -> io::Result<()> {
    let picker = picker::NativeFilePicker::new();

    while app.is_running() {
        if app.needs_redraw() {
            session
                .terminal_mut()
                .draw(|frame| ui::render(frame, app))?;
            app.mark_rendered();
        }

        if let Some(event) = event::read(event::POLL_INTERVAL)? {
            app.update(event);
        } else {
            app.on_idle();
        }

        if let Some(request) = app.take_picker_request() {
            finish_picker(session, app, &picker, request)?;
        }
    }

    app.on_idle();

    Ok(())
}

/// Falhas ao redor de um diálogo externo.
#[derive(Debug)]
enum PickerFlowError {
    Terminal(io::Error),
    Picker(picker::FilePickerError),
}

/// Executa um diálogo com o terminal suspenso e sempre o retoma.
///
/// A suspensão acontece antes de qualquer chamada ao seletor. A retomada é
/// tentada mesmo quando o seletor retorna erro, garantindo que o TUI volte ao
/// estado normal em sucesso, cancelamento e falha.
fn run_picker_dialog<L, P>(
    lifecycle: &mut L,
    picker: &P,
    request: &picker::PickerRequest,
) -> Result<Option<picker::PickerSelection>, PickerFlowError>
where
    L: terminal::TerminalLifecycle,
    P: picker::FilePicker,
{
    lifecycle.suspend().map_err(PickerFlowError::Terminal)?;

    let result = picker::run_picker(picker, request);

    lifecycle.resume().map_err(PickerFlowError::Terminal)?;

    result.map_err(PickerFlowError::Picker)
}

/// Aplica o resultado do seletor à aplicação.
fn finish_picker<L, P>(
    lifecycle: &mut L,
    app: &mut app::App,
    picker: &P,
    request: picker::PickerRequest,
) -> io::Result<()>
where
    L: terminal::TerminalLifecycle,
    P: picker::FilePicker,
{
    match run_picker_dialog(lifecycle, picker, &request) {
        Ok(selection) => app.complete_picker(selection),
        Err(PickerFlowError::Picker(error)) => app.report_picker_error(request.kind, &error),
        Err(PickerFlowError::Terminal(error)) => return Err(error),
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::picker::{FilePicker, FilePickerError, PickerKind, PickerSelection};

    #[derive(Default)]
    struct MockLifecycle {
        suspend_calls: usize,
        resume_calls: usize,
        suspend_error: Option<io::ErrorKind>,
        resume_error: bool,
    }

    impl terminal::TerminalLifecycle for MockLifecycle {
        fn suspend(&mut self) -> io::Result<()> {
            self.suspend_calls += 1;
            match self.suspend_error {
                Some(kind) => Err(io::Error::new(kind, "falha ao suspender")),
                None => Ok(()),
            }
        }

        fn resume(&mut self) -> io::Result<()> {
            self.resume_calls += 1;
            if self.resume_error {
                Err(io::Error::other("falha ao retomar"))
            } else {
                Ok(())
            }
        }
    }

    struct MockPicker {
        files: Option<Result<Option<Vec<std::path::PathBuf>>, FilePickerError>>,
        folder: Option<Result<Option<std::path::PathBuf>, FilePickerError>>,
    }

    impl MockPicker {
        fn files(result: Result<Option<Vec<std::path::PathBuf>>, FilePickerError>) -> Self {
            Self {
                files: Some(result),
                folder: None,
            }
        }

        fn folder(result: Result<Option<std::path::PathBuf>, FilePickerError>) -> Self {
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
        ) -> Result<Option<Vec<std::path::PathBuf>>, FilePickerError> {
            self.files.clone().expect("chamada inesperada a pick_files")
        }

        fn pick_folder(
            &self,
            _start_directory: Option<&Path>,
        ) -> Result<Option<std::path::PathBuf>, FilePickerError> {
            self.folder
                .clone()
                .expect("chamada inesperada a pick_folder")
        }
    }

    fn file_request() -> picker::PickerRequest {
        picker::PickerRequest::new(PickerKind::Files, None)
    }

    fn directory_request() -> picker::PickerRequest {
        picker::PickerRequest::new(PickerKind::Directory, None)
    }

    #[test]
    fn picker_success_suspends_and_resumes_the_terminal() {
        let path = std::path::PathBuf::from("/tmp/arquivo.rs");
        let mut lifecycle = MockLifecycle::default();
        let picker = MockPicker::files(Ok(Some(vec![path.clone()])));

        let result = run_picker_dialog(&mut lifecycle, &picker, &file_request());

        assert_eq!(result.unwrap(), Some(PickerSelection::Files(vec![path])));
        assert_eq!(lifecycle.suspend_calls, 1);
        assert_eq!(lifecycle.resume_calls, 1);
    }

    #[test]
    fn directory_picker_success_returns_a_workspace_selection() {
        let path = std::path::PathBuf::from("/tmp/projeto com espaço");
        let mut lifecycle = MockLifecycle::default();
        let picker = MockPicker::folder(Ok(Some(path.clone())));

        let result = run_picker_dialog(&mut lifecycle, &picker, &directory_request());

        assert_eq!(result.unwrap(), Some(PickerSelection::Directory(path)));
        assert_eq!(lifecycle.suspend_calls, 1);
        assert_eq!(lifecycle.resume_calls, 1);
    }

    #[test]
    fn picker_cancel_suspends_and_resumes_the_terminal() {
        let mut lifecycle = MockLifecycle::default();
        let picker = MockPicker::files(Ok(None));

        let result = run_picker_dialog(&mut lifecycle, &picker, &file_request());

        assert_eq!(result.unwrap(), None);
        assert_eq!(lifecycle.suspend_calls, 1);
        assert_eq!(lifecycle.resume_calls, 1);
    }

    #[test]
    fn picker_error_suspends_and_resumes_the_terminal() {
        let mut lifecycle = MockLifecycle::default();
        let picker =
            MockPicker::folder(Err(FilePickerError::Failed("falha no diálogo".to_owned())));

        let result = run_picker_dialog(&mut lifecycle, &picker, &directory_request());

        assert!(matches!(result, Err(PickerFlowError::Picker(_))));
        assert_eq!(lifecycle.suspend_calls, 1);
        assert_eq!(lifecycle.resume_calls, 1);
    }

    #[test]
    fn resume_failure_is_reported_as_a_terminal_error() {
        let mut lifecycle = MockLifecycle {
            resume_error: true,
            ..MockLifecycle::default()
        };
        let picker = MockPicker::files(Ok(None));

        let result = run_picker_dialog(&mut lifecycle, &picker, &file_request());

        assert!(matches!(result, Err(PickerFlowError::Terminal(_))));
        assert_eq!(lifecycle.suspend_calls, 1);
        assert_eq!(lifecycle.resume_calls, 1);
    }

    #[test]
    fn suspend_failure_skips_the_picker_and_resume() {
        let mut lifecycle = MockLifecycle {
            suspend_error: Some(io::ErrorKind::Other),
            ..MockLifecycle::default()
        };
        let picker = MockPicker::files(Ok(None));

        let result = run_picker_dialog(&mut lifecycle, &picker, &file_request());

        assert!(matches!(result, Err(PickerFlowError::Terminal(_))));
        assert_eq!(lifecycle.suspend_calls, 1);
        assert_eq!(lifecycle.resume_calls, 0);
    }

    #[test]
    fn picker_error_falls_back_to_the_manual_prompt() {
        let mut lifecycle = MockLifecycle::default();
        let picker = MockPicker::files(Err(FilePickerError::Unavailable(
            "sem ambiente gráfico".to_owned(),
        )));
        let mut application = app::App::with_text(80, 24, "");

        finish_picker(&mut lifecycle, &mut application, &picker, file_request()).unwrap();

        assert_eq!(application.mode(), app::Mode::Prompt(app::PromptKind::Open));
        assert_eq!(lifecycle.suspend_calls, 1);
        assert_eq!(lifecycle.resume_calls, 1);
        assert_eq!(
            application.status_message().map(|message| message.kind),
            Some(app::MessageKind::Error)
        );
    }
}
