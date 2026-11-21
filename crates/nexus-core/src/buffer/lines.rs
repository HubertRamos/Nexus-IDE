//! Operações de edição baseadas em linhas.
//!
//! As operações são construídas como uma única transação, preservando a
//! arquitetura de histórico, revisões e recuperação do [`Buffer`]. Esta camada
//! não conhece cursor, seleção visual nem atalhos: recebe intervalos de linhas
//! já resolvidos e devolve a transação aplicada.

use super::edit::{BufferError, Edit, TextRange};
use super::position::CharOffset;
use super::{Buffer, Transaction};

/// Intervalo inclusivo de linhas.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineRange {
    pub first: usize,
    pub last: usize,
}

impl LineRange {
    pub fn new(first: usize, last: usize) -> Self {
        Self { first, last }
    }

    pub fn len(self) -> usize {
        self.last.saturating_sub(self.first) + 1
    }
}

/// Direção da movimentação de linhas.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoveDirection {
    Up,
    Down,
}

/// Resultado de uma movimentação de linhas.
///
/// A transação mantém o conteúdo e as quebras de linha originais. O intervalo
/// de origem permite que a TUI ajuste cursor e seleção sem duplicar as regras
/// de reordenação.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MoveOutcome {
    pub transaction: Transaction,
    pub block: LineRange,
    pub direction: MoveDirection,
}

impl MoveOutcome {
    /// Mapeia o índice de uma linha de antes da movimentação para depois dela.
    ///
    /// Linhas fora da região movida permanecem no mesmo índice.
    pub fn map_line(&self, line: usize) -> usize {
        match self.direction {
            MoveDirection::Up if line + 1 == self.block.first => self.block.last,
            MoveDirection::Up if line >= self.block.first && line <= self.block.last => line - 1,
            MoveDirection::Down if line == self.block.last + 1 => self.block.first,
            MoveDirection::Down if line >= self.block.first && line <= self.block.last => line + 1,
            _ => line,
        }
    }
}

impl Buffer {
    /// Intervalo de linhas coberto por uma seleção semiaberta `[start, end)`.
    ///
    /// Quando a seleção termina exatamente no início de uma linha, essa linha
    /// não é incluída. Isso evita indentar, duplicar ou mover uma linha que o
    /// usuário não selecionou de fato.
    pub fn selection_line_range(&self, range: TextRange) -> Result<LineRange, BufferError> {
        self.ensure_consistent()?;

        if range.start > range.end {
            return Err(BufferError::InvalidRange {
                start: range.start.0,
                end: range.end.0,
            });
        }

        let first = self.line_of_char(range.start)?;
        let mut last = self.line_of_char(range.end)?;

        if range.start < range.end && self.line_start(last)? == range.end {
            last = last.saturating_sub(1);
        }

        Ok(LineRange::new(first, last.max(first)))
    }

    /// Intervalo da linha que contém o deslocamento informado.
    pub fn line_range_at(&self, offset: CharOffset) -> Result<LineRange, BufferError> {
        let line = self.line_of_char(offset)?;
        Ok(LineRange::new(line, line))
    }

    /// Adiciona `indent` ao início de cada linha do intervalo.
    pub fn indent_lines(
        &mut self,
        lines: LineRange,
        indent: &str,
    ) -> Result<Option<Transaction>, BufferError> {
        self.validate_line_range(lines)?;

        if indent.is_empty() || self.is_empty()? {
            return Ok(None);
        }

        let mut edits = Vec::with_capacity(lines.len());

        // De baixo para cima para que os deslocamentos já aplicados não
        // desloquem as linhas seguintes ainda não editadas.
        for line in (lines.first..=lines.last).rev() {
            let start = self.line_start(line)?;
            edits.push(Edit::new(
                TextRange::new(start, start),
                indent.to_owned(),
            ));
        }

        self.apply_transaction(edits)
    }

