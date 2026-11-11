use std::fmt;
use std::str::FromStr;

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Cor RGB independente de interface.
///
/// O tema armazena sempre RGB. A conversão para as capacidades do terminal
/// (True Color, 256 cores ou 16 cores) é responsabilidade da interface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    /// Interpreta `#RRGGBB` (maiúsculas ou minúsculas).
    ///
    /// Formatos abreviados ou nomes de cores não são aceitos para evitar
    /// ambiguidade entre temas.
    pub fn parse_hex(input: &str) -> Result<Self, ColorError> {
        let value = input.trim();
        let digits = value
            .strip_prefix('#')
            .ok_or_else(|| ColorError::new(input, "esperado o prefixo '#'"))?;

        if digits.len() != 6
            || !digits
                .chars()
                .all(|character| character.is_ascii_hexdigit())
        {
            return Err(ColorError::new(
                input,
                "esperado #RRGGBB com 6 dígitos hexadecimais",
            ));
        }

        let r = u8::from_str_radix(&digits[0..2], 16).expect("hexadecimal validado");
        let g = u8::from_str_radix(&digits[2..4], 16).expect("hexadecimal validado");
        let b = u8::from_str_radix(&digits[4..6], 16).expect("hexadecimal validado");

        Ok(Self { r, g, b })
    }

    pub fn to_hex(self) -> String {
        format!("#{:02x}{:02x}{:02x}", self.r, self.g, self.b)
    }

    /// Luminância relativa segundo a WCAG 2.x.
    pub fn relative_luminance(self) -> f32 {
        fn channel(value: u8) -> f32 {
            let value = f32::from(value) / 255.0;
            if value <= 0.040_45 {
                value / 12.92
            } else {
                ((value + 0.055) / 1.055).powf(2.4)
            }
        }

        0.2126 * channel(self.r) + 0.7152 * channel(self.g) + 0.0722 * channel(self.b)
    }

    /// Razão de contraste WCAG entre duas cores (1.0 a 21.0).
    pub fn contrast_ratio(self, other: Self) -> f32 {
        let first = self.relative_luminance();
        let second = other.relative_luminance();
        let (lighter, darker) = if first >= second {
            (first, second)
        } else {
            (second, first)
        };
        (lighter + 0.05) / (darker + 0.05)
    }

    /// Índice aproximado na paleta xterm de 256 cores.
    pub fn nearest_ansi256(self) -> u8 {
        const CUBE_LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];

        fn nearest_level(value: u8) -> u8 {
            let mut best = 0;
            let mut best_distance = u32::MAX;
            for (index, level) in CUBE_LEVELS.iter().enumerate() {
                let distance = (i32::from(value) - i32::from(*level)).unsigned_abs();
                if distance < best_distance {
                    best_distance = distance;
                    best = index as u8;
                }
            }
            best
        }

        let r = nearest_level(self.r);
        let g = nearest_level(self.g);
        let b = nearest_level(self.b);
        let cube_index = 16 + 36 * r + 6 * g + b;
        let cube = Rgb::new(
            CUBE_LEVELS[r as usize],
            CUBE_LEVELS[g as usize],
            CUBE_LEVELS[b as usize],
        );

        let average = (u16::from(self.r) + u16::from(self.g) + u16::from(self.b)) / 3;
        let gray_index = if average > 238 {
            23
        } else if average < 3 {
            0
        } else {
            ((average - 3) / 10).min(23) as u8
        };
        let gray_value = 8 + 10 * gray_index;
        let gray = Rgb::new(gray_value, gray_value, gray_value);

        if distance_squared(self, gray) < distance_squared(self, cube) {
            232 + gray_index
        } else {
            cube_index
        }
    }

    /// Índice aproximado na paleta ANSI de 16 cores.
    pub fn nearest_ansi16(self) -> u8 {
        const ANSI16: [Rgb; 16] = [
            Rgb::new(0, 0, 0),
            Rgb::new(128, 0, 0),
            Rgb::new(0, 128, 0),
            Rgb::new(128, 128, 0),
            Rgb::new(0, 0, 128),
            Rgb::new(128, 0, 128),
            Rgb::new(0, 128, 128),
            Rgb::new(192, 192, 192),
            Rgb::new(128, 128, 128),
            Rgb::new(255, 0, 0),
            Rgb::new(0, 255, 0),
            Rgb::new(255, 255, 0),
            Rgb::new(0, 0, 255),
            Rgb::new(255, 0, 255),
            Rgb::new(0, 255, 255),
            Rgb::new(255, 255, 255),
        ];

        ANSI16
            .iter()
            .enumerate()
            .min_by_key(|(_, candidate)| distance_squared(self, **candidate))
            .map(|(index, _)| index as u8)
            .unwrap_or(7)
    }
}

fn distance_squared(first: Rgb, second: Rgb) -> u32 {
    let r = i32::from(first.r) - i32::from(second.r);
    let g = i32::from(first.g) - i32::from(second.g);
    let b = i32::from(first.b) - i32::from(second.b);
    (r * r + g * g + b * b) as u32
}

impl fmt::Display for Rgb {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.to_hex())
    }
}

impl FromStr for Rgb {
    type Err = ColorError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        Self::parse_hex(input)
    }
}

impl Serialize for Rgb {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for Rgb {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct RgbVisitor;

        impl Visitor<'_> for RgbVisitor {
            type Value = Rgb;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("uma cor no formato #RRGGBB")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Rgb::parse_hex(value).map_err(E::custom)
            }
        }

        deserializer.deserialize_str(RgbVisitor)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColorError {
    value: String,
    reason: &'static str,
}

impl ColorError {
    fn new(value: &str, reason: &'static str) -> Self {
        Self {
            value: value.to_owned(),
            reason,
        }
    }
}

impl fmt::Display for ColorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "cor inválida `{}`: {}", self.value, self.reason)
    }
}

impl std::error::Error for ColorError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_formats_hex_colors() {
        let color = Rgb::parse_hex("#1E1E2E").unwrap();

        assert_eq!(color, Rgb::new(0x1e, 0x1e, 0x2e));
        assert_eq!(color.to_hex(), "#1e1e2e");
        assert_eq!(
            "#89B4FA".parse::<Rgb>().unwrap(),
            Rgb::new(0x89, 0xb4, 0xfa)
        );
    }

    #[test]
    fn rejects_invalid_colors() {
        for value in ["", "1e1e2e", "#fff", "#gggggg", "#1234567", "#12345z"] {
            assert!(
                Rgb::parse_hex(value).is_err(),
                "{value} deveria ser inválida"
            );
        }
    }

    #[test]
    fn contrast_ratio_is_symmetric_and_bounded() {
        let black = Rgb::new(0, 0, 0);
        let white = Rgb::new(255, 255, 255);

        assert!((black.contrast_ratio(white) - 21.0).abs() < 0.01);
        assert!((black.contrast_ratio(white) - white.contrast_ratio(black)).abs() < 0.01);
        assert!((black.contrast_ratio(black) - 1.0).abs() < 0.01);
    }

    #[test]
    fn maps_known_colors_to_ansi_palettes() {
        assert_eq!(Rgb::new(255, 0, 0).nearest_ansi16(), 9);
        assert_eq!(Rgb::new(0, 0, 0).nearest_ansi16(), 0);
        assert_eq!(Rgb::new(255, 255, 255).nearest_ansi16(), 15);
        assert_eq!(Rgb::new(255, 0, 0).nearest_ansi256(), 196);
    }
}
