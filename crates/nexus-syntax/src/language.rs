use std::path::Path;

use tree_sitter::Language;

use crate::queries;

/// Linguagens com suporte inicial de highlighting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LanguageId {
    Rust,
    Python,
    JavaScript,
    TypeScript,
    Tsx,
}

impl LanguageId {
    /// Detecta a linguagem pela extensão ou por nomes especiais de arquivo.
    pub fn from_path(path: &Path) -> Option<Self> {
        if let Some(name) = path.file_name().and_then(|name| name.to_str())
            && let Some(language) = Self::from_filename(name)
        {
            return Some(language);
        }

        path.extension()
            .and_then(|extension| extension.to_str())
            .and_then(Self::from_extension)
    }

    pub fn from_extension(extension: &str) -> Option<Self> {
        match extension.to_ascii_lowercase().as_str() {
            "rs" => Some(Self::Rust),
            "py" | "pyw" | "pyi" => Some(Self::Python),
            "js" | "mjs" | "cjs" | "jsx" => Some(Self::JavaScript),
            "ts" | "mts" | "cts" => Some(Self::TypeScript),
            "tsx" => Some(Self::Tsx),
            _ => None,
        }
    }

    pub fn from_filename(name: &str) -> Option<Self> {
        match name {
            "SConstruct" | "SConscript" | "wscript" => Some(Self::Python),
            "Jakefile" => Some(Self::JavaScript),
            _ => None,
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            Self::Rust => "Rust",
            Self::Python => "Python",
            Self::JavaScript => "JavaScript",
            Self::TypeScript | Self::Tsx => "TypeScript",
        }
    }

    pub(crate) fn grammar(self) -> Language {
        match self {
            Self::Rust => tree_sitter_rust::LANGUAGE.into(),
            Self::Python => tree_sitter_python::LANGUAGE.into(),
            Self::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
            Self::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Self::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
        }
    }

    pub(crate) fn highlight_query(self) -> String {
        match self {
            Self::Rust => queries::RUST.to_owned(),
            Self::Python => queries::PYTHON.to_owned(),
            Self::JavaScript => queries::JAVASCRIPT.to_owned(),
            Self::TypeScript | Self::Tsx => queries::TYPESCRIPT.to_owned(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_initial_languages_by_extension() {
        assert_eq!(
            LanguageId::from_path(Path::new("main.rs")),
            Some(LanguageId::Rust)
        );
        assert_eq!(
            LanguageId::from_path(Path::new("script.PY")),
            Some(LanguageId::Python)
        );
        assert_eq!(
            LanguageId::from_path(Path::new("app.js")),
            Some(LanguageId::JavaScript)
        );
        assert_eq!(
            LanguageId::from_path(Path::new("module.mts")),
            Some(LanguageId::TypeScript)
        );
        assert_eq!(
            LanguageId::from_path(Path::new("view.tsx")),
            Some(LanguageId::Tsx)
        );
    }

    #[test]
    fn detects_special_filenames() {
        assert_eq!(
            LanguageId::from_path(Path::new("SConstruct")),
            Some(LanguageId::Python)
        );
        assert_eq!(
            LanguageId::from_path(Path::new("Jakefile")),
            Some(LanguageId::JavaScript)
        );
    }

    #[test]
    fn unknown_extensions_are_plain_text() {
        assert_eq!(LanguageId::from_path(Path::new("notes.txt")), None);
        assert_eq!(LanguageId::from_path(Path::new("Makefile")), None);
    }
}
