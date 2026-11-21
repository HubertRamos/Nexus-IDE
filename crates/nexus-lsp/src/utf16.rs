//! Conversão entre offsets internos do Nexus e posições UTF-16 do LSP.
//!
//! Toda a lógica de UTF-16 fica isolada neste crate. Os offsets do Nexus são
//! contagens de caracteres Unicode (a mesma unidade usada pelo Ropey), enquanto
//! o LSP usa linhas e unidades UTF-16. Um caractere fora do plano básico
//! multilíngue ocupa dois code units UTF-16, mas continua sendo um único
//! caractere para o Nexus.
//!
//! As funções abaixo tratam `\n`, `\r\n` e `\r` como quebras de linha. As
//! posições LSP nunca apontam para dentro de uma quebra de linha: o fim de uma
//! linha é a posição imediatamente antes do terminador.

use std::fmt;

use crate::protocol::{Position, Range};

/// Falhas de conversão entre offsets de caracteres e posições LSP.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Utf16Error {
    /// O offset de caractere ultrapassa o tamanho do texto.
    OffsetOutOfBounds { offset: usize, len: usize },
    /// A linha solicitada não existe no texto.
    LineOutOfBounds { line: u32, lines: u32 },
    /// A coluna UTF-16 ultrapassa o conteúdo da linha.
    CharacterOutOfBounds {
        line: u32,
        character: u32,
        line_len: u32,
    },
    /// O offset cai entre `\r` e `\n` de uma quebra CRLF.
    SplitLineEnding { offset: usize },
}

impl fmt::Display for Utf16Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OffsetOutOfBounds { offset, len } => write!(
                formatter,
                "offset de caractere fora dos limites: {offset} (comprimento {len})"
            ),
            Self::LineOutOfBounds { line, lines } => {
                write!(formatter, "linha {line} inexistente (total {lines})")
            }
            Self::CharacterOutOfBounds {
                line,
                character,
                line_len,
            } => write!(
                formatter,
                "coluna UTF-16 {character} fora da linha {line} (comprimento {line_len})"
            ),
            Self::SplitLineEnding { offset } => write!(
                formatter,
                "offset {offset} cai dentro de uma quebra de linha CRLF"
            ),
        }
    }
}

impl std::error::Error for Utf16Error {}

/// Quantidade de unidades UTF-16 necessárias para representar `text`.
pub fn utf16_len(text: &str) -> usize {
    text.encode_utf16().count()
}

/// Converte um offset em caracteres para uma posição LSP.
///
/// O offset pode ser igual ao comprimento total do texto; nesse caso a posição
/// resultante é o fim da última linha.
pub fn position_from_offset(text: &str, offset: usize) -> Result<Position, Utf16Error> {
    let len = text.chars().count();
    if offset > len {
        return Err(Utf16Error::OffsetOutOfBounds { offset, len });
    }

    let mut line = 0u32;
    let mut character = 0u32;
    let mut index = 0usize;
    let mut chars = text.chars().peekable();

    while index < offset {
        let Some(current) = chars.next() else {
            return Err(Utf16Error::OffsetOutOfBounds { offset, len });
        };

        match current {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    if index + 1 >= offset {
                        return Err(Utf16Error::SplitLineEnding { offset });
                    }
                    chars.next();
                    index += 2;
                } else {
                    index += 1;
                }
                line += 1;
                character = 0;
            }
            '\n' => {
                index += 1;
                line += 1;
                character = 0;
            }
            other => {
                index += 1;
                character += other.len_utf16() as u32;
            }
        }
    }

    Ok(Position { line, character })
}

/// Converte uma posição LSP para um offset em caracteres.
pub fn offset_from_position(text: &str, position: Position) -> Result<usize, Utf16Error> {
    let mut line = 0u32;
    let mut char_index = 0usize;
    let mut chars = text.chars().peekable();

    loop {
        if line == position.line {
            let mut character = 0u32;

            loop {
                if character == position.character {
                    return Ok(char_index);
                }

                let Some(&current) = chars.peek() else {
                    return Err(Utf16Error::CharacterOutOfBounds {
                        line,
                        character: position.character,
                        line_len: character,
                    });
                };

                if current == '\r' || current == '\n' {
                    return Err(Utf16Error::CharacterOutOfBounds {
                        line,
                        character: position.character,
                        line_len: character,
                    });
                }

                let next = character + current.len_utf16() as u32;
                if next > position.character {
                    return Err(Utf16Error::CharacterOutOfBounds {
                        line,
                        character: position.character,
                        line_len: character,
                    });
                }

                character = next;
                char_index += 1;
                chars.next();
            }
        }

        match chars.next() {
            None => {
                return Err(Utf16Error::LineOutOfBounds {
                    line: position.line,
                    lines: line + 1,
                });
            }
            Some('\r') => {
                char_index += 1;
                if chars.peek() == Some(&'\n') {
                    chars.next();
                    char_index += 1;
                }
                line += 1;
            }
            Some('\n') => {
                char_index += 1;
                line += 1;
            }
            Some(_) => {
                char_index += 1;
            }
        }
    }
}

