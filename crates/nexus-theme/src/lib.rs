//! Temas independentes de interface e sintaxe para a Nexus.
//!
//! O crate não depende de Ratatui, terminal ou `nexus-core`. A interface
//! converte [`Rgb`] para as capacidades do terminal; a futura GUI pode usar o
//! mesmo modelo diretamente.
//!
//! O formato TOML e a localização dos arquivos estão documentados em
//! `docs/THEMES.md`.

mod color;
mod theme;

pub use color::{ColorError, Rgb};
pub use theme::{
    CONFIG_SCHEMA_VERSION, DEFAULT_THEME_ID, LoadedThemes, ResolvedThemes, SCHEMA_VERSION,
    SyntaxTheme, Theme, ThemeCatalog, ThemeConfig, ThemeError, ThemePaths, ThemeWarning, UiTheme,
    default_paths, load_from_environment, load_from_paths, save_config,
};
