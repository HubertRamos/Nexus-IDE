//! Mensagens JSON-RPC 2.0 utilizadas pelo protocolo LSP.
//!
//! O módulo define apenas o envelope JSON-RPC. Semântica de métodos LSP,
//! despacho e ciclo de vida de servidores pertencem às etapas seguintes.

use std::fmt;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

/// Versão exigida pelo JSON-RPC 2.0 e pelo LSP.
pub const JSONRPC_VERSION: &str = "2.0";

/// Códigos de erro definidos pelo JSON-RPC 2.0.
pub mod error_codes {
    pub const PARSE_ERROR: i64 = -32700;
    pub const INVALID_REQUEST: i64 = -32600;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
    pub const INTERNAL_ERROR: i64 = -32603;
}

/// Identificador de uma requisição ou resposta.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Id {
    Number(i64),
    String(String),
}

impl From<i64> for Id {
    fn from(value: i64) -> Self {
        Self::Number(value)
    }
}

impl From<i32> for Id {
    fn from(value: i32) -> Self {
        Self::Number(i64::from(value))
    }
}

impl From<u32> for Id {
    fn from(value: u32) -> Self {
        Self::Number(i64::from(value))
    }
}

impl From<String> for Id {
    fn from(value: String) -> Self {
        Self::String(value)
    }
}

impl From<&str> for Id {
    fn from(value: &str) -> Self {
        Self::String(value.to_owned())
    }
}

impl fmt::Display for Id {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Number(value) => write!(formatter, "{value}"),
            Self::String(value) => write!(formatter, "{value}"),
        }
    }
}

/// Presença de um campo opcional em uma mensagem JSON.
///
/// Diferencia três estados que `Option<T>` não consegue representar quando
/// desserializado: campo ausente, campo presente com `null` e campo presente
/// com valor. O estado `Absent` deve ser omitido pelo campo que o contém.
#[derive(Debug, Clone, PartialEq)]
pub enum Presence<T> {
    Absent,
    Null,
    Value(T),
}

impl<T> Presence<T> {
    pub const fn absent() -> Self {
        Self::Absent
    }

    pub const fn null() -> Self {
        Self::Null
    }

    pub const fn value(value: T) -> Self {
        Self::Value(value)
    }

    pub const fn is_absent(&self) -> bool {
        matches!(self, Self::Absent)
    }

    pub const fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }

    pub const fn is_present(&self) -> bool {
        !self.is_absent()
    }

    pub fn as_value(&self) -> Option<&T> {
        match self {
            Self::Value(value) => Some(value),
            Self::Absent | Self::Null => None,
        }
    }

    pub fn into_value(self) -> Option<T> {
        match self {
            Self::Value(value) => Some(value),
            Self::Absent | Self::Null => None,
        }
    }
}

impl<T: Serialize> Serialize for Presence<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Absent | Self::Null => serializer.serialize_none(),
            Self::Value(value) => value.serialize(serializer),
        }
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Presence<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // Um campo ausente nem chega aqui: `#[serde(default)]` produz
        // `Absent`. Um campo presente com `null` chega e produz `Null`.
        Option::<T>::deserialize(deserializer).map(|value| value.map_or(Self::Null, Self::Value))
    }
}

/// Objeto de erro JSON-RPC.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ErrorObject {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl ErrorObject {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    pub fn with_data(mut self, data: Value) -> Self {
        self.data = Some(data);
        self
    }
}

impl fmt::Display for ErrorObject {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "erro {}: {}", self.code, self.message)
    }
}

impl std::error::Error for ErrorObject {}

/// Requisição JSON-RPC: espera uma resposta com o mesmo `id`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub jsonrpc: String,
    pub id: Id,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

impl Request {
    pub fn new(id: impl Into<Id>, method: impl Into<String>) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_owned(),
            id: id.into(),
            method: method.into(),
            params: None,
        }
    }

    pub fn with_params<T: Serialize>(mut self, params: &T) -> Result<Self, serde_json::Error> {
        self.params = Some(serde_json::to_value(params)?);
        Ok(self)
    }

    pub fn params_as<T: DeserializeOwned>(&self) -> Result<T, ErrorObject> {
        let value = self.params.clone().unwrap_or(Value::Null);
        serde_json::from_value(value).map_err(|error| {
            ErrorObject::new(
                error_codes::INVALID_PARAMS,
                format!("parâmetros inválidos: {error}"),
            )
        })
    }

    pub fn validate(&self) -> Result<(), ErrorObject> {
        validate_version(&self.jsonrpc)?;
        validate_method(&self.method)
    }
}

