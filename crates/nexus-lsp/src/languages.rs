//! Presets de configuração para os primeiros servidores LSP suportados.
//!
//! O núcleo em [`crate::manager`] é genérico. Este módulo apenas declara
//! executáveis e argumentos padrão; caminhos personalizados podem ser
//! aplicados com [`ServerConfig::with_executable`] e
//! [`ServerConfig::with_argument`].

use crate::manager::ServerConfig;

/// Identificador LSP da linguagem Rust.
pub const RUST_LANGUAGE_ID: &str = "rust";
/// Identificador LSP da linguagem Python.
pub const PYTHON_LANGUAGE_ID: &str = "python";
/// Executável padrão do servidor Rust.
pub const RUST_ANALYZER_EXECUTABLE: &str = "rust-analyzer";
/// Executável padrão do servidor Python.
pub const PYRIGHT_EXECUTABLE: &str = "pyright-langserver";

/// Configuração inicial do `rust-analyzer`.
///
/// O executável pode ser substituído por um caminho absoluto ou por um nome
/// disponível no `PATH`.
pub fn rust_analyzer() -> ServerConfig {
    ServerConfig::new(RUST_LANGUAGE_ID, RUST_ANALYZER_EXECUTABLE)
}

/// Configuração inicial do `pyright-langserver`.
///
/// O modo `--stdio` é obrigatório para que o Pyright use o transporte LSP
/// sobre entrada e saída padrão.
pub fn pyright() -> ServerConfig {
    ServerConfig::new(PYTHON_LANGUAGE_ID, PYRIGHT_EXECUTABLE).with_argument("--stdio")
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use super::*;

    #[test]
    fn rust_preset_uses_rust_analyzer() {
        let config = rust_analyzer();

        assert_eq!(config.language_id, "rust");
        assert_eq!(config.executable, std::path::PathBuf::from("rust-analyzer"));
        assert!(config.arguments.is_empty());
    }

    #[test]
    fn pyright_preset_uses_stdio() {
        let config = pyright();

        assert_eq!(config.language_id, "python");
        assert_eq!(
            config.executable,
            std::path::PathBuf::from("pyright-langserver")
        );
        assert_eq!(config.arguments, vec![OsString::from("--stdio")]);
    }

    #[test]
    fn presets_accept_custom_paths_and_arguments() {
        let config = pyright()
            .with_executable("/opt/pyright-langserver")
            .with_argument("--verbose");

        assert_eq!(
            config.executable,
            std::path::PathBuf::from("/opt/pyright-langserver")
        );
        assert_eq!(
            config.arguments,
            vec![OsString::from("--stdio"), OsString::from("--verbose")]
        );
    }
}
