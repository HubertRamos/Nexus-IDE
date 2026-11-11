use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::color::Rgb;

pub const SCHEMA_VERSION: u32 = 1;
pub const CONFIG_SCHEMA_VERSION: u32 = 1;
pub const DEFAULT_THEME_ID: &str = "nexus-dark";

#[cfg(windows)]
const APP_DIR: &str = "Nexus";
#[cfg(not(windows))]
const APP_DIR: &str = "nexus";

const NEXUS_DARK: &str = include_str!("../themes/nexus-dark.toml");
const NEXUS_LIGHT: &str = include_str!("../themes/nexus-light.toml");
const NEXUS_HIGH_CONTRAST: &str = include_str!("../themes/nexus-high-contrast.toml");

const BUILTIN_THEMES: [(&str, &str); 3] = [
    ("nexus-dark", NEXUS_DARK),
    ("nexus-light", NEXUS_LIGHT),
    ("nexus-high-contrast", NEXUS_HIGH_CONTRAST),
];

/// Cores da interface.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UiTheme {
    pub background: Rgb,
    pub surface: Rgb,
    pub panel: Rgb,
    pub border: Rgb,
    pub foreground: Rgb,
    pub muted: Rgb,
    pub accent: Rgb,
    pub cursor: Rgb,
    pub cursor_text: Rgb,
    pub selection: Rgb,
    pub selection_text: Rgb,
    pub current_line: Rgb,
    pub status_bar: Rgb,
    pub status_bar_text: Rgb,
    pub success: Rgb,
    pub error: Rgb,
    pub warning: Rgb,
    pub dialog: Rgb,
}

/// Cores da sintaxe.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SyntaxTheme {
    pub keyword: Rgb,
    pub string: Rgb,
    pub comment: Rgb,
    pub number: Rgb,
    pub function: Rgb,
    pub method: Rgb,
    #[serde(rename = "type")]
    pub type_name: Rgb,
    pub variable: Rgb,
    pub constant: Rgb,
    pub operator: Rgb,
    pub attribute: Rgb,
}

/// Tema completo, com metadados e seções opcionais.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Theme {
    pub id: String,
    pub name: String,
    pub author: String,
    pub version: String,
    pub description: String,
    pub ui: Option<UiTheme>,
    pub syntax: Option<SyntaxTheme>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ThemeFile {
    schema_version: u32,
    name: String,
    #[serde(default)]
    author: String,
    #[serde(default)]
    version: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    ui: Option<UiTheme>,
    #[serde(default)]
    syntax: Option<SyntaxTheme>,
}

impl Theme {
    fn from_toml(id: &str, source: &str) -> Result<Self, ThemeError> {
        let file: ThemeFile = toml::from_str(source).map_err(|error| ThemeError::InvalidToml {
            id: id.to_owned(),
            message: error.to_string(),
        })?;

        if file.schema_version != SCHEMA_VERSION {
            return Err(ThemeError::UnsupportedSchema {
                id: id.to_owned(),
                found: file.schema_version,
                supported: SCHEMA_VERSION,
            });
        }

        if file.name.trim().is_empty() {
            return Err(ThemeError::EmptyName { id: id.to_owned() });
        }

        if file.ui.is_none() && file.syntax.is_none() {
            return Err(ThemeError::MissingSections { id: id.to_owned() });
        }

        Ok(Self {
            id: normalize_id(id),
            name: file.name,
            author: file.author,
            version: file.version,
            description: file.description,
            ui: file.ui,
            syntax: file.syntax,
        })
    }
}

/// Catálogo de temas internos e externos.
#[derive(Debug, Clone)]
pub struct ThemeCatalog {
    themes: BTreeMap<String, Theme>,
}

impl Default for ThemeCatalog {
    fn default() -> Self {
        Self::with_builtins()
    }
}

impl ThemeCatalog {
    /// Cria o catálogo com os três temas internos.
    ///
    /// Os temas internos são embutidos no binário e validados pelos testes;
    /// falhas aqui indicam um erro de programação, não configuração do usuário.
    pub fn with_builtins() -> Self {
        let mut catalog = Self {
            themes: BTreeMap::new(),
        };

        for (id, source) in BUILTIN_THEMES {
            catalog
                .add_toml(id, source)
                .expect("tema interno da Nexus deve ser válido");
        }

        catalog
    }

