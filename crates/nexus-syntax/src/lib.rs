//! Análise sintática incremental e highlighting para a Nexus.
//!
//! O crate usa Tree-sitter para produzir trechos destacados independentes de
//! interface. Ele não depende de Ratatui nem de `nexus-tui`; a TUI apenas
//! converte [`TokenKind`] em cores do tema ativo.
//!
//! # Posições
//!
//! Toda posição pública usa [`CharOffset`], a representação canônica do Core.
//! Internamente, o estado mantém um índice de linhas para converter entre
//! caracteres Unicode e bytes/pontos do Tree-sitter. Nenhum offset de byte é
//! exposto como se fosse um offset de caractere.
//!
//! # Atualização incremental
//!
//! [`SyntaxState::apply_edit`] recebe o intervalo e os textos de uma edição já
//! aplicada ao Buffer e atualiza a árvore com [`tree_sitter::InputEdit`].
//! [`SyntaxState::resync`] calcula a menor edição entre dois textos, cobrindo
//! Undo/Redo sem exigir alterações no contrato do Core.

mod language;
mod queries;
mod state;
mod token;

pub use language::LanguageId;
pub use state::{SyntaxError, SyntaxState, language_for_path, validate_language};
pub use token::{HighlightSpan, TokenKind};

pub use nexus_core::buffer::edit::TextRange;
pub use nexus_core::buffer::position::CharOffset;
