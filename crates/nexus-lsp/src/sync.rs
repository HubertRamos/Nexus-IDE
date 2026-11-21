//! Sincronização de documentos entre o editor e os servidores LSP.
//!
//! O estado de sincronização é mantido **por servidor**, não por aba. Um mesmo
//! [`DocumentSync`] pode acompanhar vários documentos abertos, identificados
//! pela URI, e fechar um deles remove apenas aquele documento. O servidor
//! permanece disponível para os demais.
//!
//! A escolha entre sincronização completa e incremental segue exatamente a
//! capacidade `textDocumentSync` anunciada no `initialize`. O módulo é puro:
//! ele produz notificações tipadas e não conhece processos, Tokio ou Ratatui.
//! O envio fica a cargo de [`crate::manager::LspServer`].

use std::collections::BTreeMap;
use std::fmt;

use serde_json::Value;

use crate::protocol::{
    DidChangeTextDocumentParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams,
    DidSaveTextDocumentParams, SaveOptions, ServerCapabilities, TextDocumentContentChangeEvent,
    TextDocumentIdentifier, TextDocumentItem, TextDocumentSaveCapability,
    TextDocumentSyncCapability, TextDocumentSyncKind, TextDocumentSyncOptions,
    VersionedTextDocumentIdentifier,
};
use crate::utf16::{self, Utf16Error};

/// Estratégia de salvamento anunciada pelo servidor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaveSync {
    /// O servidor não deseja receber `textDocument/didSave`.
    Disabled,
    /// O servidor deseja `didSave`, opcionalmente com o texto completo.
    Enabled { include_text: bool },
}

/// Capacidades de sincronização efetivamente aplicáveis a um servidor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncSettings {
    pub change: TextDocumentSyncKind,
    pub open_close: bool,
    pub save: SaveSync,
}

impl Default for SyncSettings {
    fn default() -> Self {
        Self::disabled()
    }
}

impl SyncSettings {
    /// Configuração usada quando o servidor não anuncia sincronização.
    pub const fn disabled() -> Self {
        Self {
            change: TextDocumentSyncKind::None,
            open_close: false,
            save: SaveSync::Disabled,
        }
    }

    /// Extrai a configuração de sincronização das capacidades do servidor.
    pub fn from_capabilities(capabilities: &ServerCapabilities) -> Self {
        match &capabilities.text_document_sync {
            None => Self::disabled(),
            Some(TextDocumentSyncCapability::Kind(kind)) => {
                if *kind == TextDocumentSyncKind::None {
                    Self::disabled()
                } else {
                    // A forma numérica é a variante legada e não carrega
                    // `openClose` nem `save`; servidores que a usam esperam
                    // abertura e fechamento de documentos.
                    Self {
                        change: *kind,
                        open_close: true,
                        save: SaveSync::Disabled,
                    }
                }
            }
            Some(TextDocumentSyncCapability::Options(options)) => Self::from_options(options),
        }
    }

    fn from_options(options: &TextDocumentSyncOptions) -> Self {
        let change = options.change.unwrap_or(TextDocumentSyncKind::None);
        let open_close = options.open_close.unwrap_or(false);
        // `save: false` significa que o servidor não quer `didSave`. Já
        // `save: true` apenas habilita a notificação: o texto só é incluído
        // quando a forma objeto declara `includeText: true`. O booleano `true`
        // não pode ser confundido com `includeText`.
        let save = match &options.save {
            None | Some(TextDocumentSaveCapability::IncludeText(false)) => SaveSync::Disabled,
            Some(TextDocumentSaveCapability::IncludeText(true)) => SaveSync::Enabled {
                include_text: false,
            },
            Some(TextDocumentSaveCapability::Options(SaveOptions { include_text })) => {
                SaveSync::Enabled {
                    include_text: include_text.unwrap_or(false),
                }
            }
        };

        Self {
            change,
            open_close,
            save,
        }
    }

    /// Indica se o servidor precisa acompanhar documentos abertos.
    pub fn tracks_documents(&self) -> bool {
        self.change != TextDocumentSyncKind::None
            || self.open_close
            || self.save != SaveSync::Disabled
    }
}

/// Alteração textual em offsets de caracteres do Nexus.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextChange {
    /// Offset do primeiro caractere removido/substituído.
    pub start: usize,
    /// Offset imediatamente após o último caractere removido/substituído.
    pub end: usize,
    /// Texto inserido no intervalo.
    pub text: String,
}

impl TextChange {
    pub fn new(start: usize, end: usize, text: impl Into<String>) -> Self {
        Self {
            start,
            end,
            text: text.into(),
        }
    }
}

/// Documento acompanhado por um servidor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenDocument {
    pub uri: String,
    pub language_id: String,
    pub version: i64,
    /// Último conteúdo conhecido pelo servidor. Serve de base para os deltas.
    pub text: String,
}

/// Notificação de sincronização pronta para ser enviada pelo transporte.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncNotification {
    DidOpen(DidOpenTextDocumentParams),
    DidChange(DidChangeTextDocumentParams),
    DidSave(DidSaveTextDocumentParams),
    DidClose(DidCloseTextDocumentParams),
}

