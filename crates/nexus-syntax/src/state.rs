use std::fmt;
use std::sync::OnceLock;

use nexus_core::buffer::edit::TextRange;
use nexus_core::buffer::position::CharOffset;
use tree_sitter::{
    InputEdit, Language, Parser, Point, Query, QueryCursor, StreamingIterator, Tree,
};

use crate::language::LanguageId;
use crate::token::{HighlightSpan, TokenKind};

/// Estado de análise sintática de um documento.
///
/// O estado mantém uma cópia do texto, a árvore do Tree-sitter e os trechos
/// destacados em offsets de caracteres. Ele vive fora do `Buffer`, que
/// permanece independente de qualquer mecanismo de análise.
///
/// A árvore é atualizada de forma incremental a cada edição. A coleta de spans
/// é refeita a partir da árvore após cada edição, mas nunca durante a
/// renderização. Em documentos muito grandes, uma edição pode causar uma pausa
/// breve enquanto os spans são recalculados; não há parsing integral por frame.
pub struct SyntaxState {
    language: LanguageId,
    parser: Parser,
    tree: Option<Tree>,
    text: String,
    index: LineIndex,
    highlights: Vec<HighlightSpan>,
    update_count: u64,
}

impl SyntaxState {
    /// Cria o estado e faz a análise inicial do documento.
    pub fn new(language: LanguageId, text: &str) -> Self {
        let mut parser = Parser::new();
        parser
            .set_language(&language.grammar())
            .expect("gramática embutida deve ser compatível com o Tree-sitter");

        let index = LineIndex::new(text);
        let mut state = Self {
            language,
            parser,
            tree: None,
            text: text.to_owned(),
            index,
            highlights: Vec::new(),
            update_count: 0,
        };
        state.reparse();
        state
    }

