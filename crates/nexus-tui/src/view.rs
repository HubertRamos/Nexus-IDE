use nexus_core::buffer::Buffer;
use nexus_core::buffer::edit::BufferError;
use nexus_core::buffer::edit::TextRange;
use nexus_core::buffer::position::CharOffset;
use unicode_width::UnicodeWidthChar;

/// Largura de uma tabulação em células de terminal.
pub const TAB_WIDTH: usize = 4;

pub const HEADER_HEIGHT: u16 = 1;
pub const STATUS_HEIGHT: u16 = 1;
const MIN_GUTTER_DIGITS: usize = 2;

/// Largura visual de um caractere a partir de uma coluna.
///
/// Tabulações avançam até o próximo tab stop. Caracteres de controle têm
/// largura zero; caracteres de largura dupla ocupam duas células.
pub fn char_width(character: char, column: usize) -> usize {
    if character == '\t' {
        TAB_WIDTH - (column % TAB_WIDTH)
    } else {
        UnicodeWidthChar::width(character).unwrap_or(0)
    }
}

/// Largura da coluna de números de linha, incluindo um espaço de separação.
pub fn gutter_width(line_count: usize) -> u16 {
    let digits = line_count.max(1).to_string().len().max(MIN_GUTTER_DIGITS);
    (digits + 1) as u16
}

/// Calcula o tamanho da área de texto do editor.
pub fn editor_content_size(
    terminal_width: u16,
    terminal_height: u16,
    line_count: usize,
) -> (u16, u16) {
    let rows = terminal_height.saturating_sub(HEADER_HEIGHT + STATUS_HEIGHT);
    let gutter = gutter_width(line_count).min(terminal_width);
    let rule = u16::from(terminal_width > gutter);
    let cols = terminal_width.saturating_sub(gutter).saturating_sub(rule);

    (cols, rows)
}

/// Movimentos de cursor suportados pelo editor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Motion {
    Left,
    Right,
    Up,
    Down,
    LineStart,
    LineEnd,
    DocStart,
    DocEnd,
    PageUp,
    PageDown,
}

/// Estado de visualização do documento.
///
/// O conteúdo continua pertencendo exclusivamente ao `Buffer` do Core. Este
/// tipo mantém apenas posições de cursor, seleção e viewport, sempre expressas
/// em `CharOffset` ou em colunas visuais.
#[derive(Debug, Clone)]
pub struct View {
    cursor: CharOffset,
    anchor: Option<CharOffset>,
    desired_column: Option<usize>,
    top_line: usize,
    left_col: usize,
    rows: u16,
    cols: u16,
}

impl Default for View {
    fn default() -> Self {
        Self::new()
    }
}

impl View {
    pub fn new() -> Self {
        Self {
            cursor: CharOffset(0),
            anchor: None,
            desired_column: None,
            top_line: 0,
            left_col: 0,
            rows: 0,
            cols: 0,
        }
    }

    pub fn reset(&mut self) {
        *self = Self {
            rows: self.rows,
            cols: self.cols,
            ..Self::new()
        };
    }

    pub fn cursor(&self) -> CharOffset {
        self.cursor
    }

    pub fn top_line(&self) -> usize {
        self.top_line
    }

    pub fn left_col(&self) -> usize {
        self.left_col
    }

    pub fn set_content_size(&mut self, cols: u16, rows: u16) {
        self.cols = cols;
        self.rows = rows;
    }

    pub fn content_size(&self) -> (u16, u16) {
        (self.cols, self.rows)
    }

    pub fn restore_state(
        &mut self,
        buffer: &Buffer,
        cursor: CharOffset,
        anchor: Option<CharOffset>,
        top_line: usize,
        left_col: usize,
    ) -> Result<(), BufferError> {
        self.set_cursor(buffer, cursor)?;
        if let Some(anchor) = anchor {
            self.set_selection(buffer, anchor, cursor)?;
        } else {
            self.anchor = None;
        }
        self.top_line = top_line;
        self.left_col = left_col;
        Ok(())
    }

