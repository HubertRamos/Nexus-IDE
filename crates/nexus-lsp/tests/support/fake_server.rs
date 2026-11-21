//! Servidor LSP simulado usado apenas pelos testes de ciclo de vida.
//!
//! O binário fala JSON-RPC com enquadramento `Content-Length` sobre
//! `stdin`/`stdout` e aceita um modo como primeiro argumento. Ele não depende
//! de `rust-analyzer`, Pyright ou de qualquer servidor real.

use std::collections::HashMap;
use std::io::{self, BufRead, Write};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use serde_json::{Value, json};

fn main() {
    if let Some(path) = std::env::var_os("NEXUS_FAKE_LSP_PID_FILE") {
        let _ = std::fs::write(path, std::process::id().to_string());
    }

    let mode = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "normal".to_owned());

    if mode == "crash-init" {
        std::process::exit(5);
    }

    let output = Arc::new(Mutex::new(io::stdout()));
    let stdin = io::stdin();
    let mut input = stdin.lock();
    let mut documents: HashMap<String, String> = HashMap::new();

    while let Ok(Some(message)) = read_message(&mut input) {
        let method = message
            .get("method")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let id = message.get("id").cloned();
        let params = message.get("params").cloned().unwrap_or(Value::Null);

        match method.as_deref() {
            Some("initialize") => {
                if mode == "no-response-init" {
                    thread::sleep(Duration::from_secs(60));
                    break;
                }

                if mode == "fail-init" {
                    respond_error(
                        &output,
                        id.unwrap_or_else(|| json!(1)),
                        -32603,
                        "falha simulada de inicialização",
                        None,
                    );
                    continue;
                }

                if mode == "stderr-init" {
                    eprintln!("stderr durante initialize");
                }

                respond_result(
                    &output,
                    id.unwrap_or_else(|| json!(1)),
                    json!({
                        "capabilities": {
                            "textDocumentSync": sync_capability(&mode),
                            "positionEncoding": position_encoding(&mode),
                            "hoverProvider": true,
                            "fakeCapability": {"enabled": true},
                            "definitionProvider": mode.starts_with("navigation"),
                            "declarationProvider": mode.starts_with("navigation"),
                            "typeDefinitionProvider": mode.starts_with("navigation"),
                            "implementationProvider": mode.starts_with("navigation"),
                            "referencesProvider": mode.starts_with("navigation")
                        },
                        "serverInfo": {"name": "fake-lsp", "version": "0.1"}
                    }),
                );
            }
            Some("initialized") => {
                if mode == "crash-after-init" {
                    std::process::exit(7);
                }

                eprintln!("fake server initialized");
                send_notification(&output, "fake/ready", json!({"mode": mode}));
                send_request(
                    &output,
                    json!("server-request-1"),
                    "fake/ping",
                    json!({"message": "ping"}),
                );
            }
            Some("shutdown") => {
                append_marker("shutdown");

                if mode == "ignore-shutdown" {
                    thread::sleep(Duration::from_secs(60));
                    break;
                }

                if let Some(id) = id {
                    respond_result(&output, id, Value::Null);
                }
            }
            Some("exit") => {
                append_marker("exit");

                if mode == "ignore-exit" {
                    continue;
                }
                break;
            }
            Some(method) if id.is_none() => {
                if method == "$/cancelRequest" {
                    append_marker(&format!(
                        "cancel:{}",
                        params.get("id").unwrap_or(&Value::Null)
                    ));
                    continue;
                }
                if method.starts_with("textDocument/") {
                    append_sync_event(method, &params);
                    update_documents(method, &params, &mut documents);

                    if (mode == "publish-diagnostics" || mode == "publish-diagnostics-unversioned")
                        && method == "textDocument/didOpen"
                    {
                        let uri = params["textDocument"]["uri"].as_str().unwrap_or_default();
                        let mut publication = json!({
                            "uri": uri,
                            "diagnostics": [{
                                "range": {"start":{"line":0,"character":0},"end":{"line":0,"character":1}},
                                "severity": 1,
                                "code": "E-FAKE",
                                "source": "fake",
                                "message": "diagnóstico simulado"
                            }]
                        });
                        if mode == "publish-diagnostics" {
                            publication["version"] = json!(1);
                        }
                        send_notification(&output, "textDocument/publishDiagnostics", publication);
                    }

                    if mode == "exit-on-change" && method == "textDocument/didChange" {
                        std::process::exit(11);
                    }
                }
            }
            Some(method) if id.is_some() => {
                handle_request(
                    &output,
                    &mode,
                    method,
                    id.unwrap_or_else(|| json!(0)),
                    params,
                );
            }
            Some(_) | None => {}
        }
    }
}

