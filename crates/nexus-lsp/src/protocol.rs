//! Tipos fundamentais do protocolo LSP.
//!
//! Tipos do protocolo LSP, incluindo inicialização, sincronização, diagnósticos
//! e modelos neutros de navegação semântica.

use std::collections::BTreeMap;

use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as DeError};
use serde_json::Value;

/// Posição baseada em linha e em unidades UTF-16.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Position {
    pub line: u32,
    pub character: u32,
}

/// Intervalo semiaberto no texto.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Range {
    pub start: Position,
    pub end: Position,
}

/// Identificação de um documento por URI.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TextDocumentIdentifier {
    pub uri: String,
}

/// Requisição de navegação baseada em um documento e posição UTF-16.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextDocumentPositionParams {
    pub text_document: TextDocumentIdentifier,
    pub position: Position,
}

/// Opções específicas da requisição de referências.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReferenceContext {
    pub include_declaration: bool,
}

/// Parâmetros de `textDocument/references`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReferenceParams {
    pub text_document: TextDocumentIdentifier,
    pub position: Position,
    pub context: ReferenceContext,
}

/// Métodos LSP de navegação por posição.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavigationMethod {
    Definition,
    Declaration,
    TypeDefinition,
    Implementation,
    References { include_declaration: bool },
}

impl NavigationMethod {
    /// Nome do método JSON-RPC definido pelo LSP.
    pub const fn lsp_method(self) -> &'static str {
        match self {
            Self::Definition => "textDocument/definition",
            Self::Declaration => "textDocument/declaration",
            Self::TypeDefinition => "textDocument/typeDefinition",
            Self::Implementation => "textDocument/implementation",
            Self::References { .. } => "textDocument/references",
        }
    }

    /// Prepara os parâmetros corretos para o método selecionado.
    pub fn params(
        self,
        text_document: TextDocumentIdentifier,
        position: Position,
    ) -> NavigationParams {
        match self {
            Self::References {
                include_declaration,
            } => NavigationParams::References(ReferenceParams {
                text_document,
                position,
                context: ReferenceContext {
                    include_declaration,
                },
            }),
            Self::Definition | Self::Declaration | Self::TypeDefinition | Self::Implementation => {
                NavigationParams::TextDocumentPosition(TextDocumentPositionParams {
                    text_document,
                    position,
                })
            }
        }
    }
}

/// Parâmetros serializáveis para qualquer método de navegação.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum NavigationParams {
    TextDocumentPosition(TextDocumentPositionParams),
    References(ReferenceParams),
}

/// Representação dos campos comuns das opções de provider.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NavigationProviderOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work_done_progress: Option<bool>,
    /// Opções futuras ou específicas do servidor permanecem preservadas.
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

/// Provider pode ser declarado como boolean ou como objeto de opções.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum NavigationProviderCapability {
    Enabled(bool),
    Options(NavigationProviderOptions),
}

impl NavigationProviderCapability {
    /// Um objeto de opções declara suporte; boolean `false` não declara.
    pub const fn is_supported(&self) -> bool {
        match self {
            Self::Enabled(enabled) => *enabled,
            Self::Options(_) => true,
        }
    }
}

/// Identificação versionada usada em alterações incrementais.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct VersionedTextDocumentIdentifier {
    pub uri: String,
    pub version: i64,
}

/// Conteúdo inicial de um documento aberto.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextDocumentItem {
    pub uri: String,
    pub language_id: String,
    pub version: i64,
    pub text: String,
}

/// Informações do cliente enviadas durante a inicialização.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ClientInfo {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

/// Informações do servidor devolvidas na inicialização.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ServerInfo {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

/// Pasta de trabalho declarada pelo cliente.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceFolder {
    pub uri: String,
    pub name: String,
}

/// Capacidades declaradas pelo cliente.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientCapabilities {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_document: Option<TextDocumentClientCapabilities>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub general: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub experimental: Option<Value>,
    /// Capacidades desconhecidas preservadas para compatibilidade futura.
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

impl ClientCapabilities {
    /// Capacidades padrão anunciadas pelo cliente Nexus no handshake.
    pub fn nexus_defaults() -> Self {
        let link_support = || {
            Some(TextDocumentLinkSupportCapabilities {
                link_support: Some(true),
                ..Default::default()
            })
        };

        Self {
            text_document: Some(TextDocumentClientCapabilities {
                synchronization: Some(TextDocumentSyncClientCapabilities {
                    did_save: Some(true),
                    ..Default::default()
                }),
                definition: link_support(),
                declaration: link_support(),
                type_definition: link_support(),
                implementation: link_support(),
                extra: BTreeMap::from([(
                    "completion".to_owned(),
                    serde_json::json!({
                        "dynamicRegistration": false,
                        "completionItem": {
                            "snippetSupport": true,
                            "documentationFormat": ["markdown", "plaintext"],
                            "deprecatedSupport": true,
                            "preselectSupport": true,
                            "insertReplaceSupport": false,
                            "labelDetailsSupport": true
                        },
                        "contextSupport": true
                    }),
                )]),
                ..Default::default()
            }),
            ..Default::default()
        }
    }
}

