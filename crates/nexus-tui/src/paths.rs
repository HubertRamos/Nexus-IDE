//! Localizações persistentes da aplicação.
//!
//! Reutiliza a convenção já usada por `nexus-theme` para `config.toml`; sessão,
//! recuperação e projetos recentes ficam no mesmo diretório de configuração,
//! sem criar um sistema paralelo.

use std::path::PathBuf;

use nexus_theme::{ThemePaths, default_paths};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppPaths {
    pub config_file: PathBuf,
    pub themes_dir: PathBuf,
    pub root: PathBuf,
    pub session_file: PathBuf,
    pub recent_file: PathBuf,
    pub recovery_dir: PathBuf,
}

impl AppPaths {
    pub fn from_theme_paths(paths: &ThemePaths) -> Self {
        let root = paths
            .config_file
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));

        Self {
            config_file: paths.config_file.clone(),
            themes_dir: paths.themes_dir.clone(),
            session_file: root.join("session.toml"),
            recent_file: root.join("recent.toml"),
            recovery_dir: root.join("recovery"),
            root,
        }
    }

    pub fn from_environment() -> Option<Self> {
        default_paths().map(|paths| Self::from_theme_paths(&paths))
    }
}

impl Default for AppPaths {
    fn default() -> Self {
        Self {
            config_file: PathBuf::from("~/.nexus/config.toml"),
            themes_dir: PathBuf::from("~/.nexus/themes"),
            root: PathBuf::from("~/.nexus"),
            session_file: PathBuf::from("~/.nexus/session.toml"),
            recent_file: PathBuf::from("~/.nexus/recent.toml"),
            recovery_dir: PathBuf::from("~/.nexus/recovery"),
        }
    }
}

use std::path::Path;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_auxiliary_paths_from_config_file() {
        let paths = ThemePaths::new("/tmp/nexus/config.toml", "/tmp/nexus/themes");
        let app = AppPaths::from_theme_paths(&paths);

        assert_eq!(app.root, PathBuf::from("/tmp/nexus"));
        assert_eq!(app.session_file, PathBuf::from("/tmp/nexus/session.toml"));
        assert_eq!(app.recent_file, PathBuf::from("/tmp/nexus/recent.toml"));
        assert_eq!(app.recovery_dir, PathBuf::from("/tmp/nexus/recovery"));
    }
}
