use std::io::{self, Stdout};
use std::sync::{Mutex, MutexGuard, TryLockError};

use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::cursor::{Hide, Show};
use ratatui::crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode as crossterm_disable_raw_mode,
    enable_raw_mode as crossterm_enable_raw_mode,
};

pub type Tui = Terminal<CrosstermBackend<Stdout>>;

/// Partes do terminal que podem precisar de restauração.
///
/// Cada parte só permanece marcada enquanto sua restauração não foi confirmada
/// com sucesso. Isso permite repetir apenas o que falhou.
#[derive(Debug, Default)]
struct RestoreState {
    raw_mode: bool,
    cursor_hidden: bool,
    alternate_screen: bool,
    mouse_capture: bool,
}

impl RestoreState {
    const fn new() -> Self {
        Self {
            raw_mode: false,
            cursor_hidden: false,
            alternate_screen: false,
            mouse_capture: false,
        }
    }

    fn is_active(&self) -> bool {
        self.raw_mode || self.cursor_hidden || self.alternate_screen || self.mouse_capture
    }

    /// Tenta restaurar todas as partes ativas, independentemente de falhas
    /// anteriores.
    ///
    /// Uma parte só é desmarcada depois que sua operação retorna sucesso.
    /// Partes que falharem continuam marcadas para uma nova tentativa.
    fn restore(&mut self, ops: &mut impl TerminalOps) -> RestoreErrors {
        let mut errors = RestoreErrors::default();

        if self.raw_mode {
            match ops.disable_raw_mode() {
                Ok(()) => self.raw_mode = false,
                Err(error) => errors.raw_mode = Some(error),
            }
        }

        if self.cursor_hidden {
            match ops.show_cursor() {
                Ok(()) => self.cursor_hidden = false,
                Err(error) => errors.cursor = Some(error),
            }
        }

        if self.alternate_screen {
            match ops.leave_alternate_screen() {
                Ok(()) => self.alternate_screen = false,
                Err(error) => errors.alternate_screen = Some(error),
            }
        }

        if self.mouse_capture {
            match ops.disable_mouse_capture() {
                Ok(()) => self.mouse_capture = false,
                Err(error) => errors.mouse = Some(error),
            }
        }

        errors
    }
}

/// Erros individuais de uma tentativa de restauração.
#[derive(Debug, Default)]
struct RestoreErrors {
    raw_mode: Option<io::Error>,
    cursor: Option<io::Error>,
    alternate_screen: Option<io::Error>,
    mouse: Option<io::Error>,
}

impl RestoreErrors {
    fn is_empty(&self) -> bool {
        self.raw_mode.is_none()
            && self.cursor.is_none()
            && self.alternate_screen.is_none()
            && self.mouse.is_none()
    }

    fn into_result(self) -> io::Result<()> {
        if self.is_empty() {
            Ok(())
        } else {
            Err(self.into_io_error())
        }
    }

    fn into_io_error(self) -> io::Error {
        let mut details = Vec::new();

        if let Some(error) = self.raw_mode {
            details.push(format!("raw mode: {error}"));
        }
        if let Some(error) = self.cursor {
            details.push(format!("cursor: {error}"));
        }
        if let Some(error) = self.alternate_screen {
            details.push(format!("alternate screen: {error}"));
        }
        if let Some(error) = self.mouse {
            details.push(format!("mouse capture: {error}"));
        }

        io::Error::other(details.join("; "))
    }
}

/// Operações de terminal usadas na restauração.
///
/// A abstração permite injetar falhas controladas nos testes sem alterar o
/// caminho de produção.
trait TerminalOps {
    fn disable_raw_mode(&mut self) -> io::Result<()>;
    fn show_cursor(&mut self) -> io::Result<()>;
    fn leave_alternate_screen(&mut self) -> io::Result<()>;
    fn disable_mouse_capture(&mut self) -> io::Result<()>;
}

struct SystemOps;

impl TerminalOps for SystemOps {
    fn disable_raw_mode(&mut self) -> io::Result<()> {
        crossterm_disable_raw_mode()
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        execute!(io::stdout(), Show)
    }

    fn leave_alternate_screen(&mut self) -> io::Result<()> {
        execute!(io::stdout(), LeaveAlternateScreen)
    }

