use nexus_core::buffer::edit::TextRange;

/// Categoria visual de um trecho destacado.
///
/// O tema mapeia cada categoria para uma cor. Novas categorias podem ser
/// adicionadas sem alterar o Buffer ou o renderizador de linhas: o renderizador
/// consulta o tema pelo `TokenKind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TokenKind {
    Keyword,
    String,
    Comment,
    Number,
    Function,
    Method,
    Type,
    Variable,
    Constant,
    Operator,
    Attribute,
}

impl TokenKind {
    /// Prioridade usada para resolver sobreposições entre capturas.
    ///
    /// Capturas mais específicas (funções, tipos, constantes) vencem capturas
    /// genéricas de identificador.
    pub(crate) fn priority(self) -> u8 {
        match self {
            TokenKind::Function
            | TokenKind::Method
            | TokenKind::Type
            | TokenKind::Constant
            | TokenKind::Attribute
            | TokenKind::Operator => 3,
            TokenKind::Keyword | TokenKind::String | TokenKind::Comment | TokenKind::Number => 2,
            TokenKind::Variable => 1,
        }
    }
}

/// Trecho destacado em offsets de caracteres do documento.
///
/// Os offsets seguem a mesma representação canônica do Buffer
/// (`CharOffset`), nunca bytes do Tree-sitter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HighlightSpan {
    pub range: TextRange,
    pub kind: TokenKind,
}
