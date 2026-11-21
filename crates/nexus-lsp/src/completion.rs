//! Modelos tipados do protocolo LSP para IntelliSense.
//!
//! Este módulo normaliza as respostas de `textDocument/completion` sem
//! descartar campos específicos de servidores. A camada de apresentação pode
//! consumir os campos comuns e manter os dados extras disponíveis para etapas
//! futuras (snippets, text edits, comandos e importações automáticas).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::protocol::{Position, TextDocumentIdentifier};

/// Contexto que explica por que o cliente solicitou sugestões.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompletionContext {
    /// 1 = Invoked, 2 = TriggerCharacter, 3 = TriggerForIncompleteCompletions.
    pub trigger_kind: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger_character: Option<String>,
}

/// Parâmetros de `textDocument/completion`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompletionParams {
    pub text_document: TextDocumentIdentifier,
    pub position: Position,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<CompletionContext>,
}

/// Conteúdo de documentação no formato Markdown ou plaintext do LSP.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarkupContent {
    /// "plaintext" ou "markdown".
    pub kind: String,
    pub value: String,
}

/// Documentação pode ser uma string simples ou conteúdo estruturado.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CompletionDocumentation {
    String(String),
    Markup(MarkupContent),
}

/// Texto de inserção opcionalmente associado a um formato de snippet.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompletionItem {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub documentation: Option<CompletionDocumentation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deprecated: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preselect: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sort_text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter_text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub insert_text: Option<String>,
    /// 1 = PlainText, 2 = Snippet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub insert_text_format: Option<u8>,
    /// Mantém textEdit, additionalTextEdits, command, data e extensões
    /// específicas do servidor sem acoplar o núcleo a cada variante do LSP.
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

/// Lista de sugestões devolvida pelo servidor.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompletionList {
    #[serde(default)]
    pub is_incomplete: bool,
    pub items: Vec<CompletionItem>,
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

/// O LSP aceita tanto um array simples quanto um objeto CompletionList.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CompletionResponse {
    List(CompletionList),
    Items(Vec<CompletionItem>),
}

impl CompletionResponse {
    /// Itens em ordem recebida, sem alterar a ordenação do servidor.
    pub fn items(&self) -> &[CompletionItem] {
        match self {
            Self::List(list) => &list.items,
            Self::Items(items) => items,
        }
    }

    /// Informa se o servidor marcou a lista como incompleta.
    pub fn is_incomplete(&self) -> bool {
        matches!(self, Self::List(list) if list.is_incomplete)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completion_params_use_lsp_camel_case_fields() {
        let params = CompletionParams {
            text_document: TextDocumentIdentifier {
                uri: "file:///workspace/main.py".to_owned(),
            },
            position: Position {
                line: 4,
                character: 8,
            },
            context: Some(CompletionContext {
                trigger_kind: 2,
                trigger_character: Some(".".to_owned()),
            }),
        };

        let value = serde_json::to_value(params).unwrap();
        assert_eq!(value["textDocument"]["uri"], "file:///workspace/main.py");
        assert_eq!(value["position"]["line"], 4);
        assert_eq!(value["position"]["character"], 8);
        assert_eq!(value["context"]["triggerKind"], 2);
        assert_eq!(value["context"]["triggerCharacter"], ".");
    }

    #[test]
    fn parses_completion_array_with_documentation_and_snippet_fields() {
        let response: CompletionResponse = serde_json::from_value(serde_json::json!([
            {
                "label": "println!",
                "kind": 3,
                "detail": "Macro",
                "documentation": {
                    "kind": "markdown",
                    "value": "Writes formatted text."
                },
                "insertText": "println!(${1:value});",
                "insertTextFormat": 2,
                "sortText": "001",
                "filterText": "println",
                "textEdit": {
                    "newText": "println!(${1:value});",
                    "range": {
                        "start": { "line": 0, "character": 0 },
                        "end": { "line": 0, "character": 3 }
                    }
                }
            }
        ]))
        .unwrap();

        assert_eq!(response.items().len(), 1);
        assert_eq!(response.items()[0].label, "println!");
        assert_eq!(response.items()[0].kind, Some(3));
        assert_eq!(response.items()[0].insert_text_format, Some(2));
        assert!(matches!(
            &response.items()[0].documentation,
            Some(CompletionDocumentation::Markup(markup))
                if markup.kind == "markdown" && markup.value == "Writes formatted text."
        ));
        assert!(response.items()[0].extra.contains_key("textEdit"));
        assert!(!response.is_incomplete());
    }

    #[test]
    fn parses_completion_list_and_preserves_server_extensions() {
        let response: CompletionResponse = serde_json::from_value(serde_json::json!({
            "isIncomplete": true,
            "items": [
                {
                    "label": "Usuario",
                    "documentation": "Classe de domínio",
                    "deprecated": false,
                    "data": { "resolveId": 42 }
                }
            ],
            "x-server-metadata": { "source": "pyright" }
        }))
        .unwrap();

        assert!(response.is_incomplete());
        assert_eq!(response.items()[0].label, "Usuario");
        assert!(matches!(
            &response.items()[0].documentation,
            Some(CompletionDocumentation::String(text)) if text == "Classe de domínio"
        ));
        let CompletionResponse::List(list) = response else {
            panic!("objeto deveria ser interpretado como CompletionList");
        };
        assert_eq!(list.extra["x-server-metadata"]["source"], "pyright");
        assert_eq!(list.items[0].extra["data"]["resolveId"], 42);
    }

    #[test]
    fn empty_completion_array_is_a_valid_response() {
        let response: CompletionResponse = serde_json::from_value(serde_json::json!([])).unwrap();
        assert!(response.items().is_empty());
        assert!(!response.is_incomplete());
    }
}
