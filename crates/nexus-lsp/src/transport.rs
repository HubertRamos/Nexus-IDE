//! Transporte de mensagens LSP sobre STDIO.
//!
//! O enquadramento segue o protocolo LSP: cada mensagem possui um cabeçalho
//! `Content-Length` (em bytes UTF-8) separado do payload por `\r\n\r\n`.
//! Leituras são orientadas a bytes e nunca a linhas.

use std::fmt;

use tokio::io::{
    AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader, ReadHalf, WriteHalf,
};

use crate::jsonrpc::{ErrorObject, Message};

/// Limite padrão para o bloco de cabeçalhos.
pub const DEFAULT_MAX_HEADER_BYTES: usize = 8 * 1024;
/// Limite padrão para o payload de uma mensagem (16 MiB).
pub const DEFAULT_MAX_CONTENT_LENGTH: usize = 16 * 1024 * 1024;

/// Limites aplicados ao enquadramento.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub max_header_bytes: usize,
    pub max_content_length: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_header_bytes: DEFAULT_MAX_HEADER_BYTES,
            max_content_length: DEFAULT_MAX_CONTENT_LENGTH,
        }
    }
}

impl Limits {
    pub fn new(max_header_bytes: usize, max_content_length: usize) -> Self {
        Self {
            max_header_bytes,
            max_content_length,
        }
    }
}

/// Falhas de transporte ou de validação do payload JSON-RPC.
#[derive(Debug)]
pub enum TransportError {
    Io(std::io::Error),
    HeaderTooLarge { limit: usize },
    TruncatedHeader,
    InvalidHeader(String),
    MissingContentLength,
    DuplicateContentLength,
    InvalidContentLength(String),
    ContentLengthTooLarge { length: usize, limit: usize },
    IncompletePayload { expected: usize },
    InvalidMessage(ErrorObject),
    Serialize(String),
}

impl fmt::Display for TransportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "falha de I/O no transporte LSP: {error}"),
            Self::HeaderTooLarge { limit } => {
                write!(formatter, "cabeçalho LSP excede o limite de {limit} bytes")
            }
            Self::TruncatedHeader => write!(formatter, "cabeçalho LSP incompleto"),
            Self::InvalidHeader(detail) => {
                write!(formatter, "cabeçalho LSP inválido: {detail}")
            }
            Self::MissingContentLength => {
                write!(formatter, "cabeçalho LSP sem `Content-Length`")
            }
            Self::DuplicateContentLength => {
                write!(formatter, "cabeçalho LSP com `Content-Length` duplicado")
            }
            Self::InvalidContentLength(value) => {
                write!(formatter, "`Content-Length` inválido: `{value}`")
            }
            Self::ContentLengthTooLarge { length, limit } => write!(
                formatter,
                "payload LSP de {length} bytes excede o limite de {limit} bytes"
            ),
            Self::IncompletePayload { expected } => write!(
                formatter,
                "payload LSP incompleto: esperados {expected} bytes"
            ),
            Self::InvalidMessage(error) => {
                write!(formatter, "mensagem JSON-RPC inválida: {error}")
            }
            Self::Serialize(error) => {
                write!(formatter, "não foi possível serializar a mensagem: {error}")
            }
        }
    }
}

impl std::error::Error for TransportError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::InvalidMessage(error) => Some(error),
            _ => None,
        }
    }
}

impl From<std::io::Error> for TransportError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

/// Leitor assíncrono de mensagens LSP.
pub struct LspReader<R> {
    reader: BufReader<R>,
    limits: Limits,
}

impl<R: AsyncRead + Unpin> LspReader<R> {
    pub fn new(reader: R) -> Self {
        Self::with_limits(reader, Limits::default())
    }

    pub fn with_limits(reader: R, limits: Limits) -> Self {
        Self {
            reader: BufReader::new(reader),
            limits,
        }
    }

    /// Lê um payload completo. Retorna `None` somente em EOF limpo antes de um
    /// novo cabeçalho.
    pub async fn read_frame(&mut self) -> Result<Option<Vec<u8>>, TransportError> {
        read_frame(&mut self.reader, self.limits).await
    }

    /// Lê e valida uma mensagem JSON-RPC completa.
    pub async fn read_message(&mut self) -> Result<Option<Message>, TransportError> {
        let Some(payload) = self.read_frame().await? else {
            return Ok(None);
        };

        Message::from_slice(&payload)
            .map(Some)
            .map_err(TransportError::InvalidMessage)
    }
}

/// Escritor assíncrono de mensagens LSP.
pub struct LspWriter<W> {
    writer: W,
    limits: Limits,
}

impl<W: AsyncWrite + Unpin> LspWriter<W> {
    pub fn new(writer: W) -> Self {
        Self::with_limits(writer, Limits::default())
    }