    pub fn language(&self) -> LanguageId {
        self.language
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    /// Quantidade de análises executadas. Usada por testes de desempenho para
    /// confirmar que a renderização não dispara novas análises.
    pub fn update_count(&self) -> u64 {
        self.update_count
    }

    pub fn highlights(&self) -> &[HighlightSpan] {
        &self.highlights
    }

    /// Retorna os trechos que cobrem um intervalo de caracteres.
    ///
    /// Os trechos são não sobrepostos e ordenados, então a busca é logarítmica
    /// e adequada para renderizar apenas as linhas visíveis.
    pub fn spans_for_range(&self, range: TextRange) -> &[HighlightSpan] {
        let start = self
            .highlights
            .partition_point(|span| span.range.end.0 <= range.start.0);
        let end = self
            .highlights
            .partition_point(|span| span.range.start.0 < range.end.0);

        if start >= end {
            &[]
        } else {
            &self.highlights[start..end]
        }
    }

    /// Aplica uma edição já conhecida, atualizando a árvore de forma
    /// incremental.
    ///
    /// `range` e os textos usam a mesma semântica do `AppliedEdit` do Core: o
    /// intervalo pertence ao estado anterior à edição.
    pub fn apply_edit(
        &mut self,
        range: TextRange,
        deleted_text: &str,
        inserted_text: &str,
    ) -> Result<(), SyntaxError> {
        if range.start.0 > range.end.0 || range.end.0 > self.index.char_len {
            return Err(SyntaxError::InvalidRange {
                start: range.start.0,
                end: range.end.0,
                len: self.index.char_len,
            });
        }

        let old_start_byte = self.index.char_to_byte(&self.text, range.start.0)?;
        let old_end_byte = self.index.char_to_byte(&self.text, range.end.0)?;

        if &self.text[old_start_byte..old_end_byte] != deleted_text {
            return Err(SyntaxError::EditMismatch);
        }

        let start_position = self.index.point(old_start_byte);
        let old_end_position = self.index.point(old_end_byte);

        let mut new_text =
            String::with_capacity(self.text.len() - deleted_text.len() + inserted_text.len());
        new_text.push_str(&self.text[..old_start_byte]);
        new_text.push_str(inserted_text);
        new_text.push_str(&self.text[old_end_byte..]);

        let new_index = LineIndex::new(&new_text);
        let new_end_byte = old_start_byte + inserted_text.len();
        let new_end_position = new_index.point(new_end_byte);

        let edit = InputEdit {
            start_byte: old_start_byte,
            old_end_byte,
            new_end_byte,
            start_position,
            old_end_position,
            new_end_position,
        };

        if let Some(tree) = self.tree.as_mut() {
            tree.edit(&edit);
        }

        self.text = new_text;
        self.index = new_index;
        self.reparse();

        Ok(())
    }

    /// Sincroniza o estado com um texto completo, calculando a menor edição
    /// possível entre o texto anterior e o novo.
    ///
    /// Usado quando a origem da mudança não fornece o intervalo editado, como
    /// em Undo/Redo. A árvore ainda é atualizada de forma incremental.
    pub fn resync(&mut self, new_text: &str) -> Result<(), SyntaxError> {
        if new_text == self.text {
            return Ok(());
        }

        let old_len = self.index.char_len;
        let new_len = new_text.chars().count();

        let prefix = self
            .text
            .chars()
            .zip(new_text.chars())
            .take_while(|(old, new)| old == new)
            .count();

        let max_suffix = old_len.min(new_len) - prefix;
        let suffix = self
            .text
            .chars()
            .rev()
            .zip(new_text.chars().rev())
            .take(max_suffix)
            .take_while(|(old, new)| old == new)
            .count();

        let old_end = old_len - suffix;
        let new_end = new_len - suffix;

        let old_start_byte = self.index.char_to_byte(&self.text, prefix)?;
        let old_end_byte = self.index.char_to_byte(&self.text, old_end)?;
        let new_start_byte = byte_index(new_text, prefix);
        let new_end_byte = byte_index(new_text, new_end);

        let deleted = self.text[old_start_byte..old_end_byte].to_owned();
        let inserted = new_text[new_start_byte..new_end_byte].to_owned();

        self.apply_edit(
            TextRange::new(CharOffset(prefix), CharOffset(old_end)),
            &deleted,
            &inserted,
        )
    }

    fn reparse(&mut self) {
        let old_tree = self.tree.take();
        let mut tree = self.parser.parse(&self.text, old_tree.as_ref());

        if tree.is_none() {
            // A análise incremental falhou; tenta uma análise completa antes de
            // desistir. O documento nunca é alterado por esse caminho.
            tree = self.parser.parse(&self.text, None);
        }

        self.tree = tree;
        self.update_count = self.update_count.saturating_add(1);

        self.highlights = match &self.tree {
            Some(tree) => compute_highlights(self.language, tree, &self.text, &self.index),
            None => Vec::new(),
        };
    }
}

impl fmt::Debug for SyntaxState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SyntaxState")
            .field("language", &self.language)
            .field("text_len", &self.text.len())
            .field("highlights", &self.highlights.len())
            .field("update_count", &self.update_count)
            .finish()
    }
}

/// Erro de sincronização entre o estado sintático e o documento.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyntaxError {
    InvalidRange {
        start: usize,
        end: usize,
        len: usize,
    },
    EditMismatch,
    InvalidByteOffset {
        offset: usize,
    },
}

impl fmt::Display for SyntaxError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SyntaxError::InvalidRange { start, end, len } => write!(
                formatter,
                "intervalo sintático inválido: início {start}, fim {end}, comprimento {len}"
            ),
            SyntaxError::EditMismatch => {
                write!(formatter, "o texto removido não corresponde ao documento")
            }
            SyntaxError::InvalidByteOffset { offset } => {
                write!(formatter, "offset de byte inválido: {offset}")
            }
        }
    }
}

impl std::error::Error for SyntaxError {}