    pub fn has_selection(&self) -> bool {
        self.selection_range().is_some()
    }

    /// Intervalo semiaberto `[start, end)` da seleção, se houver.
    pub fn selection_range(&self) -> Option<TextRange> {
        let anchor = self.anchor?;

        if anchor == self.cursor {
            return None;
        }

        let (start, end) = if anchor < self.cursor {
            (anchor, self.cursor)
        } else {
            (self.cursor, anchor)
        };

        Some(TextRange::new(start, end))
    }

    pub fn clear_selection(&mut self) {
        self.anchor = None;
    }

    /// Coloca o cursor em um offset, normalizando-o para uma posição válida.
    pub fn set_cursor(&mut self, buffer: &Buffer, offset: CharOffset) -> Result<(), BufferError> {
        self.cursor = self.normalized_offset(buffer, offset)?;
        self.ensure_visible(buffer)
    }

    /// Define cursor e âncora de seleção, normalizando ambos.
    pub fn set_selection(
        &mut self,
        buffer: &Buffer,
        anchor: CharOffset,
        cursor: CharOffset,
    ) -> Result<(), BufferError> {
        let anchor = self.normalized_offset(buffer, anchor)?;
        let cursor = self.normalized_offset(buffer, cursor)?;

        self.anchor = Some(anchor);
        self.cursor = cursor;
        self.desired_column = None;
        self.ensure_visible(buffer)
    }

    pub fn select_all(&mut self, buffer: &Buffer) -> Result<(), BufferError> {
        let len = buffer.len_chars()?;
        self.set_selection(buffer, CharOffset(0), CharOffset(len))
    }

    pub fn cursor_line(&self, buffer: &Buffer) -> Result<usize, BufferError> {
        buffer.line_of_char(self.cursor)
    }

    pub fn cursor_visual_column(&self, buffer: &Buffer) -> Result<usize, BufferError> {
        let line = self.cursor_line(buffer)?;
        self.visual_column_of(buffer, line, self.cursor)
    }

    /// Converte um offset da linha em coluna visual.
    pub fn visual_column_of(
        &self,
        buffer: &Buffer,
        line: usize,
        offset: CharOffset,
    ) -> Result<usize, BufferError> {
        let start = buffer.line_start(line)?;
        let end = buffer.line_end(line)?;
        let limit = offset.0.min(end.0);

        let mut column = 0;
        let mut current = start.0;

        while current < limit {
            let character = buffer.char_at(CharOffset(current))?.unwrap_or('\n');
            column += char_width(character, column);
            current += 1;
        }

        Ok(column)
    }

    /// Converte uma coluna visual em offset da linha.
    ///
    /// Se a coluna cair dentro de uma tabulação ou de um caractere de largura
    /// dupla, o offset retornado é o início desse caractere. Se a linha for
    /// menor que a coluna, retorna o fim da linha.
    pub fn offset_at_visual_column(
        &self,
        buffer: &Buffer,
        line: usize,
        target: usize,
    ) -> Result<CharOffset, BufferError> {
        let start = buffer.line_start(line)?;
        let end = buffer.line_end(line)?;

        let mut offset = start.0;
        let mut column = 0;

        while offset < end.0 {
            if column >= target {
                break;
            }

            let character = buffer.char_at(CharOffset(offset))?.unwrap_or('\n');
            let width = char_width(character, column);

            if column + width > target {
                break;
            }

            column += width;
            offset += 1;
        }

        Ok(CharOffset(offset))
    }