/// Capacidades de sincronização declaradas pelo cliente.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextDocumentClientCapabilities {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub synchronization: Option<TextDocumentSyncClientCapabilities>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub definition: Option<TextDocumentLinkSupportCapabilities>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub declaration: Option<TextDocumentLinkSupportCapabilities>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub type_definition: Option<TextDocumentLinkSupportCapabilities>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub implementation: Option<TextDocumentLinkSupportCapabilities>,
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

/// Capacidades de cliente compartilhadas pelos métodos que aceitam
/// `LocationLink`. A capability de References não possui `linkSupport`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextDocumentLinkSupportCapabilities {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dynamic_registration: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link_support: Option<bool>,
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

/// Subconjunto de sincronização de `TextDocumentClientCapabilities`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextDocumentSyncClientCapabilities {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dynamic_registration: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub will_save: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub will_save_wait_until: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub did_save: Option<bool>,
}

/// Parâmetros da requisição `initialize`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeParams {
    /// `null` é permitido quando o cliente não expõe um processo pai.
    #[serde(default)]
    pub process_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_info: Option<ClientInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locale: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_path: Option<String>,
    /// `null` é permitido quando não há raiz única.
    #[serde(default)]
    pub root_uri: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initialization_options: Option<Value>,
    pub capabilities: ClientCapabilities,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_folders: Option<Vec<WorkspaceFolder>>,
}

/// Resultado da requisição `initialize`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResult {
    pub capabilities: ServerCapabilities,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_info: Option<ServerInfo>,
}

/// Capacidades declaradas pelo servidor.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerCapabilities {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_document_sync: Option<TextDocumentSyncCapability>,
    /// Codificação de posições escolhida pelo servidor.
    ///
    /// O Nexus implementa apenas `utf-16`, o padrão do LSP quando o cliente não
    /// anuncia outras codificações. Um servidor que responda com `utf-8` ou
    /// `utf-32` é rejeitado no handshake para não corromper posições.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position_encoding: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub definition_provider: Option<NavigationProviderCapability>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub declaration_provider: Option<NavigationProviderCapability>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub type_definition_provider: Option<NavigationProviderCapability>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub implementation_provider: Option<NavigationProviderCapability>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub references_provider: Option<NavigationProviderCapability>,
    /// Capacidades desconhecidas preservadas para etapas futuras.
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

impl ServerCapabilities {
    /// Informa se o servidor declarou suporte estático ao método.
    ///
    /// O suporte dinâmico não é interpretado nesta etapa.
    pub fn supports_navigation(&self, method: NavigationMethod) -> bool {
        let capability = match method {
            NavigationMethod::Definition => self.definition_provider.as_ref(),
            NavigationMethod::Declaration => self.declaration_provider.as_ref(),
            NavigationMethod::TypeDefinition => self.type_definition_provider.as_ref(),
            NavigationMethod::Implementation => self.implementation_provider.as_ref(),
            NavigationMethod::References { .. } => self.references_provider.as_ref(),
        };
        capability.is_some_and(NavigationProviderCapability::is_supported)
    }
}

/// `textDocumentSync` pode ser um número ou um objeto de opções.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum TextDocumentSyncCapability {
    Kind(TextDocumentSyncKind),
    Options(TextDocumentSyncOptions),
}

/// Estratégia de sincronização anunciada pelo servidor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum TextDocumentSyncKind {
    None = 0,
    Full = 1,
    Incremental = 2,
}

impl Serialize for TextDocumentSyncKind {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u8(*self as u8)
    }
}

impl<'de> Deserialize<'de> for TextDocumentSyncKind {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = u8::deserialize(deserializer)?;
        Self::try_from(value).map_err(serde::de::Error::custom)
    }
}

impl TryFrom<u8> for TextDocumentSyncKind {
    type Error = String;

    fn try_from(value: u8) -> Result<Self, String> {
        match value {
            0 => Ok(Self::None),
            1 => Ok(Self::Full),
            2 => Ok(Self::Incremental),
            other => Err(format!("estratégia de sincronização desconhecida: {other}")),
        }
    }
}

impl From<TextDocumentSyncKind> for u8 {
    fn from(value: TextDocumentSyncKind) -> Self {
        value as Self
    }
}

/// Opções detalhadas de sincronização.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextDocumentSyncOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open_close: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change: Option<TextDocumentSyncKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub will_save: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub will_save_wait_until: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub save: Option<TextDocumentSaveCapability>,
}

/// `save` pode ser um booleano ou um objeto de opções.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum TextDocumentSaveCapability {
    IncludeText(bool),
    Options(SaveOptions),
}

/// Opções de salvamento.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SaveOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include_text: Option<bool>,
}

/// Parâmetros de `textDocument/didOpen`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DidOpenTextDocumentParams {
    pub text_document: TextDocumentItem,
}

/// Alteração textual enviada por `textDocument/didChange`.
///
/// `range` ausente indica sincronização completa; presente indica alteração
/// incremental.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextDocumentContentChangeEvent {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range: Option<Range>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range_length: Option<u32>,
    pub text: String,
}

/// Parâmetros de `textDocument/didChange`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DidChangeTextDocumentParams {
    pub text_document: VersionedTextDocumentIdentifier,
    pub content_changes: Vec<TextDocumentContentChangeEvent>,
}

/// Parâmetros de `textDocument/didSave`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DidSaveTextDocumentParams {
    pub text_document: TextDocumentIdentifier,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