struct LanguageRuntime {
    query: Query,
}

fn runtime(language: LanguageId) -> &'static LanguageRuntime {
    macro_rules! cached {
        ($name:ident, $language:expr) => {{
            static CACHE: OnceLock<LanguageRuntime> = OnceLock::new();
            CACHE.get_or_init(|| {
                let grammar = $language.grammar();
                let source = $language.highlight_query();
                let query = Query::new(&grammar, &source).expect(
                    "consulta de highlighting embutida deve ser compatível com a gramática",
                );
                LanguageRuntime { query }
            })
        }};
    }

    match language {
        LanguageId::Rust => cached!(RUST_RUNTIME, LanguageId::Rust),
        LanguageId::Python => cached!(PYTHON_RUNTIME, LanguageId::Python),
        LanguageId::JavaScript => cached!(JAVASCRIPT_RUNTIME, LanguageId::JavaScript),
        LanguageId::TypeScript => cached!(TYPESCRIPT_RUNTIME, LanguageId::TypeScript),
        LanguageId::Tsx => cached!(TSX_RUNTIME, LanguageId::Tsx),
    }
}

fn compute_highlights(
    language: LanguageId,
    tree: &Tree,
    text: &str,
    index: &LineIndex,
) -> Vec<HighlightSpan> {
    let runtime = runtime(language);
    let names = runtime.query.capture_names();
    let mut cursor = QueryCursor::new();
    let mut raw: Vec<RawSpan> = Vec::new();

    let mut matches = cursor.matches(&runtime.query, tree.root_node(), text.as_bytes());
    while let Some(matched) = matches.next() {
        for capture in matched.captures() {
            let Some(name) = names.get(capture.index as usize) else {
                continue;
            };
            let Some(kind) = capture_kind(name) else {
                continue;
            };

            let node = capture.node;
            let Ok(start) = index.byte_to_char(text, node.start_byte()) else {
                continue;
            };
            let Ok(mut end) = index.byte_to_char(text, node.end_byte()) else {
                continue;
            };

            // Em documentos CRLF, o Tree-sitter inclui o `\r` no fim de nós de
            // linha (comentários, por exemplo). O `\r` é invisível e não deve
            // fazer parte do trecho destacado.
            if end > start
                && node.end_byte() > 0
                && text.as_bytes().get(node.end_byte() - 1) == Some(&b'\r')
            {
                end -= 1;
            }

            if start >= end {
                continue;
            }

            raw.push(RawSpan { start, end, kind });
        }
    }

    for span in &mut raw {
        if span.kind == TokenKind::Variable
            && let Some(kind) = convention_kind(text, index, span.start, span.end)
        {
            span.kind = kind;
        }
    }

    // Prioridade mais alta primeiro; dentro da mesma prioridade, trechos mais
    // longos primeiro. Uma célula já pintada não é sobrescrita por uma captura
    // menos específica.
    raw.sort_by(|left, right| {
        right
            .kind
            .priority()
            .cmp(&left.kind.priority())
            .then(left.start.cmp(&right.start))
            .then(right.end.cmp(&left.end))
    });

    let mut cells: Vec<Option<TokenKind>> = vec![None; index.char_len];

    for span in raw {
        for cell in &mut cells[span.start..span.end] {
            if cell.is_none() {
                *cell = Some(span.kind);
            }
        }
    }

    run_length_encode(&cells)
}

struct RawSpan {
    start: usize,
    end: usize,
    kind: TokenKind,
}