impl SyncNotification {
    /// Método LSP correspondente à notificação.
    pub fn method(&self) -> &'static str {
        match self {
            Self::DidOpen(_) => "textDocument/didOpen",
            Self::DidChange(_) => "textDocument/didChange",
            Self::DidSave(_) => "textDocument/didSave",
            Self::DidClose(_) => "textDocument/didClose",
        }
    }

    /// Serializa os parâmetros da notificação.
    pub fn to_params(&self) -> serde_json::Result<Value> {
        match self {
            Self::DidOpen(params) => serde_json::to_value(params),
            Self::DidChange(params) => serde_json::to_value(params),
            Self::DidSave(params) => serde_json::to_value(params),
            Self::DidClose(params) => serde_json::to_value(params),
        }
    }
}

/// Falhas ao atualizar o estado de sincronização.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncError {
    /// A operação exige um documento aberto no servidor.
    NotOpen { uri: String },
    /// O documento já estava aberto.
    AlreadyOpen { uri: String },
    /// A alteração usa offsets inválidos para o conteúdo atual.
    InvalidChange {
        start: usize,
        end: usize,
        len: usize,
    },
    /// A versão do documento não pode mais ser incrementada.
    VersionOverflow { uri: String },
    /// Falha na conversão de posições UTF-16.
    Utf16(Utf16Error),
}

impl fmt::Display for SyncError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotOpen { uri } => {
                write!(formatter, "documento não está aberto no servidor: {uri}")
            }
            Self::AlreadyOpen { uri } => {
                write!(formatter, "documento já está aberto no servidor: {uri}")
            }
            Self::InvalidChange { start, end, len } => write!(
                formatter,
                "alteração inválida: intervalo {start}..{end} para conteúdo com {len} caracteres"
            ),
            Self::VersionOverflow { uri } => {
                write!(formatter, "versão esgotada para o documento {uri}")
            }
            Self::Utf16(error) => write!(formatter, "posição LSP inválida: {error}"),
        }
    }
}

impl std::error::Error for SyncError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Utf16(error) => Some(error),
            _ => None,
        }
    }
}

impl From<Utf16Error> for SyncError {
    fn from(error: Utf16Error) -> Self {
        Self::Utf16(error)
    }
}

/// Estado de sincronização de um único servidor LSP.
#[derive(Debug, Clone)]
pub struct DocumentSync {
    settings: SyncSettings,
    documents: BTreeMap<String, OpenDocument>,
}

impl DocumentSync {
    pub fn new(settings: SyncSettings) -> Self {
        Self {
            settings,
            documents: BTreeMap::new(),
        }
    }

    pub fn disabled() -> Self {
        Self::new(SyncSettings::disabled())
    }

    pub fn from_capabilities(capabilities: &ServerCapabilities) -> Self {
        Self::new(SyncSettings::from_capabilities(capabilities))
    }

    pub fn settings(&self) -> SyncSettings {
        self.settings
    }

    pub fn is_empty(&self) -> bool {
        self.documents.is_empty()
    }

    pub fn len(&self) -> usize {
        self.documents.len()
    }

    /// Esquece todos os documentos sem emitir notificações.
    ///
    /// Usado quando o transporte falha e o estado do servidor deixa de ser
    /// confiável. Uma futura sessão precisa reabrir os documentos com
    /// `didOpen`, em vez de tentar continuar a partir de versões antigas.
    pub fn clear(&mut self) {
        self.documents.clear();
    }

    pub fn is_open(&self, uri: &str) -> bool {
        self.documents.contains_key(uri)
    }

    pub fn document(&self, uri: &str) -> Option<&OpenDocument> {
        self.documents.get(uri)
    }

    pub fn documents(&self) -> impl Iterator<Item = &OpenDocument> {
        self.documents.values()
    }