    /// Adiciona um tema a partir de TOML. O identificador é normalizado para
    /// minúsculas e não pode colidir com um tema existente.
    pub fn add_toml(&mut self, id: &str, source: &str) -> Result<(), ThemeError> {
        let id = normalize_id(id);
        if id.is_empty() {
            return Err(ThemeError::EmptyId);
        }
        if self.themes.contains_key(&id) {
            return Err(ThemeError::DuplicateId { id });
        }

        let theme = Theme::from_toml(&id, source)?;
        self.themes.insert(id, theme);
        Ok(())
    }

    /// Carrega todos os arquivos `.toml` de um diretório.
    ///
    /// Temas inválidos ou com identificadores reservados são ignorados com
    /// aviso; um arquivo problemático não impede o carregamento dos demais.
    pub fn load_dir(&mut self, directory: &Path) -> Vec<ThemeWarning> {
        let mut warnings = Vec::new();
        let entries = match fs::read_dir(directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return warnings,
            Err(error) => {
                warnings.push(ThemeWarning::new(format!(
                    "não foi possível ler o diretório de temas `{}`: {error}",
                    directory.display()
                )));
                return warnings;
            }
        };

        let mut paths: Vec<PathBuf> = entries
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "toml")
            })
            .collect();
        paths.sort();

        for path in paths {
            let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
                warnings.push(ThemeWarning::new(format!(
                    "tema ignorado por nome de arquivo inválido: `{}`",
                    path.display()
                )));
                continue;
            };

            let source = match fs::read_to_string(&path) {
                Ok(source) => source,
                Err(error) => {
                    warnings.push(ThemeWarning::new(format!(
                        "não foi possível ler o tema `{}`: {error}",
                        path.display()
                    )));
                    continue;
                }
            };

            if let Err(error) = self.add_toml(stem, &source) {
                warnings.push(ThemeWarning::new(format!(
                    "tema `{}` ignorado: {error}",
                    path.display()
                )));
            }
        }

        warnings
    }

    pub fn get(&self, id: &str) -> Option<&Theme> {
        self.themes.get(&normalize_id(id))
    }

    pub fn ids(&self) -> impl Iterator<Item = &str> {
        self.themes.keys().map(String::as_str)
    }

    /// Resolve a seleção de interface e sintaxe, aplicando fallback e
    /// produzindo avisos legíveis quando algo estiver ausente ou inválido.
    pub fn resolve(&self, config: &ThemeConfig) -> ResolvedThemes {
        let mut warnings = Vec::new();
        let (ui, ui_id) = self.resolve_ui(&config.ui, &mut warnings);
        let (syntax, syntax_id) = self.resolve_syntax(&config.syntax, &mut warnings);

        ResolvedThemes {
            ui,
            syntax,
            ui_id,
            syntax_id,
            warnings,
        }
    }

    fn resolve_ui(&self, requested: &str, warnings: &mut Vec<ThemeWarning>) -> (UiTheme, String) {
        let requested = requested_or_default(requested);
        match self.get(requested) {
            Some(theme) => match &theme.ui {
                Some(ui) => (ui.clone(), theme.id.clone()),
                None => {
                    warnings.push(ThemeWarning::new(format!(
                        "tema de interface `{}` não define a seção [ui]; usando {DEFAULT_THEME_ID}",
                        theme.id
                    )));
                    (self.default_ui(), DEFAULT_THEME_ID.to_owned())
                }
            },
            None => {
                warnings.push(ThemeWarning::new(format!(
                    "tema de interface `{requested}` não encontrado; usando {DEFAULT_THEME_ID}"
                )));
                (self.default_ui(), DEFAULT_THEME_ID.to_owned())
            }
        }
    }

    fn resolve_syntax(
        &self,
        requested: &str,
        warnings: &mut Vec<ThemeWarning>,
    ) -> (SyntaxTheme, String) {
        let requested = requested_or_default(requested);
        match self.get(requested) {
            Some(theme) => match &theme.syntax {
                Some(syntax) => (syntax.clone(), theme.id.clone()),
                None => {
                    warnings.push(ThemeWarning::new(format!(
                        "tema de sintaxe `{}` não define a seção [syntax]; usando {DEFAULT_THEME_ID}",
                        theme.id
                    )));
                    (self.default_syntax(), DEFAULT_THEME_ID.to_owned())
                }
            },
            None => {
                warnings.push(ThemeWarning::new(format!(
                    "tema de sintaxe `{requested}` não encontrado; usando {DEFAULT_THEME_ID}"
                )));
                (self.default_syntax(), DEFAULT_THEME_ID.to_owned())
            }
        }
    }

    fn default_ui(&self) -> UiTheme {
        self.get(DEFAULT_THEME_ID)
            .and_then(|theme| theme.ui.clone())
            .expect("tema interno nexus-dark deve existir")
    }

    fn default_syntax(&self) -> SyntaxTheme {
        self.get(DEFAULT_THEME_ID)
            .and_then(|theme| theme.syntax.clone())
            .expect("tema interno nexus-dark deve existir")
    }
}