fn capture_kind(name: &str) -> Option<TokenKind> {
    match name {
        "keyword" => Some(TokenKind::Keyword),
        "string" | "string.special" | "escape" => Some(TokenKind::String),
        "comment" | "comment.documentation" => Some(TokenKind::Comment),
        "number" => Some(TokenKind::Number),
        "function" | "function.builtin" | "function.macro" => Some(TokenKind::Function),
        "function.method" | "method" => Some(TokenKind::Method),
        "type" | "type.builtin" | "constructor" => Some(TokenKind::Type),
        "variable" | "variable.parameter" | "variable.builtin" | "property" => {
            Some(TokenKind::Variable)
        }
        "constant" | "constant.builtin" => Some(TokenKind::Constant),
        "operator" => Some(TokenKind::Operator),
        "attribute" | "label" => Some(TokenKind::Attribute),
        _ => None,
    }
}

/// Aproxima as convenções de nome das consultas oficiais sem predicados.
///
/// Nomes totalmente em maiúsculas são tratados como constantes; nomes que
/// começam com maiúscula, como tipos/constructores.
fn convention_kind(text: &str, index: &LineIndex, start: usize, end: usize) -> Option<TokenKind> {
    let start_byte = index.char_to_byte(text, start).ok()?;
    let end_byte = index.char_to_byte(text, end).ok()?;
    let slice = &text[start_byte..end_byte];

    let first = slice.chars().next()?;
    if !first.is_uppercase() {
        return None;
    }

    let mut count = 0usize;
    let mut all_caps = true;
    for character in slice.chars() {
        count += 1;
        if !(character.is_uppercase() || character.is_numeric() || character == '_') {
            all_caps = false;
            break;
        }
    }

    if all_caps && count > 1 {
        Some(TokenKind::Constant)
    } else {
        Some(TokenKind::Type)
    }
}

fn run_length_encode(cells: &[Option<TokenKind>]) -> Vec<HighlightSpan> {
    let mut spans = Vec::new();
    let mut position = 0;

    while position < cells.len() {
        let Some(kind) = cells[position] else {
            position += 1;
            continue;
        };

        let start = position;
        position += 1;
        while position < cells.len() && cells[position] == Some(kind) {
            position += 1;
        }

        spans.push(HighlightSpan {
            range: TextRange::new(CharOffset(start), CharOffset(position)),
            kind,
        });
    }

    spans
}

fn byte_index(text: &str, char_index: usize) -> usize {
    text.char_indices()
        .nth(char_index)
        .map(|(byte, _)| byte)
        .unwrap_or(text.len())
}

#[derive(Debug, Clone, Copy)]
struct LineStart {
    char_start: usize,
    byte_start: usize,
}

/// Índice de linhas para converter entre offsets de caracteres (Core) e
/// offsets de bytes/pontos (Tree-sitter).
#[derive(Debug, Clone)]
struct LineIndex {
    lines: Vec<LineStart>,
    char_len: usize,
}

impl LineIndex {
    fn new(text: &str) -> Self {
        let mut lines = vec![LineStart {
            char_start: 0,
            byte_start: 0,
        }];
        let mut chars = 0usize;

        for (byte, character) in text.char_indices() {
            chars += 1;
            if character == '\n' {
                lines.push(LineStart {
                    char_start: chars,
                    byte_start: byte + 1,
                });
            }
        }

        Self {
            lines,
            char_len: chars,
        }
    }

    fn line_for_char(&self, offset: usize) -> usize {
        self.lines
            .partition_point(|line| line.char_start <= offset)
            .saturating_sub(1)
    }

    fn line_for_byte(&self, offset: usize) -> usize {
        self.lines
            .partition_point(|line| line.byte_start <= offset)
            .saturating_sub(1)
    }

    fn char_to_byte(&self, text: &str, offset: usize) -> Result<usize, SyntaxError> {
        if offset > self.char_len {
            return Err(SyntaxError::InvalidRange {
                start: offset,
                end: offset,
                len: self.char_len,
            });
        }

        let line = &self.lines[self.line_for_char(offset)];
        let mut byte = line.byte_start;
        let mut remaining = offset - line.char_start;

        for character in text[line.byte_start..].chars() {
            if remaining == 0 {
                break;
            }
            byte += character.len_utf8();
            remaining -= 1;
        }

        Ok(byte)
    }