    /// Registra a abertura de um documento e produz `didOpen` quando aplicável.
    pub fn open(
        &mut self,
        uri: impl Into<String>,
        language_id: impl Into<String>,
        text: impl Into<String>,
    ) -> Result<Option<SyncNotification>, SyncError> {
        let settings = self.settings;
        if !settings.tracks_documents() {
            return Ok(None);
        }

        let uri = uri.into();
        if self.documents.contains_key(&uri) {
            return Err(SyncError::AlreadyOpen { uri });
        }

        let language_id = language_id.into();
        let text = text.into();
        let document = OpenDocument {
            uri: uri.clone(),
            language_id: language_id.clone(),
            version: 1,
            text: text.clone(),
        };
        self.documents.insert(uri.clone(), document);

        if settings.open_close {
            Ok(Some(SyncNotification::DidOpen(DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri,
                    language_id,
                    version: 1,
                    text,
                },
            })))
        } else {
            Ok(None)
        }
    }

    /// Registra uma transação com uma ou mais edições.
    ///
    /// `changes` estão na ordem em que foram aplicadas e usam offsets relativos
    /// ao conteúdo imediatamente antes de cada edição. `current_text` é o
    /// conteúdo final do buffer. Se o resultado dos deltas divergir do buffer,
    /// a notificação degrada para uma substituição integral, evitando que o
    /// servidor fique com um estado corrompido.
    pub fn change(
        &mut self,
        uri: &str,
        changes: &[TextChange],
        current_text: &str,
    ) -> Result<Option<SyncNotification>, SyncError> {
        let settings = self.settings;
        if settings.change == TextDocumentSyncKind::None {
            return Ok(None);
        }

        if changes.is_empty() {
            return self.resync(uri, current_text);
        }

        let old_text = self
            .documents
            .get(uri)
            .ok_or_else(|| SyncError::NotOpen {
                uri: uri.to_owned(),
            })?
            .text
            .clone();

        let mut shadow = old_text;
        let mut content_changes = Vec::with_capacity(changes.len());
        let mut fallback_to_full = false;

        for change in changes {
            let len = shadow.chars().count();
            if change.start > change.end || change.end > len {
                return Err(SyncError::InvalidChange {
                    start: change.start,
                    end: change.end,
                    len,
                });
            }

            // Um range que cai entre `\r` e `\n` não é representável no LSP.
            // Nesse caso a transação inteira degrada para substituição
            // integral, em vez de enviar posições ambíguas ao servidor.
            match utf16::range_from_offsets(&shadow, change.start, change.end) {
                Ok(range) => {
                    if !fallback_to_full {
                        content_changes.push(TextDocumentContentChangeEvent {
                            range: Some(range),
                            range_length: None,
                            text: change.text.clone(),
                        });
                    }
                }
                Err(_) => {
                    fallback_to_full = true;
                    content_changes.clear();
                }
            }
            replace_chars(&mut shadow, change.start, change.end, &change.text);
        }

        if fallback_to_full || shadow != current_text {
            shadow = current_text.to_owned();
            content_changes = vec![full_content_change(&shadow)];
        }

        let document = self
            .documents
            .get_mut(uri)
            .expect("documento verificado antes das alterações");
        let version = next_version(document.version, uri)?;
        document.version = version;
        document.text = shadow;

        Ok(Some(SyncNotification::DidChange(
            DidChangeTextDocumentParams {
                text_document: VersionedTextDocumentIdentifier {
                    uri: uri.to_owned(),
                    version,
                },
                content_changes,
            },
        )))
    }

    /// Recalcula uma única alteração entre o conteúdo conhecido e `text`.
    ///
    /// É usado para Undo/Redo e recarregamentos que não produzem uma
    /// `Transaction`. No modo incremental, envia apenas o menor intervalo
    /// substituído; no modo completo, envia o texto inteiro.
    pub fn resync(&mut self, uri: &str, text: &str) -> Result<Option<SyncNotification>, SyncError> {
        let settings = self.settings;
        if settings.change == TextDocumentSyncKind::None {
            return Ok(None);
        }

        let old_text = self
            .documents
            .get(uri)
            .ok_or_else(|| SyncError::NotOpen {
                uri: uri.to_owned(),
            })?
            .text
            .clone();

        if old_text == text {
            return Ok(None);
        }

        let content_changes = match settings.change {
            TextDocumentSyncKind::None => return Ok(None),
            TextDocumentSyncKind::Full => vec![full_content_change(text)],
            TextDocumentSyncKind::Incremental => {
                let (start, end, replacement) = minimal_change(&old_text, text);
                match utf16::range_from_offsets(&old_text, start, end) {
                    Ok(range) => vec![TextDocumentContentChangeEvent {
                        range: Some(range),
                        range_length: None,
                        text: replacement,
                    }],
                    // O menor intervalo pode dividir um CRLF (por exemplo
                    // `a\r\nb` -> `a\nb`). Nenhuma posição LSP válida existe
                    // entre `\r` e `\n`, então a única degradação segura é
                    // enviar o documento inteiro.
                    Err(_) => vec![full_content_change(text)],
                }
            }
        };

        let document = self
            .documents
            .get_mut(uri)
            .expect("documento verificado antes do resync");
        let version = next_version(document.version, uri)?;
        document.version = version;
        document.text = text.to_owned();

        Ok(Some(SyncNotification::DidChange(
            DidChangeTextDocumentParams {
                text_document: VersionedTextDocumentIdentifier {
                    uri: uri.to_owned(),
                    version,
                },
                content_changes,
            },
        )))
    }

    /// Registra o salvamento e produz `didSave` quando o servidor o aceita.
    pub fn save(&mut self, uri: &str, text: &str) -> Result<Option<SyncNotification>, SyncError> {
        let SaveSync::Enabled { include_text } = self.settings.save else {
            return Ok(None);
        };

        let document = self
            .documents
            .get_mut(uri)
            .ok_or_else(|| SyncError::NotOpen {
                uri: uri.to_owned(),
            })?;
        document.text = text.to_owned();

        Ok(Some(SyncNotification::DidSave(DidSaveTextDocumentParams {
            text_document: TextDocumentIdentifier {
                uri: uri.to_owned(),
            },
            text: include_text.then(|| text.to_owned()),
        })))
    }

    /// Remove o documento e produz `didClose` quando aplicável.
    ///
    /// O servidor e os demais documentos não são afetados.
    pub fn close(&mut self, uri: &str) -> Result<Option<SyncNotification>, SyncError> {
        if self.documents.remove(uri).is_none() {
            return Ok(None);
        }

        if self.settings.open_close {
            Ok(Some(SyncNotification::DidClose(
                DidCloseTextDocumentParams {
                    text_document: TextDocumentIdentifier {
                        uri: uri.to_owned(),
                    },
                },
            )))
        } else {
            Ok(None)
        }
    }
}