/// Parâmetros de `textDocument/didClose`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DidCloseTextDocumentParams {
    pub text_document: TextDocumentIdentifier,
}

/// Severidade de um diagnóstico.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum DiagnosticSeverity {
    Error = 1,
    Warning = 2,
    Information = 3,
    Hint = 4,
}

impl Serialize for DiagnosticSeverity {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u8(*self as u8)
    }
}

impl<'de> Deserialize<'de> for DiagnosticSeverity {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = u8::deserialize(deserializer)?;
        Self::try_from(value).map_err(serde::de::Error::custom)
    }
}

impl TryFrom<u8> for DiagnosticSeverity {
    type Error = String;

    fn try_from(value: u8) -> Result<Self, String> {
        match value {
            1 => Ok(Self::Error),
            2 => Ok(Self::Warning),
            3 => Ok(Self::Information),
            4 => Ok(Self::Hint),
            other => Err(format!("severidade de diagnóstico inválida: {other}")),
        }
    }
}

impl From<DiagnosticSeverity> for u8 {
    fn from(value: DiagnosticSeverity) -> Self {
        value as Self
    }
}

/// Diagnóstico publicado pelo servidor.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Diagnostic {
    pub range: Range,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub severity: Option<DiagnosticSeverity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code_description: Option<CodeDescription>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<DiagnosticTag>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub related_information: Vec<DiagnosticRelatedInformation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    /// Campos adicionais preservados para compatibilidade com extensões LSP.
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodeDescription {
    pub href: String,
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum DiagnosticTag {
    Unnecessary = 1,
    Deprecated = 2,
}

impl Serialize for DiagnosticTag {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u8(*self as u8)
    }
}

