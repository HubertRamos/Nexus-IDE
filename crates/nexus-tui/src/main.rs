mod app;
mod cli;
mod document;
mod event;
mod explorer;
mod files;
mod input;
mod paths;
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

    let mut session = terminal::init()?;
    let (width, height) = session.size()?;
    let mut app = app::App::with_environment(width, height);

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
    }

    app.on_idle();

    Ok(())
}
