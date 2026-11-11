use std::env;

use nexus_syntax::TokenKind;
use nexus_theme::{LoadedThemes, Rgb, SyntaxTheme, ThemePaths, UiTheme, load_from_environment};
use ratatui::style::{Color, Modifier, Style};

macro_rules! ui_colors {
    ($($name:ident),* $(,)?) => {
        $(
            pub fn $name(&self) -> Color {
                self.color(self.ui.$name)
            }
        )*
    };
}

/// Capacidade de cor do terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorMode {
    TrueColor,
    Ansi256,
    Ansi16,
    NoColor,
}

impl ColorMode {
    /// Detecta a capacidade a partir do ambiente.
    ///
    /// A variável `NEXUS_COLOR` permite sobrepor a detecção:
    /// `truecolor`, `256`, `16` ou `none`.
    pub fn detect() -> Self {
        if let Ok(value) = env::var("NEXUS_COLOR")
            && let Some(mode) = Self::parse(&value)
        {
            return mode;
        }

        if env::var_os("NO_COLOR").is_some() {
            return ColorMode::NoColor;
        }

        let colorterm = env::var("COLORTERM")
            .unwrap_or_default()
            .to_ascii_lowercase();
        if colorterm.contains("truecolor") || colorterm.contains("24bit") {
            return ColorMode::TrueColor;
        }

        let term = env::var("TERM").unwrap_or_default().to_ascii_lowercase();
        if term.contains("256color") {
            return ColorMode::Ansi256;
        }
        if term == "dumb" {
            return ColorMode::NoColor;
        }

        ColorMode::Ansi16
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "truecolor" | "24bit" | "true" => Some(ColorMode::TrueColor),
            "256" | "256color" | "ansi256" => Some(ColorMode::Ansi256),
            "16" | "16color" | "ansi" | "ansi16" => Some(ColorMode::Ansi16),
            "none" | "no" | "mono" => Some(ColorMode::NoColor),
            _ => None,
        }
    }

    pub fn color(self, rgb: Rgb) -> Color {
        match self {
            ColorMode::TrueColor => Color::Rgb(rgb.r, rgb.g, rgb.b),
            ColorMode::Ansi256 => Color::Indexed(rgb.nearest_ansi256()),
            ColorMode::Ansi16 => Color::Indexed(rgb.nearest_ansi16()),
            ColorMode::NoColor => Color::Reset,
        }
    }
}

/// Tema resolvido e adaptado às capacidades do terminal.
#[derive(Debug, Clone)]
pub struct ThemeState {
    ui: UiTheme,
    syntax: SyntaxTheme,
    mode: ColorMode,
    ui_id: String,
    syntax_id: String,
    warnings: Vec<String>,
    paths: Option<ThemePaths>,
}

impl ThemeState {
    /// Temas internos padrão, sem ler configuração do usuário.
    ///
    /// Usado pelos construtores de teste e por aplicações com caminhos
    /// explícitos para manter execuções determinísticas.
    #[allow(dead_code)]
    pub fn builtin(mode: ColorMode) -> Self {
        use nexus_theme::{ThemeCatalog, ThemeConfig};

        let catalog = ThemeCatalog::with_builtins();
        let resolved = catalog.resolve(&ThemeConfig::default());

        Self {
            ui: resolved.ui,
            syntax: resolved.syntax,
            mode,
            ui_id: resolved.ui_id,
            syntax_id: resolved.syntax_id,
            warnings: resolved
                .warnings
                .into_iter()
                .map(|warning| warning.to_string())
                .collect(),
            paths: None,
        }
    }

    /// Carrega temas internos, externos e a seleção persistida no ambiente.
    pub fn from_environment(mode: ColorMode) -> Self {
        Self::from_loaded(load_from_environment(), mode)
    }

    pub fn from_loaded(loaded: LoadedThemes, mode: ColorMode) -> Self {
        Self {
            ui: loaded.ui,
            syntax: loaded.syntax,
            mode,
            ui_id: loaded.ui_id,
            syntax_id: loaded.syntax_id,
            warnings: loaded
                .warnings
                .into_iter()
                .map(|warning| warning.to_string())
                .collect(),
            paths: loaded.paths,
        }
    }

    pub fn mode(&self) -> ColorMode {
        self.mode
    }

    pub fn ui_id(&self) -> &str {
        &self.ui_id
    }

    pub fn syntax_id(&self) -> &str {
        &self.syntax_id
    }

    pub fn paths(&self) -> Option<&ThemePaths> {
        self.paths.as_ref()
    }