    pub fn with_limits(writer: W, limits: Limits) -> Self {
        Self { writer, limits }
    }

    /// Escreve um payload já serializado com o cabeçalho `Content-Length`.
    pub async fn write_frame(&mut self, payload: &[u8]) -> Result<(), TransportError> {
        write_frame(&mut self.writer, payload, self.limits).await
    }

    /// Serializa e escreve uma mensagem JSON-RPC.
    pub async fn write_message(&mut self, message: &Message) -> Result<(), TransportError> {
        let payload = serde_json::to_vec(message)
            .map_err(|error| TransportError::Serialize(error.to_string()))?;
        self.write_frame(&payload).await
    }
}

/// Par leitor/escritor sobre a mesma conexão.
pub struct LspTransport<R, W> {
    reader: LspReader<R>,
    writer: LspWriter<W>,
}

impl<R: AsyncRead + Unpin, W: AsyncWrite + Unpin> LspTransport<R, W> {
    pub fn new(reader: R, writer: W) -> Self {
        Self::with_limits(reader, writer, Limits::default())
    }

    pub fn with_limits(reader: R, writer: W, limits: Limits) -> Self {
        Self {
            reader: LspReader::with_limits(reader, limits),
            writer: LspWriter::with_limits(writer, limits),
        }
    }

    pub async fn read_frame(&mut self) -> Result<Option<Vec<u8>>, TransportError> {
        self.reader.read_frame().await
    }

    pub async fn read_message(&mut self) -> Result<Option<Message>, TransportError> {
        self.reader.read_message().await
    }

    pub async fn write_frame(&mut self, payload: &[u8]) -> Result<(), TransportError> {
        self.writer.write_frame(payload).await
    }

    pub async fn write_message(&mut self, message: &Message) -> Result<(), TransportError> {
        self.writer.write_message(message).await
    }

    /// Separa leitura e escrita para permitir tarefas concorrentes.
    pub fn split(self) -> (LspReader<R>, LspWriter<W>) {
        (self.reader, self.writer)
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> LspTransport<ReadHalf<S>, WriteHalf<S>> {
    /// Cria um transporte a partir de um stream bidirecional.
    pub fn from_stream(stream: S) -> Self {
        let (reader, writer) = tokio::io::split(stream);
        Self::new(reader, writer)
    }
}

async fn read_frame<R: AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
    limits: Limits,
) -> Result<Option<Vec<u8>>, TransportError> {
    let mut header = Vec::new();

    loop {
        let mut byte = [0u8; 1];
        let read = reader.read(&mut byte).await?;

        if read == 0 {
            if header.is_empty() {
                return Ok(None);
            }
            return Err(TransportError::TruncatedHeader);
        }

        header.push(byte[0]);

        if header.len() > limits.max_header_bytes {
            return Err(TransportError::HeaderTooLarge {
                limit: limits.max_header_bytes,
            });
        }

        if header.ends_with(b"\r\n\r\n") {
            break;
        }
    }

    let header_text = std::str::from_utf8(&header[..header.len() - 4]).map_err(|_| {
        TransportError::InvalidHeader("cabeçalho não contém texto UTF-8 válido".to_owned())
    })?;

    let mut content_length = None;
    for line in header_text.split("\r\n") {
        let (name, value) = line.split_once(':').ok_or_else(|| {
            TransportError::InvalidHeader(format!("linha sem separador `:`: `{line}`"))
        })?;

        let name = name.trim_matches(|character: char| character == ' ' || character == '\t');
        if name.is_empty() {
            return Err(TransportError::InvalidHeader(
                "nome de cabeçalho vazio".to_owned(),
            ));
        }

        let value = value.trim_matches(|character: char| character == ' ' || character == '\t');

        if name.eq_ignore_ascii_case("content-length") {
            if content_length.is_some() {
                return Err(TransportError::DuplicateContentLength);
            }

            if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(TransportError::InvalidContentLength(value.to_owned()));
            }

            let length: usize = value
                .parse()
                .map_err(|_| TransportError::InvalidContentLength(value.to_owned()))?;
            content_length = Some(length);
        }
    }

    let length = content_length.ok_or(TransportError::MissingContentLength)?;
    if length > limits.max_content_length {
        return Err(TransportError::ContentLengthTooLarge {
            length,
            limit: limits.max_content_length,
        });
    }

    let mut payload = vec![0; length];
    reader.read_exact(&mut payload).await.map_err(|error| {
        if error.kind() == std::io::ErrorKind::UnexpectedEof {
            TransportError::IncompletePayload { expected: length }
        } else {
            TransportError::Io(error)
        }
    })?;

    Ok(Some(payload))
}