    fn disable_mouse_capture(&mut self) -> io::Result<()> {
        execute!(io::stdout(), DisableMouseCapture)
    }
}

static TERMINAL_STATE: Mutex<RestoreState> = Mutex::new(RestoreState::new());

fn lock_state() -> MutexGuard<'static, RestoreState> {
    TERMINAL_STATE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn try_lock_state() -> Option<MutexGuard<'static, RestoreState>> {
    match TERMINAL_STATE.try_lock() {
        Ok(guard) => Some(guard),
        Err(TryLockError::Poisoned(poisoned)) => Some(poisoned.into_inner()),
        Err(TryLockError::WouldBlock) => None,
    }
}

/// Sessão responsável pelo ciclo de vida do terminal.
///
/// A guarda de restauração é declarada antes do terminal para que o `Drop`
/// seja executado primeiro. Ela permanece armada enquanto existir alguma parte
/// do terminal cuja restauração não foi confirmada.
pub struct TerminalSession {
    restore: RestoreGuard,
    terminal: Tui,
}

impl TerminalSession {
    pub fn terminal_mut(&mut self) -> &mut Tui {
        &mut self.terminal
    }

    pub fn size(&self) -> io::Result<(u16, u16)> {
        let area = self.terminal.size()?;
        Ok((area.width, area.height))
    }

    /// Tenta restaurar o terminal e consome a sessão.
    ///
    /// Se a tentativa falhar, a guarda continua armada e o `Drop` faz uma nova
    /// tentativa. O erro retornado descreve a primeira tentativa.
    pub fn shutdown(self) -> io::Result<()> {
        self.restore.shutdown()
    }
}

/// Executa uma restauração e permanece armada até obter sucesso.
trait RestoreRuntime {
    fn restore(&mut self) -> io::Result<()>;
}

struct SystemRestore;

impl RestoreRuntime for SystemRestore {
    fn restore(&mut self) -> io::Result<()> {
        restore_terminal()
    }
}

struct RestoreGuard<R: RestoreRuntime = SystemRestore> {
    runtime: R,
    armed: bool,
}

impl<R: RestoreRuntime> RestoreGuard<R> {
    fn new(runtime: R) -> Self {
        Self {
            runtime,
            armed: true,
        }
    }

    fn shutdown(mut self) -> io::Result<()> {
        let result = self.runtime.restore();

        if result.is_ok() {
            self.armed = false;
        }

        result
    }
}

impl<R: RestoreRuntime> Drop for RestoreGuard<R> {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.runtime.restore();
        }
    }
}

/// Entra em raw mode e no alternate screen, preparando o backend Crossterm.
///
/// As partes são marcadas antes da operação correspondente porque uma falha
/// pode ocorrer depois de a sequência já ter sido parcialmente escrita. Em
/// qualquer falha, a restauração é tentada e o erro original é preservado.
pub fn init() -> io::Result<TerminalSession> {
    crossterm_enable_raw_mode()?;

    {
        let mut state = lock_state();
        state.raw_mode = true;
    }

    let mut stdout = io::stdout();

    {
        let mut state = lock_state();
        state.alternate_screen = true;
    }

    if let Err(error) = execute!(stdout, EnterAlternateScreen) {
        return Err(cleanup_after_error(error));
    }

    {
        let mut state = lock_state();
        state.cursor_hidden = true;
    }

    if let Err(error) = execute!(stdout, Hide) {
        return Err(cleanup_after_error(error));
    }

    {
        let mut state = lock_state();
        state.mouse_capture = true;
    }

    if let Err(error) = execute!(stdout, EnableMouseCapture) {
        return Err(cleanup_after_error(error));
    }

    let backend = CrosstermBackend::new(stdout);

    match Terminal::new(backend) {
        Ok(terminal) => Ok(TerminalSession {
            restore: RestoreGuard::new(SystemRestore),
            terminal,
        }),
        Err(error) => Err(cleanup_after_error(error)),
    }
}

/// Restaura cursor, raw mode e alternate screen, tentando todas as partes.
///
/// É seguro chamar esta função mais de uma vez. Somente as partes cuja
/// restauração ainda não foi confirmada são executadas.
pub fn restore_terminal() -> io::Result<()> {
    let mut state = lock_state();

    if !state.is_active() {
        return Ok(());
    }

    state.restore(&mut SystemOps).into_result()
}