    /// Executa um movimento, opcionalmente estendendo a seleção.
    pub fn move_cursor(
        &mut self,
        buffer: &Buffer,
        motion: Motion,
        select: bool,
    ) -> Result<bool, BufferError> {
        if select {
            if self.anchor.is_none() {
                self.anchor = Some(self.cursor);
            }
        } else {
            self.anchor = None;
        }

        let moved = match motion {
            Motion::Up | Motion::Down | Motion::PageUp | Motion::PageDown => {
                let desired = match self.desired_column {
                    Some(column) => column,
                    None => self.cursor_visual_column(buffer)?,
                };

                let moved = self.move_vertical(buffer, motion, desired)?;
                self.desired_column = Some(desired);
                moved
            }
            Motion::Left => {
                self.desired_column = None;
                self.move_left(buffer)?
            }
            Motion::Right => {
                self.desired_column = None;
                self.move_right(buffer)?
            }
            Motion::LineStart => {
                self.desired_column = None;
                let line = self.cursor_line(buffer)?;
                let offset = buffer.line_start(line)?;
                let moved = offset != self.cursor;
                self.cursor = offset;
                moved
            }
            Motion::LineEnd => {
                self.desired_column = None;
                let line = self.cursor_line(buffer)?;
                let offset = buffer.line_end(line)?;
                let moved = offset != self.cursor;
                self.cursor = offset;
                moved
            }
            Motion::DocStart => {
                self.desired_column = None;
                let moved = self.cursor != CharOffset(0);
                self.cursor = CharOffset(0);
                moved
            }
            Motion::DocEnd => {
                self.desired_column = None;
                let offset = CharOffset(buffer.len_chars()?);
                let moved = offset != self.cursor;
                self.cursor = offset;
                moved
            }
        };

        if moved {
            self.ensure_visible(buffer)?;
        }

        Ok(moved)
    }

    fn move_left(&mut self, buffer: &Buffer) -> Result<bool, BufferError> {
        let line = self.cursor_line(buffer)?;
        let start = buffer.line_start(line)?;

        if self.cursor > start {
            self.cursor = CharOffset(self.cursor.0 - 1);
            return Ok(true);
        }

        if line > 0 {
            self.cursor = buffer.line_end(line - 1)?;
            return Ok(true);
        }

        Ok(false)
    }

    fn move_right(&mut self, buffer: &Buffer) -> Result<bool, BufferError> {
        let line = self.cursor_line(buffer)?;
        let end = buffer.line_end(line)?;

        if self.cursor < end {
            self.cursor = CharOffset(self.cursor.0 + 1);
            return Ok(true);
        }

        let lines = buffer.line_count()?;
        if line + 1 < lines {
            self.cursor = buffer.line_start(line + 1)?;
            return Ok(true);
        }

        Ok(false)
    }

    fn move_vertical(
        &mut self,
        buffer: &Buffer,
        motion: Motion,
        desired: usize,
    ) -> Result<bool, BufferError> {
        let lines = buffer.line_count()?;
        let line = self.cursor_line(buffer)?;

        let target = match motion {
            Motion::Up => {
                if line == 0 {
                    return Ok(false);
                }
                line - 1
            }
            Motion::Down => {
                if line + 1 >= lines {
                    return Ok(false);
                }
                line + 1
            }
            Motion::PageUp => line.saturating_sub(self.page_size()),
            Motion::PageDown => (line + self.page_size()).min(lines.saturating_sub(1)),
            _ => return Ok(false),
        };

        if target == line {
            return Ok(false);
        }

        self.cursor = self.offset_at_visual_column(buffer, target, desired)?;

        Ok(true)
    }

    fn page_size(&self) -> usize {
        self.rows.max(1) as usize
    }

    /// Mantém o cursor dentro da área visível, ajustando o viewport.
    pub fn ensure_visible(&mut self, buffer: &Buffer) -> Result<(), BufferError> {
        let lines = buffer.line_count()?;
        let line = self.cursor_line(buffer)?;
        let rows = self.rows.max(1) as usize;

        if line < self.top_line {
            self.top_line = line;
        } else if line >= self.top_line + rows {
            self.top_line = line + 1 - rows;
        }

        let max_top = lines.saturating_sub(rows);
        self.top_line = self.top_line.min(max_top);

        let column = self.cursor_visual_column(buffer)?;

        if self.cols == 0 {
            self.left_col = 0;
        } else {
            let cols = self.cols as usize;

            if column < self.left_col {
                self.left_col = column;
            } else if column >= self.left_col + cols {
                self.left_col = column + 1 - cols;
            }
        }

        Ok(())
    }