async fn write_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    payload: &[u8],
    limits: Limits,
) -> Result<(), TransportError> {
    if payload.len() > limits.max_content_length {
        return Err(TransportError::ContentLengthTooLarge {
            length: payload.len(),
            limit: limits.max_content_length,
        });
    }

    let header = format!("Content-Length: {}\r\n\r\n", payload.len());
    writer.write_all(header.as_bytes()).await?;
    writer.write_all(payload).await?;
    writer.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt, duplex};

    use super::*;
    use crate::jsonrpc::{Request, Response};

    fn frame(payload: &[u8]) -> Vec<u8> {
        let mut bytes = format!("Content-Length: {}\r\n\r\n", payload.len()).into_bytes();
        bytes.extend_from_slice(payload);
        bytes
    }

    async fn read_frame_from(
        bytes: &[u8],
        limits: Limits,
    ) -> Result<Option<Vec<u8>>, TransportError> {
        let (input, mut output) = duplex(4096);
        output.write_all(bytes).await.unwrap();
        drop(output);

        let mut reader = LspReader::with_limits(input, limits);
        reader.read_frame().await
    }

    async fn read_message_from(bytes: &[u8]) -> Result<Option<Message>, TransportError> {
        let (input, mut output) = duplex(4096);
        output.write_all(bytes).await.unwrap();
        drop(output);

        let mut reader = LspReader::new(input);
        reader.read_message().await
    }

    #[tokio::test]
    async fn writes_content_length_in_utf8_bytes() {
        let (mut input, output) = duplex(4096);
        let payload = serde_json::to_vec(&json!({"text": "ação 😀"})).unwrap();
        let expected_length = payload.len();

        let mut writer = LspWriter::new(output);
        writer.write_frame(&payload).await.unwrap();
        drop(writer);

        let mut bytes = Vec::new();
        input.read_to_end(&mut bytes).await.unwrap();

        let header = format!("Content-Length: {expected_length}\r\n\r\n");
        assert!(bytes.starts_with(header.as_bytes()));
        assert_eq!(&bytes[header.len()..], payload.as_slice());
        assert!(expected_length > "ação 😀".chars().count());
    }

    #[tokio::test]
    async fn reads_headers_and_payload_split_across_fragments() {
        let (input, mut output) = duplex(4096);
        let payload = r#"{"jsonrpc":"2.0","method":"x","params":{"text":"ação 😀"}}"#.as_bytes();
        let bytes = frame(payload);

        let writer = tokio::spawn(async move {
            for fragment in bytes.chunks(3) {
                output.write_all(fragment).await.unwrap();
                tokio::task::yield_now().await;
            }
        });

        let mut reader = LspReader::new(input);
        let message = reader.read_message().await.unwrap().unwrap();

        let Message::Notification(notification) = message else {
            panic!("esperada notificação");
        };
        assert_eq!(notification.method, "x");
        writer.await.unwrap();
    }

    #[tokio::test]
    async fn reads_multiple_consecutive_messages() {
        let (input, mut output) = duplex(4096);
        let first = br#"{"jsonrpc":"2.0","method":"first"}"#;
        let second = br#"{"jsonrpc":"2.0","method":"second"}"#;

        let mut bytes = frame(first);
        bytes.extend_from_slice(&frame(second));
        output.write_all(&bytes).await.unwrap();

        let mut reader = LspReader::new(input);

        for expected in ["first", "second"] {
            let message = reader.read_message().await.unwrap().unwrap();
            let Message::Notification(notification) = message else {
                panic!("esperada notificação");
            };
            assert_eq!(notification.method, expected);
        }
    }

    #[tokio::test]
    async fn transport_round_trips_requests_and_responses() {
        let (client, server) = duplex(4096);
        let (client_read, client_write) = tokio::io::split(client);
        let (server_read, server_write) = tokio::io::split(server);

        let mut client = LspTransport::new(client_read, client_write);
        let mut server = LspTransport::new(server_read, server_write);

        let request = Request::new(1, "initialize");
        client
            .write_message(&Message::Request(request.clone()))
            .await
            .unwrap();
        assert_eq!(
            server.read_message().await.unwrap().unwrap(),
            Message::Request(request)
        );

        let response = Response::success(1, json!({"ok": true}));
        server
            .write_message(&Message::Response(response.clone()))
            .await
            .unwrap();
        assert_eq!(
            client.read_message().await.unwrap().unwrap(),
            Message::Response(response)
        );
    }

    #[tokio::test]
    async fn clean_eof_returns_none() {
        let (input, output) = duplex(16);
        drop(output);

        let mut reader = LspReader::new(input);
        assert!(reader.read_frame().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn accepts_extra_headers_and_case_insensitive_name() {
        let payload = br#"{"jsonrpc":"2.0","method":"x"}"#;
        let mut bytes = format!(
            "content-length: {}\r\nContent-Type: application/vscode-jsonrpc\r\n\r\n",
            payload.len()
        )
        .into_bytes();
        bytes.extend_from_slice(payload);

        let frame = read_frame_from(&bytes, Limits::default()).await.unwrap();
        assert_eq!(frame.as_deref(), Some(payload.as_slice()));
    }

    #[tokio::test]
    async fn rejects_invalid_headers() {
        let cases: &[&[u8]] = &[
            b"Content-Length 5\r\n\r\nhello",
            b": 5\r\n\r\nhello",
            b"Content-Length: 5\r\nContent-Length: 5\r\n\r\nhello",
            b"Content-Length: abc\r\n\r\nhello",
            b"Content-Length: -1\r\n\r\nhello",
            b"Content-Length: 1\r\nContent-Length: 2\r\n\r\nhi",
        ];

        for case in cases {
            let result = read_frame_from(case, Limits::default()).await;
            assert!(result.is_err(), "caso aceito indevidamente: {case:?}");
        }
    }

    #[tokio::test]
    async fn rejects_missing_content_length() {
        let result = read_frame_from(
            b"Content-Type: application/json\r\n\r\n{}",
            Limits::default(),
        )
        .await;

        assert!(matches!(result, Err(TransportError::MissingContentLength)));
    }

    #[tokio::test]
    async fn rejects_line_delimited_framing() {
        let result = read_frame_from(b"Content-Length: 5\n\nhello", Limits::default()).await;

        assert!(matches!(result, Err(TransportError::TruncatedHeader)));
    }

    #[tokio::test]
    async fn rejects_incomplete_payload() {
        let result = read_frame_from(b"Content-Length: 10\r\n\r\nabc", Limits::default()).await;

        assert!(matches!(
            result,
            Err(TransportError::IncompletePayload { expected: 10 })
        ));
    }

    #[tokio::test]
    async fn rejects_oversized_content_length_before_reading_payload() {
        let limits = Limits {
            max_content_length: 4,
            ..Limits::default()
        };
        let result = read_frame_from(b"Content-Length: 5\r\n\r\n", limits).await;

        assert!(matches!(
            result,
            Err(TransportError::ContentLengthTooLarge {
                length: 5,
                limit: 4
            })
        ));
    }

    #[tokio::test]
    async fn rejects_oversized_headers() {
        let limits = Limits {
            max_header_bytes: 16,
            ..Limits::default()
        };
        let result = read_frame_from(
            b"Content-Length: 1\r\nX-Padding: 1234567890\r\n\r\nx",
            limits,
        )
        .await;

        assert!(matches!(
            result,
            Err(TransportError::HeaderTooLarge { limit: 16 })
        ));
    }

    #[tokio::test]
    async fn rejects_invalid_utf8_in_headers() {
        let result = read_frame_from(
            b"Content-Length: 1\r\nX-\xff: y\r\n\r\nx",
            Limits::default(),
        )
        .await;

        assert!(matches!(result, Err(TransportError::InvalidHeader(_))));
    }

    #[tokio::test]
    async fn malformed_json_and_empty_payload_are_reported() {
        let malformed = read_message_from(&frame(b"{not json")).await;
        let empty = read_message_from(&frame(b"")).await;
        let invalid = read_message_from(&frame(br#"{"jsonrpc":"2.0"}"#)).await;

        assert!(matches!(
            malformed,
            Err(TransportError::InvalidMessage(error))
                if error.code == crate::jsonrpc::error_codes::PARSE_ERROR
        ));
        assert!(matches!(empty, Err(TransportError::InvalidMessage(_))));
        assert!(matches!(
            invalid,
            Err(TransportError::InvalidMessage(error))
                if error.code == crate::jsonrpc::error_codes::INVALID_REQUEST
        ));
    }

    #[tokio::test]
    async fn write_rejects_payload_above_limit() {
        let (input, output) = duplex(64);
        let limits = Limits {
            max_content_length: 2,
            ..Limits::default()
        };
        let mut writer = LspWriter::with_limits(output, limits);

        let result = writer.write_frame(b"abc").await;

        assert!(matches!(
            result,
            Err(TransportError::ContentLengthTooLarge {
                length: 3,
                limit: 2
            })
        ));
        drop(input);
    }

    #[tokio::test]
    async fn empty_payload_is_a_valid_frame_but_not_a_valid_message() {
        let (input, mut output) = duplex(64);
        output
            .write_all(b"Content-Length: 0\r\n\r\n")
            .await
            .unwrap();
        drop(output);

        let mut reader = LspReader::new(input);
        assert_eq!(reader.read_frame().await.unwrap(), Some(Vec::new()));
    }

    #[allow(dead_code)]
    fn assert_send<T: Send>(_: T) {}

    #[test]
    fn transport_types_are_send() {
        let (input, output) = duplex(16);
        assert_send(input);
        assert_send(output);
    }
}