    /// Remove um nível de indentação do início de cada linha do intervalo.
    ///
    /// Uma tabulação inicial é removida inteira. Caso a linha comece com
    /// espaços, são removidos no máximo `max_spaces`, parando no primeiro
    /// caractere que não seja espaço. Linhas sem indentação permanecem
    /// intactas.
    pub fn outdent_lines(
        &mut self,
        lines: LineRange,
        max_spaces: usize,
    ) -> Result<Option<Transaction>, BufferError> {
        self.validate_line_range(lines)?;

        if max_spaces == 0 {
            return Ok(None);
        }

        let mut edits = Vec::new();

        for line in (lines.first..=lines.last).rev() {
            let start = self.line_start(line)?;
            let end = self.line_end(line)?;

            if start >= end {
                continue;
            }

            let first = self.char_at(start)?;

            if first == Some('\t') {
                edits.push(Edit::new(
                    TextRange::new(start, CharOffset(start.0 + 1)),
                    String::new(),
                ));
                continue;
            }

            let mut removed = 0;

            while removed < max_spaces && start.0 + removed < end.0 {
                match self.char_at(CharOffset(start.0 + removed))? {
                    Some(' ') => removed += 1,
                    _ => break,
                }
            }

            if removed > 0 {
                edits.push(Edit::new(
                    TextRange::new(start, CharOffset(start.0 + removed)),
                    String::new(),
                ));
            }
        }

        self.apply_transaction(edits)
    }

    /// Duplica o bloco de linhas, preservando a indentação e as quebras.
    pub fn duplicate_lines(&mut self, lines: LineRange) -> Result<Option<Transaction>, BufferError> {
        self.validate_line_range(lines)?;

        let len = self.len_chars()?;

        // Um documento vazio não possui conteúdo a duplicar.
        if len == 0 {
            return Ok(None);
        }

        let line_count = self.line_count()?;
        let first_start = self.line_start(lines.first)?;

        if lines.last + 1 < line_count {
            let after_last = self.line_start(lines.last + 1)?;
            let block = self.slice(TextRange::new(first_start, after_last))?;

            return self.apply_transaction(vec![Edit::new(
                TextRange::new(after_last, after_last),
                block,
            )]);
        }

        // Bloco no fim do documento: a última linha pode não ter quebra.
        let block = self.slice(TextRange::new(first_start, CharOffset(len)))?;
        let ending = self.preferred_line_ending(lines.last)?;
        let insert_at = CharOffset(len);
        let text = format!("{ending}{block}");

        self.apply_transaction(vec![Edit::new(
            TextRange::new(insert_at, insert_at),
            text,
        )])
    }

    /// Move o bloco de linhas uma posição para cima ou para baixo.
    ///
    /// A linha vazia final criada por um arquivo terminado em quebra de linha
    /// não é tratada como conteúdo movível. As quebras de linha originais são
    /// preservadas por posição, inclusive `\r\n` e um `EOF` sem quebra final.
    pub fn move_lines(
        &mut self,
        lines: LineRange,
        direction: MoveDirection,
    ) -> Result<Option<MoveOutcome>, BufferError> {
        self.validate_line_range(lines)?;

        let len = self.len_chars()?;
        let line_count = self.line_count()?;
        let phantom_tail =
            line_count > 1 && self.line_start(line_count - 1)? == CharOffset(len);
        let movable_last = if phantom_tail {
            line_count - 2
        } else {
            line_count - 1
        };

        if lines.first > movable_last || lines.last > movable_last {
            return Ok(None);
        }

        match direction {
            MoveDirection::Up if lines.first == 0 => return Ok(None),
            MoveDirection::Down if lines.last >= movable_last => return Ok(None),
            _ => {}
        }

        let region_first = match direction {
            MoveDirection::Up => lines.first - 1,
            MoveDirection::Down => lines.first,
        };
        let region_last = match direction {
            MoveDirection::Up => lines.last,
            MoveDirection::Down => lines.last + 1,
        };

        let mut contents = Vec::with_capacity(region_last - region_first + 1);
        let mut endings = Vec::with_capacity(contents.capacity());

        for line in region_first..=region_last {
            let start = self.line_start(line)?;
            let end = self.line_end(line)?;
            contents.push(self.slice(TextRange::new(start, end))?);

            let ending = if line + 1 < line_count {
                let next = self.line_start(line + 1)?;
                self.slice(TextRange::new(end, next))?
            } else {
                String::new()
            };
            endings.push(ending);
        }

        let mut order: Vec<usize> = (0..contents.len()).collect();

        match direction {
            MoveDirection::Up => order.rotate_left(1),
            MoveDirection::Down => order.rotate_right(1),
        }

        let mut replacement = String::new();

        for (position, original) in order.into_iter().enumerate() {
            replacement.push_str(&contents[original]);
            replacement.push_str(&endings[position]);
        }

        let region_start = self.line_start(region_first)?;
        let region_end = if region_last + 1 < line_count {
            self.line_start(region_last + 1)?
        } else {
            CharOffset(len)
        };

        let transaction = self.apply_transaction(vec![Edit::new(
            TextRange::new(region_start, region_end),
            replacement,
        )])?;

        Ok(transaction.map(|transaction| MoveOutcome {
            transaction,
            block: lines,
            direction,
        }))
    }