/// Seleção persistida em `config.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ThemeConfig {
    #[serde(default = "default_ui_id")]
    pub ui: String,
    #[serde(default = "default_syntax_id")]
    pub syntax: String,
}

impl Default for ThemeConfig {
    fn default() -> Self {
        Self {
            ui: default_ui_id(),
            syntax: default_syntax_id(),
        }
    }
}

/// Resultado da resolução de uma seleção de temas.
#[derive(Debug, Clone)]
pub struct ResolvedThemes {
    pub ui: UiTheme,
    pub syntax: SyntaxTheme,
    pub ui_id: String,
    pub syntax_id: String,
    pub warnings: Vec<ThemeWarning>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThemeWarning {
    pub message: String,
}

impl ThemeWarning {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for ThemeWarning {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThemeError {
    EmptyId,
    DuplicateId {
        id: String,
    },
    EmptyName {
        id: String,
    },
    MissingSections {
        id: String,
    },
    UnsupportedSchema {
        id: String,
        found: u32,
        supported: u32,
    },
    InvalidToml {
        id: String,
        message: String,
    },
    Io {
        path: PathBuf,
        message: String,
    },
}

impl fmt::Display for ThemeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ThemeError::EmptyId => write!(formatter, "identificador de tema vazio"),
            ThemeError::DuplicateId { id } => {
                write!(formatter, "já existe um tema com o identificador `{id}`")
            }
            ThemeError::EmptyName { id } => {
                write!(formatter, "o tema `{id}` possui nome vazio")
            }
            ThemeError::MissingSections { id } => write!(
                formatter,
                "o tema `{id}` precisa definir [ui], [syntax] ou ambos"
            ),
            ThemeError::UnsupportedSchema {
                id,
                found,
                supported,
            } => write!(
                formatter,
                "o tema `{id}` usa schema_version {found}; esta versão suporta {supported}"
            ),
            ThemeError::InvalidToml { id, message } => {
                write!(formatter, "TOML inválido no tema `{id}`: {message}")
            }
            ThemeError::Io { path, message } => {
                write!(
                    formatter,
                    "falha de arquivo em `{}`: {message}",
                    path.display()
                )
            }
        }
    }
}

impl std::error::Error for ThemeError {}

/// Localização dos arquivos de configuração e temas do usuário.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThemePaths {
    pub config_file: PathBuf,
    pub themes_dir: PathBuf,
}

impl ThemePaths {
    pub fn new(config_file: impl Into<PathBuf>, themes_dir: impl Into<PathBuf>) -> Self {
        Self {
            config_file: config_file.into(),
            themes_dir: themes_dir.into(),
        }
    }
}

/// Retorna os caminhos conforme XDG Base Directory (Linux) ou diretório de
/// configuração do usuário (Windows/macOS).
pub fn default_paths() -> Option<ThemePaths> {
    dirs::config_dir().map(|base| {
        let root = base.join(APP_DIR);
        ThemePaths::new(root.join("config.toml"), root.join("themes"))
    })
}