impl<'de> Deserialize<'de> for DiagnosticTag {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match u8::deserialize(deserializer)? {
            1 => Ok(Self::Unnecessary),
            2 => Ok(Self::Deprecated),
            value => Err(serde::de::Error::custom(format!(
                "tag de diagnóstico inválida: {value}"
            ))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticRelatedInformation {
    pub location: Location,
    pub message: String,
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Location {
    pub uri: String,
    pub range: Range,
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

/// Destino com faixas de origem e destino, usado pelas respostas `LocationLink`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocationLink {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_selection_range: Option<Range>,
    pub target_uri: String,
    pub target_range: Range,
    pub target_selection_range: Range,
    /// Campos futuros permanecem disponíveis para consumidores do protocolo.
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

/// Destino de navegação normalizado, sem dependência de abertura de arquivos.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NavigationTarget {
    pub uri: String,
    pub target_range: Range,
    pub selection_range: Range,
    pub origin_selection_range: Option<Range>,
}

impl From<Location> for NavigationTarget {
    fn from(location: Location) -> Self {
        Self {
            uri: location.uri,
            target_range: location.range,
            selection_range: location.range,
            origin_selection_range: None,
        }
    }
}

impl TryFrom<LocationLink> for NavigationTarget {
    type Error = serde_json::Error;

    fn try_from(link: LocationLink) -> Result<Self, Self::Error> {
        if !range_contains(link.target_range, link.target_selection_range) {
            return Err(serde_json::Error::custom(
                "targetSelectionRange deve estar contido em targetRange",
            ));
        }
        Ok(Self {
            uri: link.target_uri,
            target_range: link.target_range,
            selection_range: link.target_selection_range,
            origin_selection_range: link.origin_selection_range,
        })
    }
}

fn range_contains(outer: Range, inner: Range) -> bool {
    outer.start <= outer.end
        && inner.start <= inner.end
        && outer.start <= inner.start
        && inner.end <= outer.end
}

/// Resultado interpretado de uma resposta de navegação.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NavigationResult {
    Destinations(Vec<NavigationTarget>),
    Empty,
    /// Deve ser produzido pela camada que consulta `ServerCapabilities`.
    Unsupported,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum DefinitionLikeResponse {
    Location(Location),
    Locations(Vec<Location>),
    LocationLinks(Vec<LocationLink>),
}

impl NavigationResult {
    /// Interpreta respostas de definition, declaration, type definition e
    /// implementation. `null` e listas vazias são resultados sem destinos.
    pub fn from_goto_value(value: Value) -> Result<Self, serde_json::Error> {
        let Some(response) = serde_json::from_value::<Option<DefinitionLikeResponse>>(value)?
        else {
            return Ok(Self::Empty);
        };
        let destinations = match response {
            DefinitionLikeResponse::Location(location) => vec![location.into()],
            DefinitionLikeResponse::Locations(locations) => {
                locations.into_iter().map(Into::into).collect()
            }
            DefinitionLikeResponse::LocationLinks(links) => links
                .into_iter()
                .map(NavigationTarget::try_from)
                .collect::<Result<Vec<_>, _>>()?,
        };
        Ok(if destinations.is_empty() {
            Self::Empty
        } else {
            Self::Destinations(destinations)
        })
    }

    /// Interpreta `textDocument/references`, cuja resposta válida é uma lista
    /// de `Location` ou `null`.
    pub fn from_references_value(value: Value) -> Result<Self, serde_json::Error> {
        let Some(locations) = serde_json::from_value::<Option<Vec<Location>>>(value)? else {
            return Ok(Self::Empty);
        };
        let destinations = locations.into_iter().map(Into::into).collect::<Vec<_>>();
        Ok(if destinations.is_empty() {
            Self::Empty
        } else {
            Self::Destinations(destinations)
        })
    }

    /// Normaliza o resultado conforme o método, rejeitando formatos que a
    /// especificação não permite para aquela resposta.
    pub fn from_value(method: NavigationMethod, value: Value) -> Result<Self, serde_json::Error> {
        match method {
            NavigationMethod::References { .. } => Self::from_references_value(value),
            NavigationMethod::Definition
            | NavigationMethod::Declaration
            | NavigationMethod::TypeDefinition
            | NavigationMethod::Implementation => Self::from_goto_value(value),
        }
    }
}

/// Parâmetros de `textDocument/publishDiagnostics`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PublishDiagnosticsParams {
    pub uri: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<i64>,
    pub diagnostics: Vec<Diagnostic>,
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn initialize_round_trip_preserves_utf8_and_capabilities() {
        let params = InitializeParams {
            process_id: Some(42),
            client_info: Some(ClientInfo {
                name: "Nexus".to_owned(),
                version: Some("1.5".to_owned()),
            }),
            root_uri: Some("file:///projetos/ação".to_owned()),
            capabilities: ClientCapabilities {
                text_document: Some(TextDocumentClientCapabilities {
                    synchronization: Some(TextDocumentSyncClientCapabilities {
                        did_save: Some(true),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            },
            ..Default::default()
        };

        let encoded = serde_json::to_string(&params).unwrap();
        let decoded: InitializeParams = serde_json::from_str(&encoded).unwrap();

        assert_eq!(decoded, params);
        assert!(encoded.contains("ação"));
        assert!(encoded.contains("\"didSave\":true"));
        assert!(encoded.contains("\"processId\":42"));
    }

    #[test]
    fn nexus_initialize_capabilities_advertise_location_link_support() {
        let params = InitializeParams {
            capabilities: ClientCapabilities::nexus_defaults(),
            ..Default::default()
        };
        let value = serde_json::to_value(&params).unwrap();
        let text_document = &value["capabilities"]["textDocument"];

        for method in [
            "definition",
            "declaration",
            "typeDefinition",
            "implementation",
        ] {
            assert_eq!(text_document[method]["linkSupport"], true, "{method}");
        }
        assert!(text_document.get("references").is_none());
        assert_eq!(text_document["synchronization"]["didSave"], true);

        let decoded: InitializeParams = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(decoded.capabilities, params.capabilities);

        let mut with_unknown = value;
        with_unknown["capabilities"]["textDocument"]["futureCapability"] = json!({"enabled":true});
        let decoded: InitializeParams = serde_json::from_value(with_unknown.clone()).unwrap();
        let text_document = decoded.capabilities.text_document.unwrap();
        assert_eq!(
            text_document.extra["futureCapability"],
            json!({"enabled":true})
        );
        assert_eq!(
            serde_json::to_value(text_document).unwrap()["futureCapability"],
            json!({"enabled":true})
        );
    }

    #[test]
    fn initialize_result_round_trips_server_info() {
        let result = InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::Incremental,
                )),
                position_encoding: Some("utf-16".to_owned()),
                ..Default::default()
            },
            server_info: Some(ServerInfo {
                name: "rust-analyzer".to_owned(),
                version: Some("1.0".to_owned()),
            }),
        };

        let encoded = serde_json::to_string(&result).unwrap();
        let decoded: InitializeResult = serde_json::from_str(&encoded).unwrap();

        assert_eq!(decoded, result);
        assert!(encoded.contains("\"textDocumentSync\":2"));
    }

    #[test]
    fn server_capabilities_accept_numeric_and_options_sync() {
        let numeric: ServerCapabilities =
            serde_json::from_value(json!({"textDocumentSync": 1})).unwrap();
        assert_eq!(
            numeric.text_document_sync,
            Some(TextDocumentSyncCapability::Kind(TextDocumentSyncKind::Full))
        );

        let options: ServerCapabilities = serde_json::from_value(json!({
            "textDocumentSync": {
                "openClose": true,
                "change": 2,
                "save": {"includeText": true}
            },
            "futureCapability": {"enabled": true}
        }))
        .unwrap();

        let Some(TextDocumentSyncCapability::Options(sync)) = options.text_document_sync else {
            panic!("esperado objeto de sincronização");
        };
        assert_eq!(sync.open_close, Some(true));
        assert_eq!(sync.change, Some(TextDocumentSyncKind::Incremental));
        assert_eq!(
            sync.save,
            Some(TextDocumentSaveCapability::Options(SaveOptions {
                include_text: Some(true)
            }))
        );
        assert_eq!(
            options.extra.get("futureCapability"),
            Some(&json!({"enabled": true}))
        );
    }

    #[test]
    fn server_capabilities_accept_boolean_save() {
        let capabilities: ServerCapabilities = serde_json::from_value(json!({
            "textDocumentSync": {"save": true}
        }))
        .unwrap();

        let Some(TextDocumentSyncCapability::Options(sync)) = capabilities.text_document_sync
        else {
            panic!("esperado objeto de sincronização");
        };
        assert_eq!(
            sync.save,
            Some(TextDocumentSaveCapability::IncludeText(true))
        );
    }

    #[test]
    fn did_open_and_did_change_round_trip() {
        let open = DidOpenTextDocumentParams {
            text_document: TextDocumentItem {
                uri: "file:///ação.rs".to_owned(),
                language_id: "rust".to_owned(),
                version: 3,
                text: "fn main() { println!(\"😀\"); }".to_owned(),
            },
        };

        let full_change = DidChangeTextDocumentParams {
            text_document: VersionedTextDocumentIdentifier {
                uri: open.text_document.uri.clone(),
                version: 4,
            },
            content_changes: vec![TextDocumentContentChangeEvent {
                range: None,
                range_length: None,
                text: "novo conteúdo".to_owned(),
            }],
        };

        let incremental_change = DidChangeTextDocumentParams {
            text_document: VersionedTextDocumentIdentifier {
                uri: open.text_document.uri.clone(),
                version: 5,
            },
            content_changes: vec![TextDocumentContentChangeEvent {
                range: Some(Range {
                    start: Position {
                        line: 0,
                        character: 3,
                    },
                    end: Position {
                        line: 0,
                        character: 7,
                    },
                }),
                range_length: Some(4),
                text: "ação".to_owned(),
            }],
        };

        for value in [
            serde_json::to_value(&open).unwrap(),
            serde_json::to_value(&full_change).unwrap(),
            serde_json::to_value(&incremental_change).unwrap(),
        ] {
            assert!(value.is_object());
        }

        let decoded: DidOpenTextDocumentParams =
            serde_json::from_value(serde_json::to_value(&open).unwrap()).unwrap();
        let decoded_full: DidChangeTextDocumentParams =
            serde_json::from_value(serde_json::to_value(&full_change).unwrap()).unwrap();
        let decoded_incremental: DidChangeTextDocumentParams =
            serde_json::from_value(serde_json::to_value(&incremental_change).unwrap()).unwrap();

        assert_eq!(decoded, open);
        assert_eq!(decoded_full, full_change);
        assert_eq!(decoded_incremental, incremental_change);
        assert!(decoded_incremental.content_changes[0].range.is_some());
        assert!(decoded_full.content_changes[0].range.is_none());
    }

    #[test]
    fn did_save_and_did_close_round_trip() {
        let save = DidSaveTextDocumentParams {
            text_document: TextDocumentIdentifier {
                uri: "file:///a.rs".to_owned(),
            },
            text: Some("conteúdo 😀".to_owned()),
        };
        let close = DidCloseTextDocumentParams {
            text_document: TextDocumentIdentifier {
                uri: "file:///a.rs".to_owned(),
            },
        };

        assert_eq!(
            serde_json::from_value::<DidSaveTextDocumentParams>(
                serde_json::to_value(&save).unwrap()
            )
            .unwrap(),
            save
        );
        assert_eq!(
            serde_json::from_value::<DidCloseTextDocumentParams>(
                serde_json::to_value(&close).unwrap()
            )
            .unwrap(),
            close
        );
    }

    #[test]
    fn publish_diagnostics_round_trips_with_numeric_severity() {
        let params = PublishDiagnosticsParams {
            uri: "file:///ação.rs".to_owned(),
            version: Some(9),
            diagnostics: vec![
                Diagnostic {
                    range: Range {
                        start: Position {
                            line: 1,
                            character: 2,
                        },
                        end: Position {
                            line: 1,
                            character: 5,
                        },
                    },
                    severity: Some(DiagnosticSeverity::Error),
                    code: Some(json!("E0001")),
                    source: Some("nexus".to_owned()),
                    message: "erro em ação 😀".to_owned(),
                    code_description: None,
                    tags: Vec::new(),
                    related_information: Vec::new(),
                    data: None,
                    extra: BTreeMap::new(),
                },
                Diagnostic {
                    range: Range {
                        start: Position {
                            line: 3,
                            character: 0,
                        },
                        end: Position {
                            line: 3,
                            character: 1,
                        },
                    },
                    severity: None,
                    code: None,
                    source: None,
                    message: "aviso".to_owned(),
                    code_description: None,
                    tags: Vec::new(),
                    related_information: Vec::new(),
                    data: None,
                    extra: BTreeMap::new(),
                },
            ],
            extra: BTreeMap::new(),
        };

        let encoded = serde_json::to_string(&params).unwrap();
        let decoded: PublishDiagnosticsParams = serde_json::from_str(&encoded).unwrap();

        assert_eq!(decoded, params);
        assert!(encoded.contains("\"severity\":1"));
        assert!(encoded.contains("\"version\":9"));
    }

    #[test]
    fn diagnostics_accept_all_severities_optional_fields_and_unknown_fields() {
        for (severity, expected) in [
            (1, DiagnosticSeverity::Error),
            (2, DiagnosticSeverity::Warning),
            (3, DiagnosticSeverity::Information),
            (4, DiagnosticSeverity::Hint),
        ] {
            let value = json!({"uri":"file:///a.py", "diagnostics":[{"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}},"severity":severity,"message":"m","futureField":{"x":true}}], "futurePublication":7});
            let decoded: PublishDiagnosticsParams = serde_json::from_value(value).unwrap();
            assert_eq!(decoded.diagnostics[0].severity, Some(expected));
            assert_eq!(
                decoded.diagnostics[0].extra["futureField"],
                json!({"x":true})
            );
            assert_eq!(decoded.extra["futurePublication"], json!(7));
        }
    }

    #[test]
    fn diagnostic_optional_members_and_code_shapes_are_preserved() {
        let base = json!({"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}},"message":"m"});
        let missing: Diagnostic = serde_json::from_value(base.clone()).unwrap();
        assert_eq!(missing.severity, None);
        assert_eq!(missing.code, None);
        assert!(missing.tags.is_empty());
        for code in [json!("E123"), json!(123)] {
            let mut value = base.clone();
            value["code"] = code.clone();
            value["codeDescription"] = json!({"href":"https://example.test/code"});
            value["tags"] = json!([1, 2]);
            value["relatedInformation"] = json!([{"location":{"uri":"file:///b.py","range":{"start":{"line":1,"character":2},"end":{"line":1,"character":3}}},"message":"related"}]);
            value["data"] = json!({"opaque":true});
            let decoded: Diagnostic = serde_json::from_value(value).unwrap();
            assert_eq!(decoded.code, Some(code));
            assert_eq!(
                decoded.tags,
                vec![DiagnosticTag::Unnecessary, DiagnosticTag::Deprecated]
            );
            assert_eq!(decoded.related_information.len(), 1);
            assert_eq!(decoded.data, Some(json!({"opaque":true})));
        }
        let empty: PublishDiagnosticsParams =
            serde_json::from_value(json!({"uri":"file:///a.py","diagnostics":[]})).unwrap();
        assert!(empty.version.is_none());
        assert!(empty.diagnostics.is_empty());
    }

    #[test]
    fn invalid_sync_kind_and_severity_are_rejected() {
        assert!(serde_json::from_value::<TextDocumentSyncKind>(json!(3)).is_err());
        assert!(serde_json::from_value::<DiagnosticSeverity>(json!(0)).is_err());
        assert!(serde_json::from_value::<DiagnosticSeverity>(json!("error")).is_err());
    }

    #[test]
    fn optional_fields_are_omitted_from_serialization() {
        let params = DidSaveTextDocumentParams {
            text_document: TextDocumentIdentifier {
                uri: "file:///a.rs".to_owned(),
            },
            text: None,
        };

        let value = serde_json::to_value(params).unwrap();
        assert!(value.get("text").is_none());
    }

    #[test]
    fn navigation_methods_expose_lsp_names_and_typed_parameters() {
        let document = TextDocumentIdentifier {
            uri: "file:///src/lib.rs".into(),
        };
        let position = Position {
            line: 2,
            character: 7,
        };
        let methods = [
            (NavigationMethod::Definition, "textDocument/definition"),
            (NavigationMethod::Declaration, "textDocument/declaration"),
            (
                NavigationMethod::TypeDefinition,
                "textDocument/typeDefinition",
            ),
            (
                NavigationMethod::Implementation,
                "textDocument/implementation",
            ),
            (
                NavigationMethod::References {
                    include_declaration: true,
                },
                "textDocument/references",
            ),
        ];

        for (method, expected_name) in methods {
            assert_eq!(method.lsp_method(), expected_name);
            let parameters = method.params(document.clone(), position);
            let value = serde_json::to_value(parameters).unwrap();
            assert_eq!(value["textDocument"]["uri"], "file:///src/lib.rs");
            assert_eq!(value["position"], json!({"line":2,"character":7}));
            if matches!(method, NavigationMethod::References { .. }) {
                assert_eq!(value["context"]["includeDeclaration"], true);
            } else {
                assert!(value.get("context").is_none());
            }
        }

        let references = NavigationMethod::References {
            include_declaration: false,
        }
        .params(document, position);
        assert_eq!(
            serde_json::to_value(references).unwrap()["context"]["includeDeclaration"],
            false
        );
    }

    #[test]
    fn navigation_provider_capabilities_accept_booleans_options_and_preserve_unknown_fields() {
        let capabilities: ServerCapabilities = serde_json::from_value(json!({
            "definitionProvider": true,
            "declarationProvider": {"workDoneProgress": true, "futureOption": 17},
            "typeDefinitionProvider": false,
            "implementationProvider": {},
            "referencesProvider": true,
            "futureProvider": {"enabled": true}
        }))
        .unwrap();

        assert!(capabilities.supports_navigation(NavigationMethod::Definition));
        assert!(capabilities.supports_navigation(NavigationMethod::Declaration));
        assert!(!capabilities.supports_navigation(NavigationMethod::TypeDefinition));
        assert!(capabilities.supports_navigation(NavigationMethod::Implementation));
        assert!(
            capabilities.supports_navigation(NavigationMethod::References {
                include_declaration: false
            })
        );
        assert_eq!(
            capabilities.extra["futureProvider"],
            json!({"enabled":true})
        );
        let Some(NavigationProviderCapability::Options(options)) =
            capabilities.declaration_provider
        else {
            panic!("opções de provider devem ser preservadas");
        };
        assert_eq!(options.work_done_progress, Some(true));
        assert_eq!(options.extra["futureOption"], json!(17));

        let absent = ServerCapabilities::default();
        assert!(!absent.supports_navigation(NavigationMethod::Definition));
        assert!(!absent.supports_navigation(NavigationMethod::References {
            include_declaration: true
        }));
    }

    #[test]
    fn navigation_provider_capabilities_reject_invalid_types() {
        assert!(
            serde_json::from_value::<ServerCapabilities>(json!({
                "definitionProvider": 1
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<ServerCapabilities>(json!({
                "referencesProvider": "yes"
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<NavigationProviderOptions>(json!({
                "workDoneProgress": "yes"
            }))
            .is_err()
        );
    }

    #[test]
    fn location_response_single_multiple_empty_and_null_are_normalized() {
        let single = json!({
            "uri":"file:///a.rs",
            "range":{"start":{"line":1,"character":2},"end":{"line":1,"character":5}}
        });
        let first = NavigationResult::from_goto_value(single.clone()).unwrap();
        assert_eq!(
            first,
            NavigationResult::Destinations(vec![NavigationTarget {
                uri: "file:///a.rs".into(),
                target_range: Range {
                    start: Position {
                        line: 1,
                        character: 2
                    },
                    end: Position {
                        line: 1,
                        character: 5
                    },
                },
                selection_range: Range {
                    start: Position {
                        line: 1,
                        character: 2
                    },
                    end: Position {
                        line: 1,
                        character: 5
                    },
                },
                origin_selection_range: None,
            }])
        );

        let second = json!({
            "uri":"file:///b.rs",
            "range":{"start":{"line":4,"character":0},"end":{"line":4,"character":2}}
        });
        let multiple = NavigationResult::from_goto_value(json!([single, second])).unwrap();
        let NavigationResult::Destinations(destinations) = multiple else {
            panic!("eram esperados dois destinos");
        };
        assert_eq!(destinations.len(), 2);
        assert_eq!(destinations[0].uri, "file:///a.rs");
        assert_eq!(destinations[1].uri, "file:///b.rs");

        for value in [json!(null), json!([])] {
            assert_eq!(
                NavigationResult::from_goto_value(value).unwrap(),
                NavigationResult::Empty
            );
        }
    }

    #[test]
    fn location_link_optional_origin_and_multiple_targets_normalize_without_losing_ranges() {
        let origin = json!({
            "start":{"line":0,"character":3},"end":{"line":0,"character":8}
        });
        let target = json!({
            "start":{"line":9,"character":0},"end":{"line":12,"character":1}
        });
        let selection = json!({
            "start":{"line":10,"character":4},"end":{"line":10,"character":9}
        });
        let with_origin = json!({
            "originSelectionRange":origin,
            "targetUri":"file:///target.rs",
            "targetRange":target,
            "targetSelectionRange":selection,
            "futureLinkField":"preserved"
        });
        let without_origin = json!({
            "targetUri":"file:///other.rs",
            "targetRange":target,
            "targetSelectionRange":selection
        });
        let decoded: LocationLink = serde_json::from_value(with_origin.clone()).unwrap();
        assert_eq!(decoded.origin_selection_range.unwrap().start.character, 3);
        assert_eq!(decoded.extra["futureLinkField"], json!("preserved"));
        let encoded = serde_json::to_value(&decoded).unwrap();
        assert_eq!(encoded["targetUri"], "file:///target.rs");
        assert!(encoded.get("originSelectionRange").is_some());

        let result =
            NavigationResult::from_goto_value(json!([with_origin, without_origin])).unwrap();
        let NavigationResult::Destinations(destinations) = result else {
            panic!("eram esperados destinos");
        };
        assert_eq!(destinations.len(), 2);
        assert_eq!(destinations[0].uri, "file:///target.rs");
        assert_eq!(destinations[0].target_range.start.line, 9);
        assert_eq!(destinations[0].selection_range.start.line, 10);
        assert_eq!(
            destinations[0]
                .origin_selection_range
                .unwrap()
                .start
                .character,
            3
        );
        assert_eq!(destinations[1].uri, "file:///other.rs");
        assert_eq!(destinations[1].origin_selection_range, None);
        let omitted_origin = serde_json::to_value(LocationLink {
            origin_selection_range: None,
            target_uri: "file:///other.rs".into(),
            target_range: destinations[1].target_range,
            target_selection_range: destinations[1].selection_range,
            extra: BTreeMap::new(),
        })
        .unwrap();
        assert!(omitted_origin.get("originSelectionRange").is_none());
        assert!(
            serde_json::from_value::<LocationLink>(json!({
                "targetUri":"file:///incomplete.rs",
                "targetRange":target
            }))
            .is_err()
        );
    }

    #[test]
    fn references_accept_only_location_lists_and_normalize_null_or_empty() {
        let location = json!({
            "uri":"file:///use.rs",
            "range":{"start":{"line":2,"character":1},"end":{"line":2,"character":4}}
        });
        let result = NavigationResult::from_references_value(json!([location])).unwrap();
        let NavigationResult::Destinations(destinations) = result else {
            panic!("esperada uma referência");
        };
        assert_eq!(destinations[0].uri, "file:///use.rs");
        assert_eq!(
            destinations[0].selection_range,
            destinations[0].target_range
        );
        assert_eq!(destinations[0].origin_selection_range, None);
        assert_eq!(
            NavigationResult::from_references_value(json!(null)).unwrap(),
            NavigationResult::Empty
        );
        assert_eq!(
            NavigationResult::from_references_value(json!([])).unwrap(),
            NavigationResult::Empty
        );
    }

    #[test]
    fn malformed_navigation_results_are_errors_and_mixed_arrays_are_rejected() {
        assert!(serde_json::from_str::<LocationLink>("{").is_err());
        let malformed = [
            json!({"uri":"file:///missing-range.rs"}),
            json!({"uri":7,"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}}}),
            json!({"uri":"file:///bad-position.rs","range":{"start":{"line":"zero","character":0},"end":{"line":0,"character":1}}}),
        ];
        for value in malformed {
            assert!(NavigationResult::from_goto_value(value).is_err());
        }

        let location = json!({
            "uri":"file:///a.rs",
            "range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}}
        });
        let link = json!({
            "targetUri":"file:///b.rs",
            "targetRange":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}},
            "targetSelectionRange":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}}
        });
        assert!(
            NavigationResult::from_goto_value(json!([location.clone(), link.clone()])).is_err()
        );
        assert!(NavigationResult::from_references_value(json!(location)).is_err());
        assert!(NavigationResult::from_goto_value(json!([link])).is_ok());

        let unsupported = NavigationResult::Unsupported;
        assert_eq!(unsupported, NavigationResult::Unsupported);
    }

    #[test]
    fn from_value_dispatches_protocol_shapes_for_all_five_navigation_methods() {
        let location = json!({
            "uri":"file:///definition.rs",
            "range":{"start":{"line":1,"character":2},"end":{"line":1,"character":6}}
        });
        let another_location = json!({
            "uri":"file:///other.rs",
            "range":{"start":{"line":4,"character":0},"end":{"line":4,"character":3}}
        });
        let link = json!({
            "originSelectionRange":{"start":{"line":0,"character":0},"end":{"line":0,"character":3}},
            "targetUri":"file:///linked.rs",
            "targetRange":{"start":{"line":7,"character":0},"end":{"line":9,"character":0}},
            "targetSelectionRange":{"start":{"line":8,"character":1},"end":{"line":8,"character":5}}
        });
        let goto_methods = [
            NavigationMethod::Definition,
            NavigationMethod::Declaration,
            NavigationMethod::TypeDefinition,
            NavigationMethod::Implementation,
        ];

        for method in goto_methods {
            assert!(matches!(
                NavigationResult::from_value(method, location.clone()).unwrap(),
                NavigationResult::Destinations(destinations)
                    if destinations.len() == 1 && destinations[0].uri == "file:///definition.rs"
            ));
            assert!(matches!(
                NavigationResult::from_value(
                    method,
                    json!([location.clone(), another_location.clone()])
                )
                .unwrap(),
                NavigationResult::Destinations(destinations) if destinations.len() == 2
            ));
            assert!(matches!(
                NavigationResult::from_value(method, json!([link.clone()])).unwrap(),
                NavigationResult::Destinations(destinations)
                    if destinations.len() == 1
                        && destinations[0].uri == "file:///linked.rs"
                        && destinations[0].selection_range.start.line == 8
            ));
            assert_eq!(
                NavigationResult::from_value(method, json!(null)).unwrap(),
                NavigationResult::Empty
            );
            assert_eq!(
                NavigationResult::from_value(method, json!([])).unwrap(),
                NavigationResult::Empty
            );
            assert!(NavigationResult::from_value(method, json!({"unexpected":true})).is_err());
            assert!(
                NavigationResult::from_value(method, json!({"uri":"file:///missing-range.rs"}))
                    .is_err()
            );
        }

        let references = NavigationMethod::References {
            include_declaration: true,
        };
        assert!(matches!(
            NavigationResult::from_value(references, json!([location.clone()])).unwrap(),
            NavigationResult::Destinations(destinations)
                if destinations.len() == 1 && destinations[0].uri == "file:///definition.rs"
        ));
        assert_eq!(
            NavigationResult::from_value(references, json!(null)).unwrap(),
            NavigationResult::Empty
        );
        assert_eq!(
            NavigationResult::from_value(references, json!([])).unwrap(),
            NavigationResult::Empty
        );
        assert!(NavigationResult::from_value(references, location.clone()).is_err());
        assert!(NavigationResult::from_value(references, json!([link.clone()])).is_err());
        assert!(NavigationResult::from_value(references, json!([location, link])).is_err());
        assert!(NavigationResult::from_value(references, json!([{"uri":7}])).is_err());
    }

    #[test]
    fn location_link_selection_range_must_be_contained_in_target_range() {
        let outside = json!({
            "targetUri":"file:///outside.rs",
            "targetRange":{"start":{"line":1,"character":0},"end":{"line":3,"character":0}},
            "targetSelectionRange":{"start":{"line":3,"character":0},"end":{"line":3,"character":1}}
        });
        assert!(
            NavigationResult::from_value(NavigationMethod::Definition, json!([outside])).is_err()
        );

        let reversed = json!({
            "targetUri":"file:///reversed.rs",
            "targetRange":{"start":{"line":1,"character":0},"end":{"line":3,"character":0}},
            "targetSelectionRange":{"start":{"line":2,"character":4},"end":{"line":2,"character":1}}
        });
        assert!(
            NavigationResult::from_value(NavigationMethod::Implementation, json!([reversed]))
                .is_err()
        );
    }
}
