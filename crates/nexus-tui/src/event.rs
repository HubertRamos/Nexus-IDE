use std::io;
use std::time::Duration;

use ratatui::crossterm::event::{
    self as crossterm_event, Event as CrosstermEvent, KeyEvent, MouseEvent,
};

/// Intervalo máximo de espera por eventos.
///
/// O `poll` bloqueia até o timeout, evitando um loop ocupado quando o usuário
/// está inativo.
pub const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Evento de entrada já traduzido para o vocabulário da aplicação.
#[derive(Debug, Clone)]
pub enum Event {
    Key(KeyEvent),
    Mouse(MouseEvent),
    Resize { width: u16, height: u16 },
}

/// Aguarda um evento por até `timeout`.
///
/// Retorna `Ok(None)` quando o intervalo expira ou quando o evento recebido
/// ainda não é tratado por esta etapa (mouse, foco, colagem, etc.).
pub fn read(timeout: Duration) -> io::Result<Option<Event>> {
    if !crossterm_event::poll(timeout)? {
        return Ok(None);
    }

    match crossterm_event::read()? {
        CrosstermEvent::Key(key) => Ok(Some(Event::Key(key))),
        CrosstermEvent::Mouse(mouse) => Ok(Some(Event::Mouse(mouse))),
        CrosstermEvent::Resize(width, height) => Ok(Some(Event::Resize { width, height })),
        _ => Ok(None),
    }
}