    fn byte_to_char(&self, text: &str, offset: usize) -> Result<usize, SyntaxError> {
        if offset > text.len() || !text.is_char_boundary(offset) {
            return Err(SyntaxError::InvalidByteOffset { offset });
        }

        let line = &self.lines[self.line_for_byte(offset)];
        Ok(line.char_start + text[line.byte_start..offset].chars().count())
    }

    fn point(&self, byte_offset: usize) -> Point {
        let line = self.line_for_byte(byte_offset);
        let start = self.lines[line];
        Point::new(line, byte_offset - start.byte_start)
    }
}

/// Constrói a linguagem a partir do caminho, se reconhecida.
pub fn language_for_path(path: &std::path::Path) -> Option<LanguageId> {
    LanguageId::from_path(path)
}

/// Verifica se uma linguagem pode ser usada, compilando sua consulta.
///
/// Útil para testes e para validar contribuições de novas gramáticas.
pub fn validate_language(language: LanguageId) -> Result<(), String> {
    let grammar: Language = language.grammar();
    Query::new(&grammar, &language.highlight_query())
        .map(|_| ())
        .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_of(state: &SyntaxState, span: HighlightSpan) -> &str {
        let text = state.text();
        let start = text
            .char_indices()
            .nth(span.range.start.0)
            .map(|(byte, _)| byte)
            .unwrap_or(text.len());
        let end = text
            .char_indices()
            .nth(span.range.end.0)
            .map(|(byte, _)| byte)
            .unwrap_or(text.len());
        &text[start..end]
    }

    fn highlighted_text(state: &SyntaxState, kind: TokenKind) -> Vec<&str> {
        state
            .highlights()
            .iter()
            .filter(|span| span.kind == kind)
            .map(|span| text_of(state, *span))
            .collect()
    }

    #[test]
    fn all_initial_languages_compile_their_queries() {
        for language in [
            LanguageId::Rust,
            LanguageId::Python,
            LanguageId::JavaScript,
            LanguageId::TypeScript,
            LanguageId::Tsx,
        ] {
            validate_language(language).unwrap_or_else(|error| panic!("{language:?}: {error}"));
        }
    }

    #[test]
    fn highlights_rust_keywords_strings_comments_and_numbers() {
        let source = "// comentário\nfn main() { let x: u32 = 42; let s = \"texto\"; }";
        let state = SyntaxState::new(LanguageId::Rust, source);

        let keywords = highlighted_text(&state, TokenKind::Keyword);
        assert!(keywords.contains(&"fn"));
        assert!(keywords.contains(&"let"));

        let comments = highlighted_text(&state, TokenKind::Comment);
        assert!(comments.iter().any(|text| text.contains("comentário")));

        let strings = highlighted_text(&state, TokenKind::String);
        assert!(strings.iter().any(|text| text.contains("texto")));

        let numbers = highlighted_text(&state, TokenKind::Number);
        assert!(numbers.contains(&"42"));
    }

    #[test]
    fn highlights_rust_functions_types_and_constants() {
        let source = "struct Ponto { x: i32 }\nconst LIMITE: i32 = 10;\nfn soma(a: i32) -> i32 { a + 1 }\nfn main() { soma(1); }";
        let state = SyntaxState::new(LanguageId::Rust, source);

        let types = highlighted_text(&state, TokenKind::Type);
        assert!(types.contains(&"Ponto"));
        assert!(types.contains(&"i32"));

        let functions = highlighted_text(&state, TokenKind::Function);
        assert!(functions.contains(&"soma"));
        assert!(functions.contains(&"main"));

        let constants = highlighted_text(&state, TokenKind::Constant);
        assert!(constants.contains(&"LIMITE"));
    }

    #[test]
    fn highlights_python_constructs() {
        let source = "# comentário\nclass Pessoa:\n    def falar(self, nome: str):\n        total = 42\n        return f\"Olá {nome}\"\n";
        let state = SyntaxState::new(LanguageId::Python, source);

        let keywords = highlighted_text(&state, TokenKind::Keyword);
        assert!(keywords.contains(&"class"));
        assert!(keywords.contains(&"def"));
        assert!(keywords.contains(&"return"));

        let types = highlighted_text(&state, TokenKind::Type);
        assert!(types.contains(&"Pessoa"));

        let functions = highlighted_text(&state, TokenKind::Function);
        assert!(functions.contains(&"falar"));

        let comments = highlighted_text(&state, TokenKind::Comment);
        assert!(comments.iter().any(|text| text.contains("comentário")));

        let numbers = highlighted_text(&state, TokenKind::Number);
        assert!(numbers.contains(&"42"));
    }

    #[test]
    fn highlights_javascript_constructs() {
        let source = "// comentário\nfunction somar(a, b) { return a + 42; }\nclass Conta {}\nconst nome = \"Nexus\";\nsomar(1, 2);\n";
        let state = SyntaxState::new(LanguageId::JavaScript, source);

        let keywords = highlighted_text(&state, TokenKind::Keyword);
        assert!(keywords.contains(&"function"));
        assert!(keywords.contains(&"return"));
        assert!(keywords.contains(&"class"));

        let functions = highlighted_text(&state, TokenKind::Function);
        assert!(functions.contains(&"somar"));

        let types = highlighted_text(&state, TokenKind::Type);
        assert!(types.contains(&"Conta"));

        let strings = highlighted_text(&state, TokenKind::String);
        assert!(strings.iter().any(|text| text.contains("Nexus")));
    }

    #[test]
    fn highlights_typescript_constructs() {
        let source = "interface Usuario { nome: string; }\ntype Id = number;\nfunction buscar(id: Id): Usuario { return { nome: \"x\" }; }\n";
        let state = SyntaxState::new(LanguageId::TypeScript, source);

        let keywords = highlighted_text(&state, TokenKind::Keyword);
        assert!(keywords.contains(&"interface"));
        assert!(keywords.contains(&"type"));

        let types = highlighted_text(&state, TokenKind::Type);
        assert!(types.contains(&"Usuario"));
        assert!(types.contains(&"Id"));

        let functions = highlighted_text(&state, TokenKind::Function);
        assert!(functions.contains(&"buscar"));
    }

    #[test]
    fn incomplete_code_does_not_panic_and_keeps_known_tokens() {
        let state = SyntaxState::new(LanguageId::Rust, "fn main( { let x = ");

        let keywords = highlighted_text(&state, TokenKind::Keyword);
        assert!(keywords.contains(&"fn"));
        assert!(keywords.contains(&"let"));
    }

    #[test]
    fn syntactically_invalid_code_does_not_panic() {
        let state = SyntaxState::new(LanguageId::Rust, "let = ; ) } fn");

        assert!(!state.highlights().is_empty());
    }

    #[test]
    fn insertion_updates_highlights_incrementally() {
        let mut state = SyntaxState::new(LanguageId::Rust, "fn main() {}");
        let before = state.update_count();

        state
            .apply_edit(
                TextRange::new(CharOffset(11), CharOffset(11)),
                "",
                "\n// novo comentário",
            )
            .unwrap();

        assert_eq!(state.update_count(), before + 1);
        assert!(
            highlighted_text(&state, TokenKind::Comment)
                .iter()
                .any(|text| text.contains("novo comentário"))
        );
    }

    #[test]
    fn deletion_updates_highlights_incrementally() {
        let mut state = SyntaxState::new(LanguageId::Rust, "fn main() { let x = 42; }");
        assert!(highlighted_text(&state, TokenKind::Number).contains(&"42"));

        state
            .apply_edit(TextRange::new(CharOffset(20), CharOffset(22)), "42", "")
            .unwrap();

        assert!(!highlighted_text(&state, TokenKind::Number).contains(&"42"));
    }

    #[test]
    fn undo_and_redo_resync_highlights() {
        let original = "fn main() { let x = 1; }";
        let edited = "fn main() { let x = 2; }";
        let mut state = SyntaxState::new(LanguageId::Rust, edited);

        assert!(highlighted_text(&state, TokenKind::Number).contains(&"2"));

        state.resync(original).unwrap();
        assert!(highlighted_text(&state, TokenKind::Number).contains(&"1"));

        state.resync(edited).unwrap();
        assert!(highlighted_text(&state, TokenKind::Number).contains(&"2"));
    }

    #[test]
    fn multiple_edits_remain_consistent() {
        let mut state = SyntaxState::new(LanguageId::Rust, "");
        let mut expected = String::new();

        for text in ["fn ", "main", "() {", "\n    let x = 1;", "\n}"] {
            let offset = expected.chars().count();
            state
                .apply_edit(
                    TextRange::new(CharOffset(offset), CharOffset(offset)),
                    "",
                    text,
                )
                .unwrap();
            expected.push_str(text);
        }

        assert_eq!(state.text(), expected);
        assert!(highlighted_text(&state, TokenKind::Keyword).contains(&"fn"));
        assert!(highlighted_text(&state, TokenKind::Keyword).contains(&"let"));
        assert!(highlighted_text(&state, TokenKind::Function).contains(&"main"));
        assert!(highlighted_text(&state, TokenKind::Number).contains(&"1"));
    }

    #[test]
    fn unicode_accents_keep_char_offsets() {
        let source = "// café\nfn main() { let nome = \"ação\"; }";
        let state = SyntaxState::new(LanguageId::Rust, source);

        let comment = state
            .highlights()
            .iter()
            .find(|span| span.kind == TokenKind::Comment)
            .copied()
            .unwrap();

        assert_eq!(text_of(&state, comment), "// café");
        assert_eq!(comment.range.start.0, 0);
        assert_eq!(comment.range.end.0, "// café".chars().count());

        let strings = highlighted_text(&state, TokenKind::String);
        assert!(strings.iter().any(|text| text.contains("ação")));
    }

    #[test]
    fn double_width_and_emoji_use_char_offsets() {
        let source = "// 界 😀\nfn main() {}";
        let state = SyntaxState::new(LanguageId::Rust, source);

        let comment = state
            .highlights()
            .iter()
            .find(|span| span.kind == TokenKind::Comment)
            .copied()
            .unwrap();

        assert_eq!(text_of(&state, comment), "// 界 😀");
        assert_eq!(comment.range.end.0, "// 界 😀".chars().count());
    }

    #[test]
    fn crlf_documents_keep_char_offsets() {
        let source = "// linha\r\nfn main() {\r\n    let x = 1;\r\n}\r\n";
        let state = SyntaxState::new(LanguageId::Rust, source);

        let comment = state
            .highlights()
            .iter()
            .find(|span| span.kind == TokenKind::Comment)
            .copied()
            .unwrap();
        assert_eq!(text_of(&state, comment), "// linha");
        assert_eq!(comment.range.end.0, "// linha".chars().count());

        assert!(highlighted_text(&state, TokenKind::Number).contains(&"1"));
    }

    #[test]
    fn large_document_incremental_edits_remain_practical() {
        use std::time::{Duration, Instant};

        let source = (0..2_000)
            .map(|line| format!("fn funcao_{line}() {{ let valor = {line}; }}\n"))
            .collect::<String>();
        let mut state = SyntaxState::new(LanguageId::Rust, &source);
        let initial_updates = state.update_count();

        let started = Instant::now();
        for _ in 0..20 {
            let offset = state.text().chars().count();
            state
                .apply_edit(
                    TextRange::new(CharOffset(offset), CharOffset(offset)),
                    "",
                    "// fim\n",
                )
                .unwrap();
        }
        let elapsed = started.elapsed();

        assert_eq!(state.update_count(), initial_updates + 20);
        assert!(state.text().ends_with("// fim\n"));
        assert!(
            elapsed < Duration::from_secs(10),
            "20 edições incrementais demoraram {elapsed:?}"
        );

        let query_started = Instant::now();
        let visible = state.spans_for_range(TextRange::new(CharOffset(0), CharOffset(200)));
        assert!(!visible.is_empty());
        assert!(
            query_started.elapsed() < Duration::from_millis(100),
            "consulta de trechos visíveis demorou {:?}",
            query_started.elapsed()
        );
    }

    #[test]
    fn unicode_and_crlf_edits_keep_offsets_consistent() {
        let original = "// café\r\nfn main() {}\r\n";
        let mut state = SyntaxState::new(LanguageId::Rust, original);

        // Insere um caractere de largura dupla e um espaço após `café`.
        state
            .apply_edit(TextRange::new(CharOffset(7), CharOffset(7)), "", " 界")
            .unwrap();
        assert_eq!(state.text(), "// café 界\r\nfn main() {}\r\n");
        assert_eq!(
            highlighted_text(&state, TokenKind::Comment),
            vec!["// café 界"]
        );

        // Remove novamente usando offsets de caracteres, não bytes.
        state
            .apply_edit(TextRange::new(CharOffset(7), CharOffset(9)), " 界", "")
            .unwrap();
        assert_eq!(state.text(), original);
        assert_eq!(
            highlighted_text(&state, TokenKind::Comment),
            vec!["// café"]
        );

        // Insere uma quebra CRLF completa e confirma que o comentário termina
        // antes do `\r`, sem deslocar os tokens seguintes.
        state
            .apply_edit(TextRange::new(CharOffset(7), CharOffset(7)), "", "\r\n")
            .unwrap();
        assert_eq!(state.text(), "// café\r\n\r\nfn main() {}\r\n");
        assert_eq!(
            highlighted_text(&state, TokenKind::Comment),
            vec!["// café"]
        );
        assert!(highlighted_text(&state, TokenKind::Keyword).contains(&"fn"));

        state.resync(original).unwrap();
        assert_eq!(state.text(), original);
        assert_eq!(
            highlighted_text(&state, TokenKind::Comment),
            vec!["// café"]
        );
    }

    #[test]
    fn spans_for_range_returns_only_visible_spans() {
        let state = SyntaxState::new(LanguageId::Rust, "fn a() {}\nfn b() {}\nfn c() {}\n");

        let line_two_start = "fn a() {}\n".chars().count();
        let line_two_end = line_two_start + "fn b() {}".chars().count();
        let spans = state.spans_for_range(TextRange::new(
            CharOffset(line_two_start),
            CharOffset(line_two_end),
        ));

        assert!(spans.iter().any(|span| span.kind == TokenKind::Function));
        assert!(spans.iter().all(|span| {
            span.range.end.0 > line_two_start && span.range.start.0 < line_two_end
        }));
    }

    #[test]
    fn resync_with_identical_text_does_not_reparse() {
        let mut state = SyntaxState::new(LanguageId::Rust, "fn main() {}");
        let count = state.update_count();

        state.resync("fn main() {}").unwrap();

        assert_eq!(state.update_count(), count);
    }

    #[test]
    fn edit_mismatch_is_rejected_without_changing_state() {
        let mut state = SyntaxState::new(LanguageId::Rust, "fn main() {}");
        let before = state.text().to_owned();

        let error = state
            .apply_edit(TextRange::new(CharOffset(0), CharOffset(2)), "xx", "yy")
            .unwrap_err();

        assert_eq!(error, SyntaxError::EditMismatch);
        assert_eq!(state.text(), before);
    }
}