/// Temas carregados e resolvidos a partir do ambiente.
#[derive(Debug, Clone)]
pub struct LoadedThemes {
    pub ui: UiTheme,
    pub syntax: SyntaxTheme,
    pub ui_id: String,
    pub syntax_id: String,
    pub warnings: Vec<ThemeWarning>,
    pub paths: Option<ThemePaths>,
}

impl LoadedThemes {
    pub fn warning_summary(&self) -> Option<String> {
        if self.warnings.is_empty() {
            None
        } else {
            Some(
                self.warnings
                    .iter()
                    .map(ThemeWarning::to_string)
                    .collect::<Vec<_>>()
                    .join("; "),
            )
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ConfigFile {
    #[serde(default = "default_schema_version")]
    schema_version: u32,
    #[serde(default)]
    theme: ThemeConfig,
}

/// Carrega temas internos, temas externos e a seleção persistida.
pub fn load_from_paths(paths: &ThemePaths) -> LoadedThemes {
    let mut catalog = ThemeCatalog::with_builtins();
    let mut warnings = catalog.load_dir(&paths.themes_dir);
    let config = load_config(&paths.config_file, &mut warnings);
    let resolved = catalog.resolve(&config);
    warnings.extend(resolved.warnings);

    LoadedThemes {
        ui: resolved.ui,
        syntax: resolved.syntax,
        ui_id: resolved.ui_id,
        syntax_id: resolved.syntax_id,
        warnings,
        paths: Some(paths.clone()),
    }
}

/// Carrega a configuração do usuário a partir das convenções do sistema.
pub fn load_from_environment() -> LoadedThemes {
    match default_paths() {
        Some(paths) => load_from_paths(&paths),
        None => {
            let resolved = ThemeCatalog::with_builtins().resolve(&ThemeConfig::default());
            LoadedThemes {
                ui: resolved.ui,
                syntax: resolved.syntax,
                ui_id: resolved.ui_id,
                syntax_id: resolved.syntax_id,
                warnings: resolved.warnings,
                paths: None,
            }
        }
    }
}

fn load_config(path: &Path, warnings: &mut Vec<ThemeWarning>) -> ThemeConfig {
    let source = match fs::read_to_string(path) {
        Ok(source) => source,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return ThemeConfig::default(),
        Err(error) => {
            warnings.push(ThemeWarning::new(format!(
                "não foi possível ler a configuração `{}`: {error}; usando temas padrão",
                path.display()
            )));
            return ThemeConfig::default();
        }
    };

    match toml::from_str::<ConfigFile>(&source) {
        Ok(config) if config.schema_version == CONFIG_SCHEMA_VERSION => config.theme,
        Ok(config) => {
            warnings.push(ThemeWarning::new(format!(
                "configuração usa schema_version {}; esta versão suporta {}; usando temas padrão",
                config.schema_version, CONFIG_SCHEMA_VERSION
            )));
            ThemeConfig::default()
        }
        Err(error) => {
            warnings.push(ThemeWarning::new(format!(
                "configuração inválida em `{}`: {error}; usando temas padrão",
                path.display()
            )));
            ThemeConfig::default()
        }
    }
}

/// Grava a seleção de temas para a próxima execução.
///
/// A escrita é simples e atômica o suficiente para um arquivo de configuração
/// pequeno; a interface ainda não oferece editor visual de temas.
pub fn save_config(paths: &ThemePaths, config: &ThemeConfig) -> Result<(), ThemeError> {
    let file = ConfigFile {
        schema_version: CONFIG_SCHEMA_VERSION,
        theme: config.clone(),
    };
    let source = toml::to_string_pretty(&file).map_err(|error| ThemeError::InvalidToml {
        id: "config".to_owned(),
        message: error.to_string(),
    })?;

    if let Some(parent) = paths.config_file.parent() {
        fs::create_dir_all(parent).map_err(|error| ThemeError::Io {
            path: parent.to_path_buf(),
            message: error.to_string(),
        })?;
    }

    fs::write(&paths.config_file, source).map_err(|error| ThemeError::Io {
        path: paths.config_file.clone(),
        message: error.to_string(),
    })
}

fn normalize_id(id: &str) -> String {
    id.trim().to_ascii_lowercase()
}

fn requested_or_default(requested: &str) -> &str {
    let requested = requested.trim();
    if requested.is_empty() {
        DEFAULT_THEME_ID
    } else {
        requested
    }
}

fn default_schema_version() -> u32 {
    CONFIG_SCHEMA_VERSION
}

fn default_ui_id() -> String {
    DEFAULT_THEME_ID.to_owned()
}

fn default_syntax_id() -> String {
    DEFAULT_THEME_ID.to_owned()
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    const EXTERNAL_SYNTAX_THEME: &str = r##"
schema_version = 1
name = "Tema externo"
author = "Teste"
version = "1.0.0"
[syntax]
keyword = "#ff0000"
string = "#00ff00"
comment = "#808080"
number = "#ff8000"
function = "#00ffff"
method = "#00ffff"
type = "#ffff00"
variable = "#ffffff"
constant = "#ff8000"
operator = "#00ffff"
attribute = "#ff00ff"
"##;

    struct TestDir {
        path: PathBuf,
    }

    impl TestDir {
        fn new(label: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "nexus-theme-test-{}-{label}-{unique}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).expect("diretório de teste");
            Self { path }
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn builtins_are_loaded_and_distinct() {
        let catalog = ThemeCatalog::with_builtins();
        let dark = catalog.get("nexus-dark").unwrap();
        let light = catalog.get("nexus-light").unwrap();
        let contrast = catalog.get("nexus-high-contrast").unwrap();

        assert!(dark.ui.is_some() && dark.syntax.is_some());
        assert!(light.ui.is_some() && light.syntax.is_some());
        assert!(contrast.ui.is_some() && contrast.syntax.is_some());
        assert_ne!(dark.ui, light.ui);
        assert_ne!(light.ui, contrast.ui);
        assert_ne!(dark.syntax, light.syntax);
    }

    #[test]
    fn builtin_contrast_is_usable() {
        for id in ["nexus-dark", "nexus-light", "nexus-high-contrast"] {
            let theme = ThemeCatalog::with_builtins();
            let theme = theme.get(id).unwrap();
            let ui = theme.ui.as_ref().unwrap();
            assert!(
                ui.foreground.contrast_ratio(ui.background) >= 4.5,
                "{id} deve ter contraste de texto >= 4.5"
            );
        }
    }

    #[test]
    fn resolves_independent_ui_and_syntax() {
        let catalog = ThemeCatalog::with_builtins();
        let config = ThemeConfig {
            ui: "nexus-light".to_owned(),
            syntax: "nexus-dark".to_owned(),
        };

        let resolved = catalog.resolve(&config);

        assert_eq!(resolved.ui_id, "nexus-light");
        assert_eq!(resolved.syntax_id, "nexus-dark");
        assert_eq!(
            resolved.ui,
            catalog.get("nexus-light").unwrap().ui.clone().unwrap()
        );
        assert_eq!(
            resolved.syntax,
            catalog.get("nexus-dark").unwrap().syntax.clone().unwrap()
        );
        assert!(resolved.warnings.is_empty());
    }

    #[test]
    fn missing_theme_falls_back_with_warning() {
        let catalog = ThemeCatalog::with_builtins();
        let config = ThemeConfig {
            ui: "nao-existe".to_owned(),
            syntax: "tambem-nao".to_owned(),
        };

        let resolved = catalog.resolve(&config);

        assert_eq!(resolved.ui_id, DEFAULT_THEME_ID);
        assert_eq!(resolved.syntax_id, DEFAULT_THEME_ID);
        assert_eq!(resolved.warnings.len(), 2);
    }

    #[test]
    fn invalid_toml_is_rejected() {
        let mut catalog = ThemeCatalog::with_builtins();
        let error = catalog
            .add_toml("quebrado", "schema_version = 1\nname = [\n")
            .unwrap_err();

        assert!(matches!(error, ThemeError::InvalidToml { .. }));
    }

    #[test]
    fn unknown_fields_and_invalid_colors_are_rejected() {
        let mut catalog = ThemeCatalog::with_builtins();
        let unknown = r##"
schema_version = 1
name = "Teste"
[ui]
background = "#000000"
surface = "#000000"
panel = "#000000"
border = "#000000"
foreground = "#ffffff"
muted = "#808080"
accent = "#00ffff"
cursor = "#ffffff"
cursor_text = "#000000"
selection = "#0000ff"
selection_text = "#ffffff"
current_line = "#111111"
status_bar = "#000000"
status_bar_text = "#ffffff"
success = "#00ff00"
error = "#ff0000"
warning = "#ffff00"
dialog = "#111111"
extra = "#ffffff"
"##;
        assert!(catalog.add_toml("unknown", unknown).is_err());

        let invalid = unknown
            .replace("extra = \"#ffffff\"", "")
            .replace("#000000", "#00000");
        assert!(catalog.add_toml("invalid-color", &invalid).is_err());
    }

    #[test]
    fn unsupported_schema_is_rejected() {
        let mut catalog = ThemeCatalog::with_builtins();
        let error = catalog
            .add_toml(
                "futuro",
                r##"
schema_version = 99
name = "Futuro"
[syntax]
keyword = "#ffffff"
string = "#ffffff"
comment = "#ffffff"
number = "#ffffff"
function = "#ffffff"
method = "#ffffff"
type = "#ffffff"
variable = "#ffffff"
constant = "#ffffff"
operator = "#ffffff"
attribute = "#ffffff"
"##,
            )
            .unwrap_err();

        assert!(matches!(error, ThemeError::UnsupportedSchema { .. }));
    }

    #[test]
    fn partial_theme_falls_back_for_missing_section() {
        let mut catalog = ThemeCatalog::with_builtins();
        catalog
            .add_toml(
                "so-sintaxe",
                r##"
schema_version = 1
name = "Só sintaxe"
[syntax]
keyword = "#ff00ff"
string = "#00ff00"
comment = "#808080"
number = "#ff8000"
function = "#00ffff"
method = "#00ffff"
type = "#ffff00"
variable = "#ffffff"
constant = "#ff8000"
operator = "#00ffff"
attribute = "#ff00ff"
"##,
            )
            .unwrap();

        let resolved = catalog.resolve(&ThemeConfig {
            ui: "so-sintaxe".to_owned(),
            syntax: "so-sintaxe".to_owned(),
        });

        assert_eq!(resolved.ui_id, DEFAULT_THEME_ID);
        assert_eq!(resolved.syntax_id, "so-sintaxe");
        assert_eq!(resolved.warnings.len(), 1);
    }

    #[test]
    fn load_dir_loads_valid_themes_and_warns_about_invalid_files() {
        let dir = TestDir::new("load-dir");
        fs::write(dir.path.join("externa.toml"), EXTERNAL_SYNTAX_THEME).unwrap();
        fs::write(
            dir.path.join("quebrado.toml"),
            "schema_version = 1\nname = [\n",
        )
        .unwrap();
        fs::write(dir.path.join("ignorado.txt"), EXTERNAL_SYNTAX_THEME).unwrap();

        let mut catalog = ThemeCatalog::with_builtins();
        let warnings = catalog.load_dir(&dir.path);

        assert!(catalog.get("externa").is_some());
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].message.contains("quebrado.toml"));

        let resolved = catalog.resolve(&ThemeConfig {
            ui: DEFAULT_THEME_ID.to_owned(),
            syntax: "externa".to_owned(),
        });
        assert_eq!(resolved.syntax_id, "externa");
        assert_eq!(resolved.syntax.keyword, Rgb::new(255, 0, 0));
        assert!(resolved.warnings.is_empty());
    }

    #[test]
    fn load_dir_missing_directory_is_not_an_error() {
        let dir = TestDir::new("missing-dir");
        let missing = dir.path.join("nao-existe");
        let mut catalog = ThemeCatalog::with_builtins();

        assert!(catalog.load_dir(&missing).is_empty());
    }

    #[test]
    fn duplicate_external_theme_is_ignored_with_warning() {
        let dir = TestDir::new("duplicate");
        fs::write(dir.path.join("nexus-dark.toml"), EXTERNAL_SYNTAX_THEME).unwrap();

        let mut catalog = ThemeCatalog::with_builtins();
        let warnings = catalog.load_dir(&dir.path);

        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].message.contains("nexus-dark"));
        assert_eq!(
            catalog.get("nexus-dark").unwrap().name,
            "Nexus Dark",
            "tema interno não deve ser sobrescrito"
        );
    }

    #[test]
    fn config_round_trip_persists_ui_and_syntax_selection() {
        let dir = TestDir::new("config-round-trip");
        let paths = ThemePaths::new(dir.path.join("config.toml"), dir.path.join("themes"));
        let config = ThemeConfig {
            ui: "nexus-light".to_owned(),
            syntax: "nexus-high-contrast".to_owned(),
        };

        save_config(&paths, &config).unwrap();
        let loaded = load_from_paths(&paths);

        assert_eq!(loaded.ui_id, "nexus-light");
        assert_eq!(loaded.syntax_id, "nexus-high-contrast");
        assert!(loaded.warnings.is_empty());
        assert_eq!(
            loaded.ui,
            ThemeCatalog::with_builtins()
                .get("nexus-light")
                .unwrap()
                .ui
                .clone()
                .unwrap()
        );
    }

    #[test]
    fn malformed_config_falls_back_with_warning() {
        let dir = TestDir::new("malformed-config");
        let paths = ThemePaths::new(dir.path.join("config.toml"), dir.path.join("themes"));
        fs::write(&paths.config_file, "schema_version = [\n").unwrap();

        let loaded = load_from_paths(&paths);

        assert_eq!(loaded.ui_id, DEFAULT_THEME_ID);
        assert_eq!(loaded.syntax_id, DEFAULT_THEME_ID);
        assert_eq!(loaded.warnings.len(), 1);
        assert!(loaded.warnings[0].message.contains("configuração inválida"));
    }

    #[test]
    fn config_with_unknown_themes_falls_back_with_warnings() {
        let dir = TestDir::new("unknown-config");
        let paths = ThemePaths::new(dir.path.join("config.toml"), dir.path.join("themes"));
        fs::write(
            &paths.config_file,
            "schema_version = 1\n[theme]\nui = \"nao-existe\"\nsyntax = \"tambem-nao\"\n",
        )
        .unwrap();

        let loaded = load_from_paths(&paths);

        assert_eq!(loaded.ui_id, DEFAULT_THEME_ID);
        assert_eq!(loaded.syntax_id, DEFAULT_THEME_ID);
        assert_eq!(loaded.warnings.len(), 2);
        assert!(
            loaded
                .warnings
                .iter()
                .all(|warning| warning.message.contains("não encontrado"))
        );
    }

    #[test]
    fn unsupported_config_schema_falls_back_with_warning() {
        let dir = TestDir::new("config-schema");
        let paths = ThemePaths::new(dir.path.join("config.toml"), dir.path.join("themes"));
        fs::write(
            &paths.config_file,
            "schema_version = 99\n[theme]\nui = \"nexus-light\"\nsyntax = \"nexus-light\"\n",
        )
        .unwrap();

        let loaded = load_from_paths(&paths);

        assert_eq!(loaded.ui_id, DEFAULT_THEME_ID);
        assert_eq!(loaded.syntax_id, DEFAULT_THEME_ID);
        assert_eq!(loaded.warnings.len(), 1);
        assert!(loaded.warnings[0].message.contains("schema_version"));
    }

    #[test]
    fn config_defaults_apply_when_section_is_absent() {
        let dir = TestDir::new("config-defaults");
        let paths = ThemePaths::new(dir.path.join("config.toml"), dir.path.join("themes"));
        fs::write(&paths.config_file, "schema_version = 1\n").unwrap();

        let loaded = load_from_paths(&paths);

        assert_eq!(loaded.ui_id, DEFAULT_THEME_ID);
        assert_eq!(loaded.syntax_id, DEFAULT_THEME_ID);
        assert!(loaded.warnings.is_empty());
    }

    #[test]
    fn save_config_creates_missing_directories() {
        let dir = TestDir::new("config-create-dir");
        let paths = ThemePaths::new(
            dir.path.join("sub").join("config.toml"),
            dir.path.join("themes"),
        );

        save_config(&paths, &ThemeConfig::default()).unwrap();

        assert!(paths.config_file.is_file());
    }
}