    pub fn warning_summary(&self) -> Option<String> {
        if self.warnings.is_empty() {
            None
        } else {
            Some(self.warnings.join("; "))
        }
    }

    pub fn color(&self, rgb: Rgb) -> Color {
        self.mode.color(rgb)
    }

    pub fn syntax_color(&self, kind: TokenKind) -> Color {
        let rgb = match kind {
            TokenKind::Keyword => self.syntax.keyword,
            TokenKind::String => self.syntax.string,
            TokenKind::Comment => self.syntax.comment,
            TokenKind::Number => self.syntax.number,
            TokenKind::Function => self.syntax.function,
            TokenKind::Method => self.syntax.method,
            TokenKind::Type => self.syntax.type_name,
            TokenKind::Variable => self.syntax.variable,
            TokenKind::Constant => self.syntax.constant,
            TokenKind::Operator => self.syntax.operator,
            TokenKind::Attribute => self.syntax.attribute,
        };

        self.mode.color(rgb)
    }

    pub fn selection_style(&self) -> Style {
        if self.mode == ColorMode::NoColor {
            Style::new().add_modifier(Modifier::REVERSED)
        } else {
            Style::new().bg(self.selection()).fg(self.selection_text())
        }
    }

    pub fn cursor_style(&self) -> Style {
        if self.mode == ColorMode::NoColor {
            Style::new().add_modifier(Modifier::REVERSED)
        } else {
            Style::new().bg(self.cursor()).fg(self.cursor_text())
        }
    }

    ui_colors! {
        background,
        surface,
        panel,
        border,
        foreground,
        muted,
        accent,
        cursor,
        cursor_text,
        selection,
        selection_text,
        current_line,
        status_bar,
        status_bar_text,
        success,
        error,
        warning,
        dialog,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_color_mode_overrides() {
        assert_eq!(ColorMode::parse("truecolor"), Some(ColorMode::TrueColor));
        assert_eq!(ColorMode::parse("24bit"), Some(ColorMode::TrueColor));
        assert_eq!(ColorMode::parse("256"), Some(ColorMode::Ansi256));
        assert_eq!(ColorMode::parse("ansi256"), Some(ColorMode::Ansi256));
        assert_eq!(ColorMode::parse("16"), Some(ColorMode::Ansi16));
        assert_eq!(ColorMode::parse("ansi"), Some(ColorMode::Ansi16));
        assert_eq!(ColorMode::parse("none"), Some(ColorMode::NoColor));
        assert_eq!(ColorMode::parse("mono"), Some(ColorMode::NoColor));
        assert_eq!(ColorMode::parse("desconhecido"), None);
    }

    #[test]
    fn converts_rgb_according_to_terminal_capability() {
        let rgb = Rgb::new(255, 0, 0);

        assert_eq!(ColorMode::TrueColor.color(rgb), Color::Rgb(255, 0, 0));
        assert_eq!(
            ColorMode::Ansi256.color(rgb),
            Color::Indexed(rgb.nearest_ansi256())
        );
        assert_eq!(
            ColorMode::Ansi16.color(rgb),
            Color::Indexed(rgb.nearest_ansi16())
        );
        assert_eq!(ColorMode::NoColor.color(rgb), Color::Reset);
    }

    #[test]
    fn no_color_mode_uses_reversed_selection_and_cursor() {
        let theme = ThemeState::builtin(ColorMode::NoColor);

        assert!(
            theme
                .selection_style()
                .add_modifier
                .contains(Modifier::REVERSED)
        );
        assert!(
            theme
                .cursor_style()
                .add_modifier
                .contains(Modifier::REVERSED)
        );
    }

    #[test]
    fn syntax_colors_follow_the_selected_syntax_theme() {
        let catalog = nexus_theme::ThemeCatalog::with_builtins();
        let resolved = catalog.resolve(&nexus_theme::ThemeConfig {
            ui: "nexus-dark".to_owned(),
            syntax: "nexus-light".to_owned(),
        });
        let theme = ThemeState::from_loaded(
            LoadedThemes {
                ui: resolved.ui,
                syntax: resolved.syntax,
                ui_id: resolved.ui_id,
                syntax_id: resolved.syntax_id,
                warnings: resolved.warnings,
                paths: None,
            },
            ColorMode::TrueColor,
        );
        let light = catalog.get("nexus-light").unwrap().syntax.clone().unwrap();

        assert_eq!(
            theme.syntax_color(TokenKind::Keyword),
            Color::Rgb(light.keyword.r, light.keyword.g, light.keyword.b)
        );
    }
}