fn restore_from_panic_hook() {
    let Some(mut state) = try_lock_state() else {
        // Outra restauração está em andamento nesta thread. O `Drop` da guarda
        // fará uma nova tentativa depois que o unwind liberar o lock.
        return;
    };

    if state.is_active() {
        let _ = state.restore(&mut SystemOps).into_result();
    }
}

/// Restaura o terminal antes de delegar ao hook de pânico anterior.
///
/// Deve ser instalado no início de `main`, antes de qualquer inicialização da
/// interface.
pub fn install_panic_hook() {
    let previous = std::panic::take_hook();

    std::panic::set_hook(Box::new(move |info| {
        restore_from_panic_hook();
        previous(info);
    }));
}

fn cleanup_after_error(primary: io::Error) -> io::Error {
    match restore_terminal() {
        Ok(()) => primary,
        Err(restore_error) => io::Error::new(
            primary.kind(),
            format!("{primary}; falha adicional ao restaurar o terminal: {restore_error}"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::collections::VecDeque;
    use std::rc::Rc;

    use super::*;

    #[derive(Default)]
    struct MockOps {
        raw_mode: VecDeque<io::Result<()>>,
        cursor: VecDeque<io::Result<()>>,
        alternate_screen: VecDeque<io::Result<()>>,
        mouse: VecDeque<io::Result<()>>,
        raw_calls: usize,
        cursor_calls: usize,
        alternate_calls: usize,
        mouse_calls: usize,
    }

    impl TerminalOps for MockOps {
        fn disable_raw_mode(&mut self) -> io::Result<()> {
            self.raw_calls += 1;
            self.raw_mode.pop_front().unwrap_or(Ok(()))
        }

        fn show_cursor(&mut self) -> io::Result<()> {
            self.cursor_calls += 1;
            self.cursor.pop_front().unwrap_or(Ok(()))
        }

        fn leave_alternate_screen(&mut self) -> io::Result<()> {
            self.alternate_calls += 1;
            self.alternate_screen.pop_front().unwrap_or(Ok(()))
        }

        fn disable_mouse_capture(&mut self) -> io::Result<()> {
            self.mouse_calls += 1;
            self.mouse.pop_front().unwrap_or(Ok(()))
        }
    }

    fn failure(message: &str) -> io::Result<()> {
        Err(io::Error::other(message.to_owned()))
    }

    fn active_state() -> RestoreState {
        RestoreState {
            raw_mode: true,
            cursor_hidden: true,
            alternate_screen: true,
            mouse_capture: true,
        }
    }

    #[test]
    fn raw_mode_failure_stays_retryable_and_other_parts_are_restored() {
        let mut state = active_state();
        let mut ops = MockOps {
            raw_mode: VecDeque::from([failure("raw mode failure")]),
            ..MockOps::default()
        };

        let errors = state.restore(&mut ops);

        assert!(errors.raw_mode.is_some());
        assert!(errors.cursor.is_none());
        assert!(errors.alternate_screen.is_none());
        assert_eq!(ops.raw_calls, 1);
        assert_eq!(ops.cursor_calls, 1);
        assert_eq!(ops.alternate_calls, 1);
        assert!(state.raw_mode);
        assert!(!state.cursor_hidden);
        assert!(!state.alternate_screen);
    }

    #[test]
    fn cursor_failure_stays_retryable() {
        let mut state = active_state();
        let mut ops = MockOps {
            cursor: VecDeque::from([failure("cursor failure")]),
            ..MockOps::default()
        };

        let errors = state.restore(&mut ops);

        assert!(errors.raw_mode.is_none());
        assert!(errors.cursor.is_some());
        assert!(errors.alternate_screen.is_none());
        assert!(!state.raw_mode);
        assert!(state.cursor_hidden);
        assert!(!state.alternate_screen);
    }

    #[test]
    fn alternate_screen_failure_stays_retryable() {
        let mut state = active_state();
        let mut ops = MockOps {
            alternate_screen: VecDeque::from([failure("screen failure")]),
            ..MockOps::default()
        };

        let errors = state.restore(&mut ops);

        assert!(errors.raw_mode.is_none());
        assert!(errors.cursor.is_none());
        assert!(errors.alternate_screen.is_some());
        assert!(!state.raw_mode);
        assert!(!state.cursor_hidden);
        assert!(state.alternate_screen);
    }

    #[test]
    fn partial_failure_retry_repeats_only_the_failed_parts() {
        let mut state = active_state();
        let mut first = MockOps {
            raw_mode: VecDeque::from([failure("raw mode failure")]),
            cursor: VecDeque::from([failure("cursor failure")]),
            ..MockOps::default()
        };

        let errors = state.restore(&mut first);

        assert!(errors.raw_mode.is_some());
        assert!(errors.cursor.is_some());
        assert!(errors.alternate_screen.is_none());
        assert!(state.raw_mode);
        assert!(state.cursor_hidden);
        assert!(!state.alternate_screen);
        assert_eq!(first.alternate_calls, 1);

        let mut second = MockOps::default();
        let errors = state.restore(&mut second);

        assert!(errors.is_empty());
        assert_eq!(second.raw_calls, 1);
        assert_eq!(second.cursor_calls, 1);
        assert_eq!(second.alternate_calls, 0);
        assert!(!state.is_active());

        let mut third = MockOps::default();
        let errors = state.restore(&mut third);

        assert!(errors.is_empty());
        assert_eq!(third.raw_calls, 0);
        assert_eq!(third.cursor_calls, 0);
        assert_eq!(third.alternate_calls, 0);
    }

    #[test]
    fn successful_restore_is_not_repeated() {
        let mut state = active_state();
        let mut first = MockOps::default();

        let errors = state.restore(&mut first);

        assert!(errors.is_empty());
        assert_eq!(first.raw_calls, 1);
        assert_eq!(first.cursor_calls, 1);
        assert_eq!(first.alternate_calls, 1);
        assert!(!state.is_active());

        let mut second = MockOps::default();
        let errors = state.restore(&mut second);

        assert!(errors.is_empty());
        assert_eq!(second.raw_calls, 0);
        assert_eq!(second.cursor_calls, 0);
        assert_eq!(second.alternate_calls, 0);
    }

    #[test]
    fn aggregate_error_preserves_each_individual_failure() {
        let errors = RestoreErrors {
            raw_mode: Some(io::Error::other("raw mode failure")),
            cursor: Some(io::Error::other("cursor failure")),
            alternate_screen: Some(io::Error::other("screen failure")),
            mouse: Some(io::Error::other("mouse failure")),
        };

        let message = errors.into_io_error().to_string();

        assert!(message.contains("raw mode: raw mode failure"));
        assert!(message.contains("cursor: cursor failure"));
        assert!(message.contains("alternate screen: screen failure"));
    }

    struct MockRuntime {
        calls: Rc<Cell<usize>>,
        results: VecDeque<io::Result<()>>,
    }

    impl RestoreRuntime for MockRuntime {
        fn restore(&mut self) -> io::Result<()> {
            self.calls.set(self.calls.get() + 1);
            self.results.pop_front().unwrap_or(Ok(()))
        }
    }

    fn mock_runtime(results: Vec<io::Result<()>>) -> (MockRuntime, Rc<Cell<usize>>) {
        let calls = Rc::new(Cell::new(0));
        (
            MockRuntime {
                calls: Rc::clone(&calls),
                results: VecDeque::from(results),
            },
            calls,
        )
    }

    #[test]
    fn successful_shutdown_does_not_restore_again_on_drop() {
        let (runtime, calls) = mock_runtime(vec![Ok(())]);

        RestoreGuard::new(runtime).shutdown().unwrap();

        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn failed_shutdown_is_retried_by_drop() {
        let (runtime, calls) = mock_runtime(vec![failure("first failure"), Ok(())]);

        let result = RestoreGuard::new(runtime).shutdown();

        assert!(result.is_err());
        assert_eq!(calls.get(), 2);
    }

    #[test]
    fn failed_shutdown_and_drop_retry_are_bounded() {
        let (runtime, calls) =
            mock_runtime(vec![failure("first failure"), failure("second failure")]);

        let result = RestoreGuard::new(runtime).shutdown();

        assert!(result.is_err());
        assert_eq!(calls.get(), 2);
    }

    #[test]
    fn drop_without_shutdown_attempts_restore_once() {
        let (runtime, calls) = mock_runtime(vec![Ok(())]);

        drop(RestoreGuard::new(runtime));

        assert_eq!(calls.get(), 1);
    }
}