fn append_marker(event: &str) {
    let Some(path) = std::env::var_os("NEXUS_FAKE_LSP_MARKER_FILE") else {
        return;
    };

    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(file, "{event}");
    }
}

fn append_sync_event(method: &str, params: &Value) {
    let Some(path) = std::env::var_os("NEXUS_FAKE_LSP_SYNC_FILE") else {
        return;
    };

    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(file, "{}", json!({"method": method, "params": params}));
    }
}

fn update_documents(method: &str, params: &Value, documents: &mut HashMap<String, String>) {
    let Some(uri) = params
        .get("textDocument")
        .and_then(|document| document.get("uri"))
        .and_then(Value::as_str)
    else {
        return;
    };

    let version = params
        .get("textDocument")
        .and_then(|document| document.get("version"));

    match method {
        "textDocument/didOpen" => {
            let text = params
                .get("textDocument")
                .and_then(|document| document.get("text"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            documents.insert(uri.to_owned(), text.to_owned());
        }
        "textDocument/didChange" => {
            let mut text = documents.get(uri).cloned().unwrap_or_default();
            if let Some(changes) = params.get("contentChanges").and_then(Value::as_array) {
                for change in changes {
                    if let Some(updated) = apply_change(&text, change) {
                        text = updated;
                    }
                }
            }
            documents.insert(uri.to_owned(), text);
        }
        "textDocument/didSave" => {
            if let Some(text) = params.get("text").and_then(Value::as_str) {
                documents.insert(uri.to_owned(), text.to_owned());
            }
        }
        _ => {}
    }

    let text = documents.get(uri).cloned().unwrap_or_default();
    append_snapshot(method, uri, &text, version);
}

fn apply_change(text: &str, change: &Value) -> Option<String> {
    let Some(range) = change.get("range") else {
        return change
            .get("text")
            .and_then(Value::as_str)
            .map(str::to_owned);
    };

    let start = range.get("start")?;
    let end = range.get("end")?;
    let start_offset = position_to_offset(
        text,
        start.get("line")?.as_u64()? as u32,
        start.get("character")?.as_u64()? as u32,
    )?;
    let end_offset = position_to_offset(
        text,
        end.get("line")?.as_u64()? as u32,
        end.get("character")?.as_u64()? as u32,
    )?;

    if start_offset > end_offset || end_offset > text.len() {
        return None;
    }

    let mut result = String::with_capacity(text.len());
    result.push_str(&text[..start_offset]);
    result.push_str(change.get("text")?.as_str()?);
    result.push_str(&text[end_offset..]);
    Some(result)
}

/// Converte uma posição LSP (linha e coluna UTF-16) em offset de bytes.
fn position_to_offset(text: &str, line: u32, character: u32) -> Option<usize> {
    let mut current_line = 0u32;
    let mut line_start = 0usize;
    let mut index = 0usize;

    while current_line < line {
        let newline = text[index..].find('\n')?;
        index += newline + 1;
        line_start = index;
        current_line += 1;
    }

    let line_end = text[line_start..]
        .find('\n')
        .map(|offset| line_start + offset)
        .unwrap_or(text.len());
    let mut content = &text[line_start..line_end];
    if let Some(stripped) = content.strip_suffix('\r') {
        content = stripped;
    }

    let mut units = 0u32;
    for (offset, current) in content.char_indices() {
        if units == character {
            return Some(line_start + offset);
        }
        units += current.len_utf16() as u32;
        if units > character {
            return None;
        }
    }

    if units == character {
        Some(line_start + content.len())
    } else {
        None
    }
}

fn append_snapshot(method: &str, uri: &str, text: &str, version: Option<&Value>) {
    let Some(path) = std::env::var_os("NEXUS_FAKE_LSP_SNAPSHOT_FILE") else {
        return;
    };

    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(
            file,
            "{}",
            json!({"method": method, "uri": uri, "text": text, "version": version})
        );
    }
}

fn position_encoding(mode: &str) -> &'static str {
    if mode == "position-utf8" {
        "utf-8"
    } else {
        "utf-16"
    }
}