/// Notificação JSON-RPC: não espera resposta e não possui `id`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Notification {
    pub jsonrpc: String,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

impl Notification {
    pub fn new(method: impl Into<String>) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_owned(),
            method: method.into(),
            params: None,
        }
    }

    pub fn with_params<T: Serialize>(mut self, params: &T) -> Result<Self, serde_json::Error> {
        self.params = Some(serde_json::to_value(params)?);
        Ok(self)
    }

    pub fn validate(&self) -> Result<(), ErrorObject> {
        validate_version(&self.jsonrpc)?;
        validate_method(&self.method)
    }
}

/// Resposta JSON-RPC: contém exatamente um entre `result` e `error`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    pub jsonrpc: String,
    pub id: Id,
    #[serde(
        default = "Presence::absent",
        skip_serializing_if = "Presence::is_absent"
    )]
    pub result: Presence<Value>,
    #[serde(
        default = "Presence::absent",
        skip_serializing_if = "Presence::is_absent"
    )]
    pub error: Presence<ErrorObject>,
}

impl Response {
    pub fn success(id: impl Into<Id>, result: Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_owned(),
            id: id.into(),
            result: match result {
                Value::Null => Presence::Null,
                value => Presence::Value(value),
            },
            error: Presence::Absent,
        }
    }

    pub fn failure(id: impl Into<Id>, error: ErrorObject) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_owned(),
            id: id.into(),
            result: Presence::Absent,
            error: Presence::Value(error),
        }
    }

    pub fn validate(&self) -> Result<(), ErrorObject> {
        validate_version(&self.jsonrpc)?;

        if self.error.is_null() {
            return Err(invalid_request("`error` não pode ser `null`"));
        }

        match (self.result.is_absent(), self.error.is_absent()) {
            (true, true) => Err(invalid_request("resposta deve conter `result` ou `error`")),
            (false, false) => Err(invalid_request(
                "resposta não pode conter `result` e `error` ao mesmo tempo",
            )),
            _ => Ok(()),
        }
    }
}

/// Envelope de qualquer mensagem JSON-RPC recebida.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum Message {
    Request(Request),
    Notification(Notification),
    Response(Response),
}

impl Message {
    /// Interpreta um payload JSON-RPC completo e valida o envelope.
    pub fn from_slice(bytes: &[u8]) -> Result<Self, ErrorObject> {
        if bytes.iter().all(u8::is_ascii_whitespace) {
            return Err(ErrorObject::new(
                error_codes::PARSE_ERROR,
                "mensagem JSON-RPC vazia",
            ));
        }

        let value: Value = serde_json::from_slice(bytes).map_err(|error| {
            ErrorObject::new(
                error_codes::PARSE_ERROR,
                format!("JSON malformado: {error}"),
            )
        })?;

        Self::from_json(value)
    }

    /// Classifica um valor JSON já decodificado como requisição, notificação
    /// ou resposta.
    pub fn from_json(value: Value) -> Result<Self, ErrorObject> {
        if !value.is_object() {
            return Err(invalid_request("mensagem deve ser um objeto JSON"));
        }

        let has_method = value.get("method").is_some();
        let has_id = value.get("id").is_some();

        if has_method {
            if has_id {
                let request: Request = serde_json::from_value(value)
                    .map_err(|error| invalid_request(format!("requisição inválida: {error}")))?;
                request.validate()?;
                Ok(Self::Request(request))
            } else {
                let notification: Notification = serde_json::from_value(value)
                    .map_err(|error| invalid_request(format!("notificação inválida: {error}")))?;
                notification.validate()?;
                Ok(Self::Notification(notification))
            }
        } else if has_id || value.get("result").is_some() || value.get("error").is_some() {
            let response: Response = serde_json::from_value(value)
                .map_err(|error| invalid_request(format!("resposta inválida: {error}")))?;
            response.validate()?;
            Ok(Self::Response(response))
        } else {
            Err(invalid_request(
                "mensagem deve conter `method`, `result` ou `error`",
            ))
        }
    }
}

fn validate_version(version: &str) -> Result<(), ErrorObject> {
    if version == JSONRPC_VERSION {
        Ok(())
    } else {
        Err(invalid_request(format!(
            "versão JSON-RPC inválida: `{version}` (esperado `{JSONRPC_VERSION}`)"
        )))
    }
}