/// Converte um intervalo de offsets de caracteres para um [`Range`] LSP.
pub fn range_from_offsets(text: &str, start: usize, end: usize) -> Result<Range, Utf16Error> {
    if start > end {
        return Err(Utf16Error::OffsetOutOfBounds {
            offset: start,
            len: end,
        });
    }

    Ok(Range {
        start: position_from_offset(text, start)?,
        end: position_from_offset(text, end)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn position(line: u32, character: u32) -> Position {
        Position { line, character }
    }

    #[test]
    fn ascii_positions_match_bytes_and_characters() {
        let text = "abc\ndef\n";

        assert_eq!(position_from_offset(text, 0).unwrap(), position(0, 0));
        assert_eq!(position_from_offset(text, 3).unwrap(), position(0, 3));
        assert_eq!(position_from_offset(text, 4).unwrap(), position(1, 0));
        assert_eq!(position_from_offset(text, 7).unwrap(), position(1, 3));
        assert_eq!(position_from_offset(text, 8).unwrap(), position(2, 0));
    }

    #[test]
    fn accents_are_counted_as_utf16_units() {
        let text = "café\nção";

        assert_eq!(position_from_offset(text, 4).unwrap(), position(0, 4));
        assert_eq!(position_from_offset(text, 5).unwrap(), position(1, 0));
        assert_eq!(position_from_offset(text, 7).unwrap(), position(1, 2));
        assert_eq!(position_from_offset(text, 8).unwrap(), position(1, 3));
    }

    #[test]
    fn emoji_uses_two_utf16_units_but_one_character() {
        let text = "a😀b";

        assert_eq!(utf16_len("😀"), 2);
        assert_eq!(position_from_offset(text, 0).unwrap(), position(0, 0));
        assert_eq!(position_from_offset(text, 1).unwrap(), position(0, 1));
        assert_eq!(position_from_offset(text, 2).unwrap(), position(0, 3));
        assert_eq!(position_from_offset(text, 3).unwrap(), position(0, 4));
    }

    #[test]
    fn characters_outside_bmp_round_trip() {
        let text = "x𝄞y\n😀z";

        for offset in 0..=text.chars().count() {
            let lsp = position_from_offset(text, offset).unwrap();
            assert_eq!(offset_from_position(text, lsp).unwrap(), offset);
        }
    }

    #[test]
    fn crlf_is_a_single_line_break() {
        let text = "a\r\nb\r\n";

        assert_eq!(position_from_offset(text, 0).unwrap(), position(0, 0));
        assert_eq!(position_from_offset(text, 1).unwrap(), position(0, 1));
        assert_eq!(
            position_from_offset(text, 2).unwrap_err(),
            Utf16Error::SplitLineEnding { offset: 2 }
        );
        assert_eq!(position_from_offset(text, 3).unwrap(), position(1, 0));
        assert_eq!(position_from_offset(text, 4).unwrap(), position(1, 1));
        assert_eq!(
            position_from_offset(text, 5).unwrap_err(),
            Utf16Error::SplitLineEnding { offset: 5 }
        );
        assert_eq!(position_from_offset(text, 6).unwrap(), position(2, 0));

        assert_eq!(offset_from_position(text, position(0, 1)).unwrap(), 1);
        assert_eq!(offset_from_position(text, position(1, 0)).unwrap(), 3);
        assert_eq!(offset_from_position(text, position(1, 1)).unwrap(), 4);
    }

    #[test]
    fn lone_cr_is_a_line_break() {
        let text = "a\rb";

        assert_eq!(position_from_offset(text, 1).unwrap(), position(0, 1));
        assert_eq!(position_from_offset(text, 2).unwrap(), position(1, 0));
        assert_eq!(offset_from_position(text, position(1, 0)).unwrap(), 2);
    }

    #[test]
    fn line_boundaries_and_end_of_document() {
        let text = "primeira\nsegunda";

        assert_eq!(offset_from_position(text, position(0, 0)).unwrap(), 0);
        assert_eq!(offset_from_position(text, position(0, 8)).unwrap(), 8);
        assert_eq!(offset_from_position(text, position(1, 0)).unwrap(), 9);
        assert_eq!(offset_from_position(text, position(1, 7)).unwrap(), 16);
    }

    #[test]
    fn empty_text_has_one_empty_line() {
        assert_eq!(position_from_offset("", 0).unwrap(), position(0, 0));
        assert_eq!(offset_from_position("", position(0, 0)).unwrap(), 0);
        assert_eq!(
            offset_from_position("", position(1, 0)).unwrap_err(),
            Utf16Error::LineOutOfBounds { line: 1, lines: 1 }
        );
    }

    #[test]
    fn invalid_positions_are_rejected() {
        let text = "abc";

        assert!(matches!(
            offset_from_position(text, position(0, 4)),
            Err(Utf16Error::CharacterOutOfBounds { .. })
        ));
        assert!(matches!(
            offset_from_position(text, position(1, 0)),
            Err(Utf16Error::LineOutOfBounds { .. })
        ));
        assert!(matches!(
            position_from_offset(text, 4),
            Err(Utf16Error::OffsetOutOfBounds { .. })
        ));
    }

    #[test]
    fn range_from_offsets_uses_current_text() {
        let text = "a😀b\ncafé";

        assert_eq!(
            range_from_offsets(text, 1, 2).unwrap(),
            Range {
                start: position(0, 1),
                end: position(0, 3),
            }
        );
        assert_eq!(
            range_from_offsets(text, 4, 8).unwrap(),
            Range {
                start: position(1, 0),
                end: position(1, 4),
            }
        );
    }
}