fn full_content_change(text: &str) -> TextDocumentContentChangeEvent {
    TextDocumentContentChangeEvent {
        range: None,
        range_length: None,
        text: text.to_owned(),
    }
}

fn next_version(version: i64, uri: &str) -> Result<i64, SyncError> {
    version
        .checked_add(1)
        .ok_or_else(|| SyncError::VersionOverflow {
            uri: uri.to_owned(),
        })
}

fn replace_chars(text: &mut String, start: usize, end: usize, replacement: &str) {
    let byte_start = char_to_byte(text, start);
    let byte_end = char_to_byte(text, end);
    text.replace_range(byte_start..byte_end, replacement);
}

fn char_to_byte(text: &str, char_index: usize) -> usize {
    text.char_indices()
        .nth(char_index)
        .map(|(byte, _)| byte)
        .unwrap_or(text.len())
}

/// Calcula o menor intervalo único que transforma `old` em `new`.
///
/// Prefixo e sufixo comuns são preservados; o trecho restante é substituído.
fn minimal_change(old: &str, new: &str) -> (usize, usize, String) {
    let old_chars: Vec<char> = old.chars().collect();
    let new_chars: Vec<char> = new.chars().collect();

    let mut prefix = 0;
    while prefix < old_chars.len()
        && prefix < new_chars.len()
        && old_chars[prefix] == new_chars[prefix]
    {
        prefix += 1;
    }

    let mut suffix = 0;
    while suffix < old_chars.len() - prefix
        && suffix < new_chars.len() - prefix
        && old_chars[old_chars.len() - 1 - suffix] == new_chars[new_chars.len() - 1 - suffix]
    {
        suffix += 1;
    }

    let old_end = old_chars.len() - suffix;
    let new_end = new_chars.len() - suffix;
    let replacement: String = new_chars[prefix..new_end].iter().collect();

    (prefix, old_end, replacement)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn settings(change: TextDocumentSyncKind, open_close: bool, save: SaveSync) -> SyncSettings {
        SyncSettings {
            change,
            open_close,
            save,
        }
    }

    fn incremental() -> DocumentSync {
        DocumentSync::new(settings(
            TextDocumentSyncKind::Incremental,
            true,
            SaveSync::Enabled { include_text: true },
        ))
    }

    #[test]
    fn numeric_sync_kind_enables_open_close_but_not_save() {
        let capabilities = ServerCapabilities {
            text_document_sync: Some(TextDocumentSyncCapability::Kind(TextDocumentSyncKind::Full)),
            ..Default::default()
        };

        assert_eq!(
            SyncSettings::from_capabilities(&capabilities),
            settings(TextDocumentSyncKind::Full, true, SaveSync::Disabled)
        );
    }

    #[test]
    fn options_respect_open_close_and_save_fields() {
        let capabilities = ServerCapabilities {
            text_document_sync: Some(TextDocumentSyncCapability::Options(
                TextDocumentSyncOptions {
                    open_close: Some(false),
                    change: Some(TextDocumentSyncKind::Incremental),
                    save: Some(TextDocumentSaveCapability::Options(SaveOptions {
                        include_text: Some(true),
                    })),
                    ..Default::default()
                },
            )),
            ..Default::default()
        };

        assert_eq!(
            SyncSettings::from_capabilities(&capabilities),
            settings(
                TextDocumentSyncKind::Incremental,
                false,
                SaveSync::Enabled { include_text: true }
            )
        );
    }

    #[test]
    fn save_capability_formats_are_interpreted_correctly() {
        let settings_for = |save: Value| {
            let capabilities: ServerCapabilities =
                serde_json::from_value(json!({"textDocumentSync": {"change": 2, "save": save}}))
                    .expect("capacidades válidas");
            SyncSettings::from_capabilities(&capabilities).save
        };

        assert_eq!(settings_for(json!(false)), SaveSync::Disabled);
        assert_eq!(
            settings_for(json!(true)),
            SaveSync::Enabled {
                include_text: false
            }
        );
        assert_eq!(
            settings_for(json!({})),
            SaveSync::Enabled {
                include_text: false
            }
        );
        assert_eq!(
            settings_for(json!({"includeText": false})),
            SaveSync::Enabled {
                include_text: false
            }
        );
        assert_eq!(
            settings_for(json!({"includeText": true})),
            SaveSync::Enabled { include_text: true }
        );

        let without_save: ServerCapabilities =
            serde_json::from_value(json!({"textDocumentSync": {"change": 2}})).unwrap();
        assert_eq!(
            SyncSettings::from_capabilities(&without_save).save,
            SaveSync::Disabled
        );
    }

    #[test]
    fn save_false_does_not_produce_did_save() {
        let capabilities: ServerCapabilities = serde_json::from_value(json!({
            "textDocumentSync": {"openClose": true, "change": 2, "save": false}
        }))
        .unwrap();
        let mut sync = DocumentSync::from_capabilities(&capabilities);
        sync.open("file:///a.rs", "rust", "a").unwrap();

        assert_eq!(sync.save("file:///a.rs", "ab").unwrap(), None);
    }

    #[test]
    fn save_true_produces_did_save_without_text() {
        let capabilities: ServerCapabilities = serde_json::from_value(json!({
            "textDocumentSync": {"openClose": true, "change": 2, "save": true}
        }))
        .unwrap();
        let mut sync = DocumentSync::from_capabilities(&capabilities);
        sync.open("file:///a.rs", "rust", "a").unwrap();

        let SyncNotification::DidSave(params) = sync.save("file:///a.rs", "ab").unwrap().unwrap()
        else {
            panic!("esperado didSave");
        };
        assert_eq!(params.text, None);
    }

    #[test]
    fn open_close_false_from_capabilities_suppresses_open_and_close() {
        let capabilities: ServerCapabilities = serde_json::from_value(json!({
            "textDocumentSync": {"openClose": false, "change": 2}
        }))
        .unwrap();
        let mut sync = DocumentSync::from_capabilities(&capabilities);

        assert_eq!(sync.open("file:///a.rs", "rust", "a").unwrap(), None);
        assert!(sync.is_open("file:///a.rs"));
        assert_eq!(sync.close("file:///a.rs").unwrap(), None);
        assert!(!sync.is_open("file:///a.rs"));
    }

    #[test]
    fn missing_sync_capability_disables_everything() {
        let capabilities = ServerCapabilities::default();
        let mut sync = DocumentSync::from_capabilities(&capabilities);

        assert!(!sync.settings().tracks_documents());
        assert_eq!(sync.open("file:///a.rs", "rust", "a").unwrap(), None);
        assert!(sync.is_empty());
    }

    #[test]
    fn open_full_mode_sends_initial_content() {
        let mut sync = DocumentSync::new(settings(
            TextDocumentSyncKind::Full,
            true,
            SaveSync::Disabled,
        ));

        let notification = sync
            .open("file:///a.rs", "rust", "fn main() {}")
            .unwrap()
            .unwrap();
        let SyncNotification::DidOpen(params) = notification else {
            panic!("esperado didOpen");
        };

        assert_eq!(params.text_document.version, 1);
        assert_eq!(params.text_document.language_id, "rust");
        assert_eq!(params.text_document.text, "fn main() {}");
        assert_eq!(sync.document("file:///a.rs").unwrap().version, 1);
    }

    #[test]
    fn full_mode_sends_the_whole_updated_content() {
        let mut sync = DocumentSync::new(settings(
            TextDocumentSyncKind::Full,
            true,
            SaveSync::Disabled,
        ));
        sync.open("file:///a.rs", "rust", "abc").unwrap();

        let notification = sync
            .change(
                "file:///a.rs",
                &[TextChange::new(0, 1, "x"), TextChange::new(2, 3, "z")],
                "xbc",
            )
            .unwrap()
            .unwrap();
        let SyncNotification::DidChange(params) = notification else {
            panic!("esperado didChange");
        };

        assert_eq!(params.text_document.version, 2);
        assert_eq!(params.content_changes.len(), 1);
        assert_eq!(params.content_changes[0].range, None);
        assert_eq!(params.content_changes[0].text, "xbc");
    }

    #[test]
    fn incremental_mode_sends_positions_in_order() {
        let mut sync = incremental();
        sync.open("file:///a.rs", "rust", "abc\ndef").unwrap();

        let notification = sync
            .change(
                "file:///a.rs",
                &[TextChange::new(1, 2, "X"), TextChange::new(5, 6, "Y")],
                "aXc\ndYf",
            )
            .unwrap()
            .unwrap();
        let SyncNotification::DidChange(params) = notification else {
            panic!("esperado didChange");
        };

        assert_eq!(params.text_document.version, 2);
        assert_eq!(params.content_changes.len(), 2);
        assert_eq!(
            params.content_changes[0].range.unwrap().start,
            crate::protocol::Position {
                line: 0,
                character: 1
            }
        );
        assert_eq!(params.content_changes[0].text, "X");
        assert_eq!(
            params.content_changes[1].range.unwrap().start,
            crate::protocol::Position {
                line: 1,
                character: 1
            }
        );
        assert_eq!(params.content_changes[1].text, "Y");
    }

    #[test]
    fn emoji_and_accents_use_utf16_positions() {
        let mut sync = incremental();
        sync.open("file:///a.rs", "rust", "a😀b\ncafé").unwrap();

        let notification = sync
            .change(
                "file:///a.rs",
                &[TextChange::new(1, 2, "Z"), TextChange::new(7, 8, "e")],
                "aZb\ncafe",
            )
            .unwrap()
            .unwrap();
        let SyncNotification::DidChange(params) = notification else {
            panic!("esperado didChange");
        };

        assert_eq!(params.content_changes.len(), 2);
        assert_eq!(
            params.content_changes[0].range.unwrap(),
            crate::protocol::Range {
                start: crate::protocol::Position {
                    line: 0,
                    character: 1
                },
                end: crate::protocol::Position {
                    line: 0,
                    character: 3
                },
            }
        );
        assert_eq!(
            params.content_changes[1].range.unwrap(),
            crate::protocol::Range {
                start: crate::protocol::Position {
                    line: 1,
                    character: 3
                },
                end: crate::protocol::Position {
                    line: 1,
                    character: 4
                },
            }
        );
    }

    #[test]
    fn versions_increase_once_per_notification() {
        let mut sync = incremental();
        sync.open("file:///a.rs", "rust", "a").unwrap();

        let first = sync
            .change("file:///a.rs", &[TextChange::new(1, 1, "b")], "ab")
            .unwrap()
            .unwrap();
        let second = sync
            .change("file:///a.rs", &[TextChange::new(2, 2, "c")], "abc")
            .unwrap()
            .unwrap();

        let SyncNotification::DidChange(first) = first else {
            panic!("esperado didChange");
        };
        let SyncNotification::DidChange(second) = second else {
            panic!("esperado didChange");
        };

        assert_eq!(first.text_document.version, 2);
        assert_eq!(second.text_document.version, 3);
        assert_eq!(sync.document("file:///a.rs").unwrap().version, 3);
    }

    #[test]
    fn duplicate_or_empty_changes_are_not_sent() {
        let mut sync = incremental();
        sync.open("file:///a.rs", "rust", "abc").unwrap();

        assert_eq!(sync.change("file:///a.rs", &[], "abc").unwrap(), None);
        assert_eq!(sync.resync("file:///a.rs", "abc").unwrap(), None);
    }

    #[test]
    fn mismatched_shadow_degrades_to_full_change() {
        let mut sync = incremental();
        sync.open("file:///a.rs", "rust", "abc").unwrap();

        let notification = sync
            .change(
                "file:///a.rs",
                &[TextChange::new(0, 1, "x")],
                "conteúdo diferente",
            )
            .unwrap()
            .unwrap();
        let SyncNotification::DidChange(params) = notification else {
            panic!("esperado didChange");
        };

        assert_eq!(params.content_changes.len(), 1);
        assert_eq!(params.content_changes[0].range, None);
        assert_eq!(params.content_changes[0].text, "conteúdo diferente");
        assert_eq!(
            sync.document("file:///a.rs").unwrap().text,
            "conteúdo diferente"
        );
    }

    #[test]
    fn resync_incremental_uses_the_minimal_changed_range() {
        let mut sync = incremental();
        sync.open("file:///a.rs", "rust", "prefixo antigo sufixo")
            .unwrap();

        let notification = sync
            .resync("file:///a.rs", "prefixo novo sufixo")
            .unwrap()
            .unwrap();
        let SyncNotification::DidChange(params) = notification else {
            panic!("esperado didChange");
        };

        assert_eq!(params.content_changes.len(), 1);
        assert_eq!(
            params.content_changes[0].range.unwrap().start,
            crate::protocol::Position {
                line: 0,
                character: 8
            }
        );
        assert_eq!(params.content_changes[0].text, "nov");
    }

    fn apply_content_changes(old: &str, params: &DidChangeTextDocumentParams) -> String {
        let mut text = old.to_owned();

        for change in &params.content_changes {
            match change.range {
                None => text = change.text.clone(),
                Some(range) => {
                    let start = utf16::offset_from_position(&text, range.start)
                        .expect("início do range deveria ser uma posição LSP válida");
                    let end = utf16::offset_from_position(&text, range.end)
                        .expect("fim do range deveria ser uma posição LSP válida");
                    replace_chars(&mut text, start, end, &change.text);
                }
            }
        }

        text
    }

    fn did_change_params(notification: SyncNotification) -> DidChangeTextDocumentParams {
        match notification {
            SyncNotification::DidChange(params) => params,
            other => panic!("esperado didChange, recebido {other:?}"),
        }
    }

    #[test]
    fn resync_crlf_to_lf_uses_full_fallback() {
        let mut sync = incremental();
        sync.open("file:///a.rs", "rust", "a\r\nb").unwrap();

        let notification = sync.resync("file:///a.rs", "a\nb").unwrap().unwrap();
        let params = did_change_params(notification);

        assert_eq!(params.content_changes.len(), 1);
        assert_eq!(params.content_changes[0].range, None);
        assert_eq!(apply_content_changes("a\r\nb", &params), "a\nb");
        assert_eq!(sync.document("file:///a.rs").unwrap().text, "a\nb");
    }

    #[test]
    fn resync_lf_to_crlf_uses_safe_incremental_range() {
        let mut sync = incremental();
        sync.open("file:///a.rs", "rust", "a\nb").unwrap();

        let notification = sync.resync("file:///a.rs", "a\r\nb").unwrap().unwrap();
        let params = did_change_params(notification);

        assert_eq!(params.content_changes.len(), 1);
        assert!(params.content_changes[0].range.is_some());
        assert_eq!(apply_content_changes("a\nb", &params), "a\r\nb");
        assert_eq!(sync.document("file:///a.rs").unwrap().text, "a\r\nb");
    }

    #[test]
    fn resync_crlf_to_cr_uses_full_fallback() {
        let mut sync = incremental();
        sync.open("file:///a.rs", "rust", "a\r\nb").unwrap();

        let notification = sync.resync("file:///a.rs", "a\rb").unwrap().unwrap();
        let params = did_change_params(notification);

        assert_eq!(params.content_changes.len(), 1);
        assert_eq!(params.content_changes[0].range, None);
        assert_eq!(apply_content_changes("a\r\nb", &params), "a\rb");
        assert_eq!(sync.document("file:///a.rs").unwrap().text, "a\rb");
    }

    #[test]
    fn resync_cr_to_crlf_uses_safe_incremental_range() {
        let mut sync = incremental();
        sync.open("file:///a.rs", "rust", "a\rb").unwrap();

        let notification = sync.resync("file:///a.rs", "a\r\nb").unwrap().unwrap();
        let params = did_change_params(notification);

        assert_eq!(params.content_changes.len(), 1);
        assert!(params.content_changes[0].range.is_some());
        assert_eq!(apply_content_changes("a\rb", &params), "a\r\nb");
        assert_eq!(sync.document("file:///a.rs").unwrap().text, "a\r\nb");
    }

    #[test]
    fn resync_near_document_boundaries_never_splits_crlf() {
        let mut sync = incremental();

        // Início: remover o `\r` de um CRLF inicial exige posição inválida.
        sync.open("file:///start.rs", "rust", "\r\na").unwrap();
        let params = did_change_params(sync.resync("file:///start.rs", "\na").unwrap().unwrap());
        assert_eq!(params.content_changes[0].range, None);
        assert_eq!(apply_content_changes("\r\na", &params), "\na");

        // Fim: remover o `\r` de um CRLF final também exige fallback integral.
        sync.open("file:///end.rs", "rust", "a\r\n").unwrap();
        let params = did_change_params(sync.resync("file:///end.rs", "a\n").unwrap().unwrap());
        assert_eq!(params.content_changes[0].range, None);
        assert_eq!(apply_content_changes("a\r\n", &params), "a\n");

        // Início: alterar o primeiro caractere preserva o CRLF seguinte.
        sync.open("file:///start-change.rs", "rust", "a\r\nb")
            .unwrap();
        let params = did_change_params(
            sync.resync("file:///start-change.rs", "x\r\nb")
                .unwrap()
                .unwrap(),
        );
        assert!(params.content_changes[0].range.is_some());
        assert_eq!(apply_content_changes("a\r\nb", &params), "x\r\nb");

        // Fim: acrescentar um CRLF no fim é representável no último offset.
        sync.open("file:///end-append.rs", "rust", "a\r\nb")
            .unwrap();
        let params = did_change_params(
            sync.resync("file:///end-append.rs", "a\r\nb\r\n")
                .unwrap()
                .unwrap(),
        );
        assert!(params.content_changes[0].range.is_some());
        assert_eq!(apply_content_changes("a\r\nb", &params), "a\r\nb\r\n");
    }

    #[test]
    fn transaction_that_would_split_crlf_degrades_to_full_change() {
        let mut sync = incremental();
        sync.open("file:///a.rs", "rust", "a\r\nb").unwrap();

        let notification = sync
            .change("file:///a.rs", &[TextChange::new(2, 3, "")], "a\rb")
            .unwrap()
            .unwrap();
        let params = did_change_params(notification);

        assert_eq!(params.content_changes.len(), 1);
        assert_eq!(params.content_changes[0].range, None);
        assert_eq!(params.content_changes[0].text, "a\rb");
        assert_eq!(apply_content_changes("a\r\nb", &params), "a\rb");
        assert_eq!(sync.document("file:///a.rs").unwrap().text, "a\rb");
    }

    #[test]
    fn clear_forgets_documents_without_notifications() {
        let mut sync = incremental();
        sync.open("file:///a.rs", "rust", "a").unwrap();

        sync.clear();

        assert!(sync.is_empty());
        assert!(!sync.is_open("file:///a.rs"));
        assert_eq!(sync.close("file:///a.rs").unwrap(), None);
    }

    #[test]
    fn save_uses_include_text_capability() {
        let mut with_text = incremental();
        with_text.open("file:///a.rs", "rust", "a").unwrap();
        let SyncNotification::DidSave(params) =
            with_text.save("file:///a.rs", "abc").unwrap().unwrap()
        else {
            panic!("esperado didSave");
        };
        assert_eq!(params.text.as_deref(), Some("abc"));

        let mut without_text = DocumentSync::new(settings(
            TextDocumentSyncKind::Incremental,
            true,
            SaveSync::Enabled {
                include_text: false,
            },
        ));
        without_text.open("file:///a.rs", "rust", "a").unwrap();
        let SyncNotification::DidSave(params) =
            without_text.save("file:///a.rs", "abc").unwrap().unwrap()
        else {
            panic!("esperado didSave");
        };
        assert_eq!(params.text, None);
    }

    #[test]
    fn save_is_ignored_when_capability_is_disabled() {
        let mut sync = DocumentSync::new(settings(
            TextDocumentSyncKind::Incremental,
            true,
            SaveSync::Disabled,
        ));
        sync.open("file:///a.rs", "rust", "a").unwrap();

        assert_eq!(sync.save("file:///a.rs", "abc").unwrap(), None);
    }

    #[test]
    fn open_close_disabled_tracks_changes_without_open_or_close() {
        let mut sync = DocumentSync::new(settings(
            TextDocumentSyncKind::Incremental,
            false,
            SaveSync::Disabled,
        ));

        assert_eq!(sync.open("file:///a.rs", "rust", "a").unwrap(), None);
        assert!(sync.is_open("file:///a.rs"));

        let notification = sync
            .change("file:///a.rs", &[TextChange::new(1, 1, "b")], "ab")
            .unwrap()
            .unwrap();
        assert_eq!(notification.method(), "textDocument/didChange");
        assert_eq!(sync.close("file:///a.rs").unwrap(), None);
        assert!(!sync.is_open("file:///a.rs"));
    }

    #[test]
    fn closing_one_document_keeps_the_others() {
        let mut sync = incremental();
        sync.open("file:///a/main.rs", "rust", "a").unwrap();
        sync.open("file:///b/main.rs", "rust", "b").unwrap();

        let notification = sync.close("file:///a/main.rs").unwrap().unwrap();
        let SyncNotification::DidClose(params) = notification else {
            panic!("esperado didClose");
        };

        assert_eq!(params.text_document.uri, "file:///a/main.rs");
        assert!(!sync.is_open("file:///a/main.rs"));
        assert!(sync.is_open("file:///b/main.rs"));
        assert_eq!(sync.len(), 1);
    }

    #[test]
    fn same_name_in_different_directories_is_independent() {
        let mut sync = incremental();
        sync.open("file:///a/main.rs", "rust", "a").unwrap();
        sync.open("file:///b/main.rs", "rust", "b").unwrap();

        sync.change("file:///a/main.rs", &[TextChange::new(1, 1, "!")], "a!")
            .unwrap();

        assert_eq!(sync.document("file:///a/main.rs").unwrap().text, "a!");
        assert_eq!(sync.document("file:///b/main.rs").unwrap().text, "b");
    }

    #[test]
    fn operations_require_an_open_document() {
        let mut sync = incremental();

        assert_eq!(
            sync.change("file:///a.rs", &[TextChange::new(0, 0, "a")], "a"),
            Err(SyncError::NotOpen {
                uri: "file:///a.rs".to_owned()
            })
        );
        assert_eq!(
            sync.save("file:///a.rs", "a"),
            Err(SyncError::NotOpen {
                uri: "file:///a.rs".to_owned()
            })
        );
        assert_eq!(sync.close("file:///a.rs").unwrap(), None);
    }

    #[test]
    fn duplicate_open_is_rejected() {
        let mut sync = incremental();
        sync.open("file:///a.rs", "rust", "a").unwrap();

        assert_eq!(
            sync.open("file:///a.rs", "rust", "b"),
            Err(SyncError::AlreadyOpen {
                uri: "file:///a.rs".to_owned()
            })
        );
    }

    #[test]
    fn invalid_change_offsets_are_rejected_without_mutating_state() {
        let mut sync = incremental();
        sync.open("file:///a.rs", "rust", "abc").unwrap();

        assert_eq!(
            sync.change("file:///a.rs", &[TextChange::new(2, 1, "x")], "abc"),
            Err(SyncError::InvalidChange {
                start: 2,
                end: 1,
                len: 3
            })
        );
        assert_eq!(sync.document("file:///a.rs").unwrap().text, "abc");
        assert_eq!(sync.document("file:///a.rs").unwrap().version, 1);
    }

    #[test]
    fn version_overflow_is_reported() {
        let mut sync = incremental();
        sync.open("file:///a.rs", "rust", "a").unwrap();
        sync.documents.get_mut("file:///a.rs").unwrap().version = i64::MAX;

        assert_eq!(
            sync.change("file:///a.rs", &[TextChange::new(1, 1, "b")], "ab"),
            Err(SyncError::VersionOverflow {
                uri: "file:///a.rs".to_owned()
            })
        );
    }

    #[test]
    fn notifications_serialize_with_expected_methods() {
        let mut sync = incremental();
        let open = sync.open("file:///a.rs", "rust", "a").unwrap().unwrap();
        assert_eq!(open.method(), "textDocument/didOpen");
        assert!(open.to_params().unwrap().get("textDocument").is_some());

        let change = sync
            .change("file:///a.rs", &[TextChange::new(1, 1, "b")], "ab")
            .unwrap()
            .unwrap();
        assert_eq!(change.method(), "textDocument/didChange");
        assert!(change.to_params().unwrap().get("contentChanges").is_some());

        let save = sync.save("file:///a.rs", "ab").unwrap().unwrap();
        assert_eq!(save.method(), "textDocument/didSave");

        let close = sync.close("file:///a.rs").unwrap().unwrap();
        assert_eq!(close.method(), "textDocument/didClose");
    }
}