    fn normalized_offset(
        &self,
        buffer: &Buffer,
        offset: CharOffset,
    ) -> Result<CharOffset, BufferError> {
        let len = buffer.len_chars()?;
        let offset = offset.0.min(len);
        let line = buffer.line_of_char(CharOffset(offset))?;
        let end = buffer.line_end(line)?;

        Ok(CharOffset(offset.min(end.0)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buffer(text: &str) -> Buffer {
        Buffer::from_text(text)
    }

    #[test]
    fn empty_document_navigation_is_safe() {
        let buffer = buffer("");
        let mut view = View::new();
        view.set_content_size(20, 5);

        for motion in [
            Motion::Left,
            Motion::Right,
            Motion::Up,
            Motion::Down,
            Motion::LineStart,
            Motion::LineEnd,
            Motion::DocStart,
            Motion::DocEnd,
            Motion::PageUp,
            Motion::PageDown,
        ] {
            view.move_cursor(&buffer, motion, false).unwrap();
            assert_eq!(view.cursor(), CharOffset(0));
        }

        assert_eq!(view.cursor_line(&buffer).unwrap(), 0);
        assert_eq!(view.cursor_visual_column(&buffer).unwrap(), 0);
    }

    #[test]
    fn horizontal_movement_respects_document_boundaries() {
        let buffer = buffer("abc");
        let mut view = View::new();
        view.set_content_size(20, 5);

        assert!(!view.move_cursor(&buffer, Motion::Left, false).unwrap());

        for expected in 1..=3 {
            assert!(view.move_cursor(&buffer, Motion::Right, false).unwrap());
            assert_eq!(view.cursor(), CharOffset(expected));
        }

        assert!(!view.move_cursor(&buffer, Motion::Right, false).unwrap());
        assert_eq!(view.cursor(), CharOffset(3));
    }

    #[test]
    fn horizontal_movement_crosses_line_boundaries_without_landing_on_newlines() {
        let buffer = buffer("ab\ncd");
        let mut view = View::new();
        view.set_content_size(20, 5);

        view.move_cursor(&buffer, Motion::LineEnd, false).unwrap();
        assert_eq!(view.cursor(), CharOffset(2));

        assert!(view.move_cursor(&buffer, Motion::Right, false).unwrap());
        assert_eq!(view.cursor(), CharOffset(3));
        assert_eq!(view.cursor_line(&buffer).unwrap(), 1);

        assert!(view.move_cursor(&buffer, Motion::Left, false).unwrap());
        assert_eq!(view.cursor(), CharOffset(2));
        assert_eq!(view.cursor_line(&buffer).unwrap(), 0);
    }

    #[test]
    fn home_end_and_document_motions() {
        let buffer = buffer("alpha\nbeta\ngamma");
        let mut view = View::new();
        view.set_content_size(20, 5);

        view.move_cursor(&buffer, Motion::Down, false).unwrap();
        view.move_cursor(&buffer, Motion::LineEnd, false).unwrap();
        assert_eq!(view.cursor(), CharOffset(10));

        view.move_cursor(&buffer, Motion::LineStart, false).unwrap();
        assert_eq!(view.cursor(), CharOffset(6));

        view.move_cursor(&buffer, Motion::DocEnd, false).unwrap();
        assert_eq!(view.cursor(), CharOffset(16));

        view.move_cursor(&buffer, Motion::DocStart, false).unwrap();
        assert_eq!(view.cursor(), CharOffset(0));
    }

    #[test]
    fn vertical_movement_preserves_desired_visual_column() {
        let buffer = buffer("abcdef\nx\nabcdef");
        let mut view = View::new();
        view.set_content_size(20, 5);

        for _ in 0..3 {
            view.move_cursor(&buffer, Motion::Right, false).unwrap();
        }
        assert_eq!(view.cursor(), CharOffset(3));

        view.move_cursor(&buffer, Motion::Down, false).unwrap();
        assert_eq!(view.cursor(), CharOffset(8));
        assert_eq!(view.cursor_visual_column(&buffer).unwrap(), 1);
        assert_eq!(view.desired_column, Some(3));

        view.move_cursor(&buffer, Motion::Down, false).unwrap();
        assert_eq!(view.cursor(), CharOffset(12));
        assert_eq!(view.cursor_visual_column(&buffer).unwrap(), 3);
        assert_eq!(view.desired_column, Some(3));
    }

    #[test]
    fn horizontal_movement_clears_desired_column() {
        let buffer = buffer("abcdef\nx\nabcdef");
        let mut view = View::new();
        view.set_content_size(20, 5);

        view.move_cursor(&buffer, Motion::Right, false).unwrap();
        view.move_cursor(&buffer, Motion::Down, false).unwrap();
        assert_eq!(view.desired_column, Some(1));

        view.move_cursor(&buffer, Motion::Right, false).unwrap();
        assert_eq!(view.desired_column, None);
    }

    #[test]
    fn page_movement_uses_viewport_height() {
        let text = (0..10)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let buffer = buffer(&text);
        let mut view = View::new();
        view.set_content_size(20, 3);

        view.move_cursor(&buffer, Motion::PageDown, false).unwrap();
        assert_eq!(view.cursor_line(&buffer).unwrap(), 3);

        view.move_cursor(&buffer, Motion::PageDown, false).unwrap();
        assert_eq!(view.cursor_line(&buffer).unwrap(), 6);

        view.move_cursor(&buffer, Motion::PageUp, false).unwrap();
        assert_eq!(view.cursor_line(&buffer).unwrap(), 3);
    }

    #[test]
    fn automatic_vertical_scroll_follows_cursor() {
        let text = (0..10)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let buffer = buffer(&text);
        let mut view = View::new();
        view.set_content_size(20, 3);

        for _ in 0..5 {
            view.move_cursor(&buffer, Motion::Down, false).unwrap();
        }

        assert_eq!(view.cursor_line(&buffer).unwrap(), 5);
        assert_eq!(view.top_line(), 3);

        for _ in 0..5 {
            view.move_cursor(&buffer, Motion::Up, false).unwrap();
        }

        assert_eq!(view.cursor_line(&buffer).unwrap(), 0);
        assert_eq!(view.top_line(), 0);
    }

    #[test]
    fn automatic_horizontal_scroll_follows_cursor() {
        let buffer = buffer("abcdefghijklmnopqrstuvwxyz");
        let mut view = View::new();
        view.set_content_size(10, 3);

        view.move_cursor(&buffer, Motion::LineEnd, false).unwrap();

        assert_eq!(view.cursor_visual_column(&buffer).unwrap(), 26);
        assert_eq!(view.left_col(), 17);
    }

    #[test]
    fn selection_is_semi_open_and_normalized() {
        let buffer = buffer("abcdef");
        let mut view = View::new();
        view.set_content_size(20, 5);

        view.move_cursor(&buffer, Motion::Right, true).unwrap();
        view.move_cursor(&buffer, Motion::Right, true).unwrap();

        assert_eq!(
            view.selection_range(),
            Some(TextRange::new(CharOffset(0), CharOffset(2)))
        );
        assert_eq!(buffer.slice(view.selection_range().unwrap()).unwrap(), "ab");

        view.move_cursor(&buffer, Motion::Left, true).unwrap();
        assert_eq!(
            view.selection_range(),
            Some(TextRange::new(CharOffset(0), CharOffset(1)))
        );

        view.move_cursor(&buffer, Motion::Left, true).unwrap();
        assert_eq!(view.selection_range(), None);
    }

    #[test]
    fn selection_can_span_lines() {
        let buffer = buffer("ab\ncd\nef");
        let mut view = View::new();
        view.set_content_size(20, 5);

        view.move_cursor(&buffer, Motion::LineEnd, false).unwrap();
        view.move_cursor(&buffer, Motion::Right, true).unwrap();
        view.move_cursor(&buffer, Motion::LineEnd, true).unwrap();

        assert_eq!(
            view.selection_range(),
            Some(TextRange::new(CharOffset(2), CharOffset(5)))
        );
        assert_eq!(
            buffer.slice(view.selection_range().unwrap()).unwrap(),
            "\ncd"
        );
    }

    #[test]
    fn select_all_covers_entire_document() {
        let buffer = buffer("café\n界");
        let mut view = View::new();
        view.set_content_size(20, 5);

        view.select_all(&buffer).unwrap();

        assert_eq!(
            view.selection_range(),
            Some(TextRange::new(CharOffset(0), CharOffset(6)))
        );
        assert_eq!(view.cursor(), CharOffset(6));
    }

    #[test]
    fn multibyte_characters_use_character_offsets() {
        let buffer = buffer("café");
        let mut view = View::new();
        view.set_content_size(20, 5);

        for expected in 1..=4 {
            view.move_cursor(&buffer, Motion::Right, false).unwrap();
            assert_eq!(view.cursor(), CharOffset(expected));
        }

        assert_eq!(view.cursor_visual_column(&buffer).unwrap(), 4);
        assert!(view.cursor().0 <= buffer.len_chars().unwrap());
    }

    #[test]
    fn double_width_characters_use_two_visual_columns() {
        let buffer = buffer("界a");
        let mut view = View::new();
        view.set_content_size(20, 5);

        view.move_cursor(&buffer, Motion::Right, false).unwrap();
        assert_eq!(view.cursor(), CharOffset(1));
        assert_eq!(view.cursor_visual_column(&buffer).unwrap(), 2);

        view.move_cursor(&buffer, Motion::Right, false).unwrap();
        assert_eq!(view.cursor(), CharOffset(2));
        assert_eq!(view.cursor_visual_column(&buffer).unwrap(), 3);

        assert_eq!(
            view.offset_at_visual_column(&buffer, 0, 1).unwrap(),
            CharOffset(0)
        );
        assert_eq!(
            view.offset_at_visual_column(&buffer, 0, 2).unwrap(),
            CharOffset(1)
        );
    }

    #[test]
    fn tabs_expand_to_tab_stops() {
        let buffer = buffer("\tx");
        let mut view = View::new();
        view.set_content_size(20, 5);

        assert_eq!(view.visual_column_of(&buffer, 0, CharOffset(1)).unwrap(), 4);
        assert_eq!(view.visual_column_of(&buffer, 0, CharOffset(2)).unwrap(), 5);

        assert_eq!(
            view.offset_at_visual_column(&buffer, 0, 0).unwrap(),
            CharOffset(0)
        );
        assert_eq!(
            view.offset_at_visual_column(&buffer, 0, 3).unwrap(),
            CharOffset(0)
        );
        assert_eq!(
            view.offset_at_visual_column(&buffer, 0, 4).unwrap(),
            CharOffset(1)
        );
    }

    #[test]
    fn cursor_normalization_never_lands_on_line_endings() {
        let buffer = buffer("ab\r\ncd");
        let mut view = View::new();
        view.set_content_size(20, 5);

        view.set_cursor(&buffer, CharOffset(3)).unwrap();
        assert_eq!(view.cursor(), CharOffset(2));

        view.set_cursor(&buffer, CharOffset(2)).unwrap();
        assert_eq!(view.cursor(), CharOffset(2));

        view.set_cursor(&buffer, CharOffset(4)).unwrap();
        assert_eq!(view.cursor(), CharOffset(4));
    }

    #[test]
    fn zero_width_viewport_is_safe() {
        let buffer = buffer("abcdef");
        let mut view = View::new();
        view.set_content_size(0, 0);

        view.move_cursor(&buffer, Motion::LineEnd, false).unwrap();
        view.move_cursor(&buffer, Motion::Down, false).unwrap();

        assert_eq!(view.left_col(), 0);
        assert_eq!(view.top_line(), 0);
    }
}