    fn validate_line_range(&self, lines: LineRange) -> Result<(), BufferError> {
        self.ensure_consistent()?;

        let count = self.line_count()?;

        if lines.first > lines.last || lines.last >= count {
            return Err(BufferError::OutOfBounds {
                offset: lines.last,
                len: count,
            });
        }

        Ok(())
    }

    /// Quebra de linha mais próxima antes de `before_line`, com fallback para
    /// a primeira quebra encontrada no documento ou `\n`.
    fn preferred_line_ending(&self, before_line: usize) -> Result<String, BufferError> {
        let count = self.line_count()?;
        let upper = before_line.min(count.saturating_sub(1));

        for line in (0..upper).rev() {
            let end = self.line_end(line)?;
            let next = self.line_start(line + 1)?;

            if next > end {
                return self.slice(TextRange::new(end, next));
            }
        }

        let text = self.text()?;

        Ok(if text.contains("\r\n") {
            "\r\n".to_owned()
        } else {
            "\n".to_owned()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buffer(text: &str) -> Buffer {
        Buffer::from_text(text)
    }

    fn range(_buffer: &Buffer, first_line: usize, last_line: usize) -> LineRange {
        LineRange::new(first_line, last_line)
    }

    #[test]
    fn selection_ending_at_next_line_start_excludes_that_line() {
        let buffer = buffer("alpha\nbeta\ngamma\n");

        assert_eq!(
            buffer
                .selection_line_range(TextRange::new(CharOffset(0), CharOffset(6)))
                .unwrap(),
            LineRange::new(0, 0)
        );
        assert_eq!(
            buffer
                .selection_line_range(TextRange::new(CharOffset(0), CharOffset(10)))
                .unwrap(),
            LineRange::new(0, 1)
        );
        assert_eq!(
            buffer
                .selection_line_range(TextRange::new(CharOffset(2), CharOffset(9)))
                .unwrap(),
            LineRange::new(0, 1)
        );
    }

    #[test]
    fn selection_ending_at_document_end_with_trailing_newline_excludes_phantom_line() {
        let buffer = buffer("alpha\nbeta\n");

        assert_eq!(
            buffer
                .selection_line_range(TextRange::new(CharOffset(0), CharOffset(11)))
                .unwrap(),
            LineRange::new(0, 1)
        );
    }

    #[test]
    fn selection_inside_a_single_line_stays_on_that_line() {
        let buffer = buffer("alpha\nbeta");

        assert_eq!(
            buffer
                .selection_line_range(TextRange::new(CharOffset(1), CharOffset(3)))
                .unwrap(),
            LineRange::new(0, 0)
        );
        assert_eq!(
            buffer
                .selection_line_range(TextRange::new(CharOffset(7), CharOffset(10)))
                .unwrap(),
            LineRange::new(1, 1)
        );
    }

    #[test]
    fn indent_adds_one_tab_per_selected_line_as_a_single_transaction() {
        let mut buffer = buffer("alpha\nbeta\ngamma\ndelta");

        let transaction = buffer
            .indent_lines(range(&buffer, 1, 2), "\t")
            .unwrap()
            .unwrap();

        assert_eq!(transaction.len(), 2);
        assert_eq!(buffer.text().unwrap(), "alpha\n\tbeta\n\tgamma\ndelta");
        assert!(buffer.undo().unwrap());
        assert_eq!(buffer.text().unwrap(), "alpha\nbeta\ngamma\ndelta");
        assert!(buffer.redo().unwrap());
        assert_eq!(buffer.text().unwrap(), "alpha\n\tbeta\n\tgamma\ndelta");
    }

    #[test]
    fn indent_handles_empty_lines_and_empty_document() {
        let mut document = buffer("alpha\n\nomega");
        document.indent_lines(range(&document, 1, 1), "\t").unwrap();
        assert_eq!(document.text().unwrap(), "alpha\n\t\nomega");

        let mut empty = buffer("");
        assert!(empty.indent_lines(range(&empty, 0, 0), "\t").unwrap().is_none());
        assert_eq!(empty.text().unwrap(), "");
        assert!(!empty.can_undo());
    }

    #[test]
    fn indent_preserves_content_outside_the_selected_lines() {
        let mut buffer = buffer("keep\nfirst\nsecond\nkeep too");
        buffer.indent_lines(range(&buffer, 1, 2), "  ").unwrap();

        assert_eq!(
            buffer.text().unwrap(),
            "keep\n  first\n  second\nkeep too"
        );
    }

    #[test]
    fn outdent_removes_tabs_and_at_most_four_spaces() {
        let mut buffer = buffer("\talpha\n    beta\n  gamma\nnone");

        let transaction = buffer
            .outdent_lines(range(&buffer, 0, 3), 4)
            .unwrap()
            .unwrap();

        assert_eq!(transaction.len(), 3);
        assert_eq!(buffer.text().unwrap(), "alpha\nbeta\ngamma\nnone");
        assert!(buffer.undo().unwrap());
        assert_eq!(buffer.text().unwrap(), "\talpha\n    beta\n  gamma\nnone");
    }

    #[test]
    fn outdent_does_not_remove_non_indentation_content() {
        let mut buffer = buffer("    \n\t\tcode\n  x");

        buffer.outdent_lines(range(&buffer, 0, 2), 4).unwrap();

        assert_eq!(buffer.text().unwrap(), "\n\tcode\nx");
    }

    #[test]
    fn outdent_without_indentation_does_not_create_history() {
        let mut buffer = buffer("alpha\nbeta");

        assert!(buffer
            .outdent_lines(range(&buffer, 0, 1), 4)
            .unwrap()
            .is_none());
        assert_eq!(buffer.text().unwrap(), "alpha\nbeta");
        assert!(!buffer.can_undo());
    }

    #[test]
    fn duplicate_middle_line_preserves_content_and_line_endings() {
        let mut buffer = buffer("alpha\nbeta\ngamma");

        let transaction = buffer
            .duplicate_lines(range(&buffer, 1, 1))
            .unwrap()
            .unwrap();

        assert_eq!(transaction.len(), 1);
        assert_eq!(buffer.text().unwrap(), "alpha\nbeta\nbeta\ngamma");
        assert!(buffer.undo().unwrap());
        assert_eq!(buffer.text().unwrap(), "alpha\nbeta\ngamma");
    }

    #[test]
    fn duplicate_last_line_adds_the_missing_separator() {
        let mut buffer = buffer("alpha\nbeta");
        buffer.duplicate_lines(range(&buffer, 1, 1)).unwrap();

        assert_eq!(buffer.text().unwrap(), "alpha\nbeta\nbeta");
    }

    #[test]
    fn duplicate_first_line_and_crlf_document() {
        let mut buffer = buffer("alpha\r\nbeta\r\n");

        buffer.duplicate_lines(range(&buffer, 0, 0)).unwrap();
        assert_eq!(buffer.text().unwrap(), "alpha\r\nalpha\r\nbeta\r\n");

        buffer.duplicate_lines(range(&buffer, 3, 3)).unwrap();
        assert_eq!(
            buffer.text().unwrap(),
            "alpha\r\nalpha\r\nbeta\r\n\r\n"
        );
    }

    #[test]
    fn duplicate_block_selection_copies_all_lines_in_order() {
        let mut buffer = buffer("zero\none\ntwo\nthree");
        buffer.duplicate_lines(range(&buffer, 1, 2)).unwrap();

        assert_eq!(buffer.text().unwrap(), "zero\none\ntwo\none\ntwo\nthree");
    }

    #[test]
    fn duplicate_empty_document_is_a_no_op() {
        let mut buffer = buffer("");

        assert!(buffer.duplicate_lines(range(&buffer, 0, 0)).unwrap().is_none());
        assert_eq!(buffer.text().unwrap(), "");
        assert!(!buffer.can_undo());
    }

    #[test]
    fn move_line_up_and_down_preserves_text_and_history() {
        let mut buffer = buffer("alpha\nbeta\ngamma");

        let outcome = buffer
            .move_lines(range(&buffer, 1, 1), MoveDirection::Up)
            .unwrap()
            .unwrap();
        assert_eq!(outcome.map_line(1), 0);
        assert_eq!(buffer.text().unwrap(), "beta\nalpha\ngamma");

        assert!(buffer.undo().unwrap());
        assert_eq!(buffer.text().unwrap(), "alpha\nbeta\ngamma");

        assert!(buffer.redo().unwrap());
        assert_eq!(buffer.text().unwrap(), "beta\nalpha\ngamma");

        assert!(buffer.undo().unwrap());

        let outcome = buffer
            .move_lines(range(&buffer, 0, 0), MoveDirection::Down)
            .unwrap()
            .unwrap();
        assert_eq!(outcome.map_line(0), 1);
        assert_eq!(buffer.text().unwrap(), "beta\nalpha\ngamma");

        assert!(buffer.undo().unwrap());
        assert_eq!(buffer.text().unwrap(), "alpha\nbeta\ngamma");
    }

    #[test]
    fn move_block_up_and_down_keeps_relative_order() {
        let mut buffer = buffer("zero\none\ntwo\nthree\nfour");

        buffer
            .move_lines(range(&buffer, 1, 2), MoveDirection::Up)
            .unwrap();
        assert_eq!(buffer.text().unwrap(), "one\ntwo\nzero\nthree\nfour");

        buffer
            .move_lines(range(&buffer, 0, 1), MoveDirection::Down)
            .unwrap();
        assert_eq!(buffer.text().unwrap(), "zero\none\ntwo\nthree\nfour");
    }

    #[test]
    fn move_respects_document_boundaries() {
        let mut buffer = buffer("alpha\nbeta");

        assert!(buffer
            .move_lines(range(&buffer, 0, 0), MoveDirection::Up)
            .unwrap()
            .is_none());
        assert!(buffer
            .move_lines(range(&buffer, 1, 1), MoveDirection::Down)
            .unwrap()
            .is_none());
        assert_eq!(buffer.text().unwrap(), "alpha\nbeta");
        assert!(!buffer.can_undo());
    }

    #[test]
    fn move_does_not_move_into_the_phantom_trailing_line() {
        let mut buffer = buffer("alpha\nbeta\n");

        assert!(buffer
            .move_lines(range(&buffer, 1, 1), MoveDirection::Down)
            .unwrap()
            .is_none());
        assert_eq!(buffer.text().unwrap(), "alpha\nbeta\n");
    }

    #[test]
    fn move_preserves_crlf_and_missing_final_newline() {
        let mut buffer = buffer("alpha\r\nbeta\r\ngamma");

        buffer
            .move_lines(range(&buffer, 2, 2), MoveDirection::Up)
            .unwrap();
        assert_eq!(buffer.text().unwrap(), "alpha\r\ngamma\r\nbeta");

        buffer
            .move_lines(range(&buffer, 1, 1), MoveDirection::Down)
            .unwrap();
        assert_eq!(buffer.text().unwrap(), "alpha\r\nbeta\r\ngamma");
    }

    #[test]
    fn move_of_last_line_without_trailing_newline_keeps_eof_without_newline() {
        let mut buffer = buffer("alpha\nbeta");

        buffer
            .move_lines(range(&buffer, 1, 1), MoveDirection::Up)
            .unwrap();

        assert_eq!(buffer.text().unwrap(), "beta\nalpha");
    }
}