fn sync_capability(mode: &str) -> Value {
    match mode {
        "sync-none" => json!(0),
        "sync-full" => json!({
            "openClose": true,
            "change": 1,
            "save": {"includeText": true}
        }),
        "sync-no-open-close" => json!({
            "change": 2,
            "save": {"includeText": false}
        }),
        "sync-save-no-text" => json!({
            "openClose": true,
            "change": 2,
            "save": true
        }),
        "sync-save-disabled" => json!({
            "openClose": true,
            "change": 2,
            "save": false
        }),
        "sync-save-empty" => json!({
            "openClose": true,
            "change": 2,
            "save": {}
        }),
        "sync-save-object-no-text" => json!({
            "openClose": true,
            "change": 2,
            "save": {"includeText": false}
        }),
        _ => json!({
            "openClose": true,
            "change": 2,
            "save": {"includeText": true}
        }),
    }
}

fn handle_request(
    output: &Arc<Mutex<io::Stdout>>,
    mode: &str,
    method: &str,
    id: Value,
    params: Value,
) {
    match method {
        "fake/echo" => respond_result(output, id, params),
        "fake/delayed" => {
            let output = Arc::clone(output);
            let value = params.get("value").cloned().unwrap_or(Value::Null);
            let delay = params.get("delay_ms").and_then(Value::as_u64).unwrap_or(0);
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(delay));
                respond_result(&output, id, json!({"value": value}));
            });
        }
        "fake/error" => respond_error(output, id, -32000, "erro simulado", Some(params)),
        "fake/notification" => {
            send_notification(output, "fake/event", params);
            respond_result(output, id, Value::Null);
        }
        "fake/stderr" => {
            eprintln!("stderr sob demanda: {params}");
            respond_result(output, id, Value::Null);
        }
        "fake/exit" => std::process::exit(9),
        "fake/never" => {}
        method if method.starts_with("textDocument/") && mode.starts_with("navigation") => {
            if mode == "navigation-exit" {
                std::process::exit(14);
            }
            let output = Arc::clone(output);
            let uri = params["textDocument"]["uri"]
                .as_str()
                .unwrap_or("file:///unknown")
                .to_owned();
            let position = params["position"].clone();
            let character = position["character"].as_u64().unwrap_or_default();
            let is_references = method == "textDocument/references";
            let delay = if mode == "navigation-slow" {
                250
            } else if character == 1 {
                180
            } else {
                10
            };
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(delay));
                let line = position["line"].as_u64().unwrap_or_default();
                let start = position["character"].as_u64().unwrap_or_default();
                let location = json!({
                    "uri": uri,
                    "range": {
                        "start": {"line": line, "character": start},
                        "end": {"line": line, "character": start + 1}
                    }
                });
                respond_result(
                    &output,
                    id,
                    if is_references {
                        json!([location])
                    } else {
                        location
                    },
                );
            });
        }
        _ => respond_error(output, id, -32601, "método não encontrado", None),
    }
}

fn read_message(input: &mut impl BufRead) -> io::Result<Option<Value>> {
    let mut content_length = None;

    loop {
        let mut line = String::new();
        let read = input.read_line(&mut line)?;
        if read == 0 {
            return Ok(None);
        }

        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            break;
        }

        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            content_length = value.trim().parse::<usize>().ok();
        }
    }

    let length = content_length
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "Content-Length ausente"))?;
    let mut payload = vec![0; length];
    input.read_exact(&mut payload)?;
    serde_json::from_slice(&payload)
        .map(Some)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn write_message(output: &Arc<Mutex<io::Stdout>>, message: &Value) -> io::Result<()> {
    let payload = serde_json::to_vec(message)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let mut output = output
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    write!(output, "Content-Length: {}\r\n\r\n", payload.len())?;
    output.write_all(&payload)?;
    output.flush()
}

fn respond_result(output: &Arc<Mutex<io::Stdout>>, id: Value, result: Value) {
    let _ = write_message(
        output,
        &json!({"jsonrpc": "2.0", "id": id, "result": result}),
    );
}

fn respond_error(
    output: &Arc<Mutex<io::Stdout>>,
    id: Value,
    code: i64,
    message: &str,
    data: Option<Value>,
) {
    let mut error = json!({"code": code, "message": message});
    if let Some(data) = data {
        error["data"] = data;
    }
    let _ = write_message(output, &json!({"jsonrpc": "2.0", "id": id, "error": error}));
}

fn send_notification(output: &Arc<Mutex<io::Stdout>>, method: &str, params: Value) {
    let _ = write_message(
        output,
        &json!({"jsonrpc": "2.0", "method": method, "params": params}),
    );
}

fn send_request(output: &Arc<Mutex<io::Stdout>>, id: Value, method: &str, params: Value) {
    let _ = write_message(
        output,
        &json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}),
    );
}