fn validate_method(method: &str) -> Result<(), ErrorObject> {
    if method.is_empty() {
        Err(invalid_request("método JSON-RPC não pode ser vazio"))
    } else {
        Ok(())
    }
}

fn invalid_request(message: impl Into<String>) -> ErrorObject {
    ErrorObject::new(error_codes::INVALID_REQUEST, message)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn request_with_numeric_id_round_trips() {
        let request = Request::new(7, "initialize")
            .with_params(&json!({"processId": null}))
            .unwrap();

        let encoded = serde_json::to_vec(&request).unwrap();
        let decoded = Message::from_slice(&encoded).unwrap();

        assert_eq!(decoded, Message::Request(request));
    }

    #[test]
    fn request_with_textual_id_round_trips() {
        let request = Request::new("req-1", "shutdown");

        let encoded = serde_json::to_vec(&request).unwrap();
        let decoded = Message::from_slice(&encoded).unwrap();

        assert_eq!(decoded, Message::Request(request));
    }

    #[test]
    fn notification_round_trips_without_id() {
        let notification = Notification::new("textDocument/didOpen")
            .with_params(&json!({"textDocument": {"uri": "file:///a.rs"}}))
            .unwrap();

        let encoded = serde_json::to_vec(&notification).unwrap();
        let decoded = Message::from_slice(&encoded).unwrap();

        assert_eq!(decoded, Message::Notification(notification));
    }

    #[test]
    fn successful_and_failed_responses_round_trip() {
        let success = Response::success(1, json!({"ok": true}));
        let failure = Response::failure(
            2,
            ErrorObject::new(error_codes::METHOD_NOT_FOUND, "método ausente")
                .with_data(json!({"method": "x"})),
        );

        for response in [success, failure] {
            let encoded = serde_json::to_vec(&response).unwrap();
            let decoded = Message::from_slice(&encoded).unwrap();
            assert_eq!(decoded, Message::Response(response));
        }
    }

    #[test]
    fn params_are_available_as_typed_values() {
        let request = Request::new(1, "initialize")
            .with_params(&json!({"processId": 42}))
            .unwrap();

        #[derive(Deserialize)]
        struct Params {
            #[serde(rename = "processId")]
            process_id: i64,
        }

        let params: Params = request.params_as().unwrap();
        assert_eq!(params.process_id, 42);
    }

    #[test]
    fn malformed_and_empty_json_are_reported_without_panicking() {
        let malformed = Message::from_slice(b"{not json").unwrap_err();
        let empty = Message::from_slice(b"").unwrap_err();
        let whitespace = Message::from_slice(b" \r\n\t").unwrap_err();

        assert_eq!(malformed.code, error_codes::PARSE_ERROR);
        assert_eq!(empty.code, error_codes::PARSE_ERROR);
        assert_eq!(whitespace.code, error_codes::PARSE_ERROR);
    }

    #[test]
    fn invalid_envelopes_are_rejected() {
        let cases = [
            json!([]),
            json!({}),
            json!({"jsonrpc": "1.0", "id": 1, "method": "x"}),
            json!({"jsonrpc": "2.0", "id": 1}),
            json!({"jsonrpc": "2.0", "method": ""}),
            json!({"jsonrpc": "2.0", "id": 1, "method": 3}),
            json!({"jsonrpc": "2.0", "id": 1, "result": 1, "error": {"code": 1, "message": "x"}}),
            json!({"jsonrpc": "2.0", "id": 1}),
            json!({"jsonrpc": "2.0", "result": 1}),
        ];

        for case in cases {
            assert!(Message::from_json(case).is_err());
        }
    }

    #[test]
    fn utf8_content_is_preserved_in_messages() {
        let notification = Notification::new("textDocument/didSave")
            .with_params(&json!({"textDocument": {"uri": "file:///ação.rs"}, "text": "😀 café"}))
            .unwrap();

        let encoded = serde_json::to_vec(&notification).unwrap();
        let decoded = Message::from_slice(&encoded).unwrap();

        assert_eq!(decoded, Message::Notification(notification));
        assert!(encoded.windows(4).any(|window| window == "😀".as_bytes()));
    }

    #[test]
    fn null_result_is_accepted_as_a_successful_response() {
        let message = Message::from_json(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": null
        }))
        .unwrap();

        let Message::Response(response) = message else {
            panic!("esperada resposta");
        };
        assert_eq!(response.result, Presence::Null);
        assert_eq!(response.error, Presence::Absent);
    }

    #[test]
    fn null_result_survives_serialization_round_trip() {
        let response = Response::success(1, Value::Null);

        let encoded = serde_json::to_string(&response).unwrap();
        assert!(encoded.contains("\"result\":null"));
        assert!(!encoded.contains("\"error\""));

        let decoded = Message::from_slice(encoded.as_bytes()).unwrap();
        assert_eq!(decoded, Message::Response(response));
    }

    #[test]
    fn response_without_result_and_error_is_rejected() {
        let error = Message::from_json(json!({
            "jsonrpc": "2.0",
            "id": 1
        }))
        .unwrap_err();

        assert_eq!(error.code, error_codes::INVALID_REQUEST);
    }

    #[test]
    fn null_error_is_rejected() {
        let error = Message::from_json(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "error": null
        }))
        .unwrap_err();

        assert_eq!(error.code, error_codes::INVALID_REQUEST);
    }

    #[test]
    fn request_and_response_with_null_id_are_rejected() {
        let request = br#"{"jsonrpc":"2.0","id":null,"method":"x"}"#;
        let response = br#"{"jsonrpc":"2.0","id":null,"result":1}"#;

        assert!(Message::from_slice(request).is_err());
        assert!(Message::from_slice(response).is_err());
    }

    #[test]
    fn result_and_error_together_are_rejected_even_with_null() {
        let cases = [
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "result": null,
                "error": {"code": -32603, "message": "falha"}
            }),
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "result": 1,
                "error": null
            }),
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "result": null,
                "error": null
            }),
        ];

        for case in cases {
            let error = Message::from_json(case).unwrap_err();
            assert_eq!(error.code, error_codes::INVALID_REQUEST);
        }
    }

    #[test]
    fn valid_error_response_round_trips_without_result() {
        let response = Response::failure(
            7,
            ErrorObject::new(error_codes::METHOD_NOT_FOUND, "método ausente")
                .with_data(json!({"method": "x"})),
        );

        let encoded = serde_json::to_string(&response).unwrap();
        assert!(!encoded.contains("\"result\""));

        let decoded = Message::from_slice(encoded.as_bytes()).unwrap();
        assert_eq!(decoded, Message::Response(response));
    }

    #[test]
    fn valid_non_null_result_response_round_trips() {
        let response = Response::success(8, json!({"ok": true, "texto": "ação"}));

        let encoded = serde_json::to_string(&response).unwrap();
        assert!(encoded.contains("\"result\":"));

        let decoded = Message::from_slice(encoded.as_bytes()).unwrap();
        assert_eq!(decoded, Message::Response(response));
    }

    #[test]
    fn malformed_error_object_is_rejected() {
        let cases = [
            json!({"jsonrpc": "2.0", "id": 1, "error": {"code": 1}}),
            json!({"jsonrpc": "2.0", "id": 1, "error": 42}),
            json!({"jsonrpc": "2.0", "id": 1, "error": []}),
        ];

        for case in cases {
            assert!(Message::from_json(case).is_err());
        }
    }

    #[test]
    fn numeric_id_boundaries_round_trip() {
        for id in [i64::MIN, i64::MAX] {
            let request = Request::new(id, "x");
            let encoded = serde_json::to_vec(&request).unwrap();

            assert_eq!(
                Message::from_slice(&encoded).unwrap(),
                Message::Request(request)
            );
        }
    }

    #[test]
    fn numeric_id_outside_i64_range_is_rejected() {
        let cases: &[&[u8]] = &[
            br#"{"jsonrpc":"2.0","id":9223372036854775808,"method":"x"}"#,
            br#"{"jsonrpc":"2.0","id":-9223372036854775809,"method":"x"}"#,
        ];

        for case in cases {
            assert!(Message::from_slice(case).is_err());
        }
    }

    #[test]
    fn textual_ids_include_empty_and_long_values() {
        let long_id = "a".repeat(16 * 1024);

        for id in [String::new(), long_id] {
            let request = Request::new(id, "x");
            let encoded = serde_json::to_vec(&request).unwrap();

            assert_eq!(
                Message::from_slice(&encoded).unwrap(),
                Message::Request(request)
            );
        }
    }
}
