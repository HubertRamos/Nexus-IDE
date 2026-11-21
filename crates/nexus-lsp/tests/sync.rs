//! Testes de integração da sincronização de documentos.
//!
//! O servidor simulado registra as notificações `textDocument/*` em um arquivo
//! JSONL. Uma requisição `fake/echo` é usada como barreira: quando ela responde,
//! todas as notificações anteriores já foram processadas e registradas.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use nexus_lsp::manager::{LspServer, ServerConfig, ServerError, ServerStatus};
use nexus_lsp::protocol::TextDocumentSyncKind;
use nexus_lsp::sync::{SaveSync, TextChange};
use serde_json::{Value, json};

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn sync_file(label: &str) -> PathBuf {
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "nexus-lsp-sync-{label}-{}-{counter}.jsonl",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    path
}

fn fake_config(mode: &str, sync_file: &Path) -> ServerConfig {
    ServerConfig::new("fake", env!("CARGO_BIN_EXE_nexus-lsp-fake-server"))
        .with_argument(mode)
        .with_environment("NEXUS_FAKE_LSP_SYNC_FILE", sync_file.as_os_str())
        .with_startup_timeout(Duration::from_secs(5))
        .with_request_timeout(Duration::from_secs(5))
        .with_shutdown_timeout(Duration::from_secs(2))
}

async fn barrier(server: &LspServer) {
    server
        .request("fake/echo", Some(json!({"barrier": true})))
        .await
        .expect("barreira de sincronização falhou");
}

fn read_events(path: &Path) -> Vec<Value> {
    let content = std::fs::read_to_string(path).unwrap_or_default();
    content
        .lines()
        .map(|line| serde_json::from_str(line).expect("evento JSON inválido"))
        .collect()
}

fn method(event: &Value) -> &str {
    event
        .get("method")
        .and_then(Value::as_str)
        .expect("evento sem método")
}

#[tokio::test]
async fn incremental_sync_sends_open_change_save_and_close() {
    let file = sync_file("incremental");
    let mut server = LspServer::start(fake_config("sync-incremental", &file))
        .await
        .unwrap();

    assert_eq!(
        server.sync_settings().change,
        TextDocumentSyncKind::Incremental
    );
    assert_eq!(
        server.sync_settings().save,
        SaveSync::Enabled { include_text: true }
    );

    let uri = "file:///tmp/a.rs";
    server.open_document(uri, "rust", "let x = 1;").unwrap();
    assert_eq!(server.open_document_count(), 1);
    assert!(server.is_document_open(uri));

    server
        .change_document(uri, &[TextChange::new(4, 5, "y")], "let y = 1;")
        .unwrap();
    server.save_document(uri, "let y = 1;").unwrap();
    server.close_document(uri).unwrap();

    assert_eq!(server.open_document_count(), 0);
    assert!(!server.is_document_open(uri));

    barrier(&server).await;
    let events = read_events(&file);
    assert_eq!(events.len(), 4, "eventos: {events:#?}");

    assert_eq!(method(&events[0]), "textDocument/didOpen");
    assert_eq!(events[0]["params"]["textDocument"]["uri"], uri);
    assert_eq!(events[0]["params"]["textDocument"]["languageId"], "rust");
    assert_eq!(events[0]["params"]["textDocument"]["version"], 1);
    assert_eq!(events[0]["params"]["textDocument"]["text"], "let x = 1;");

    assert_eq!(method(&events[1]), "textDocument/didChange");
    assert_eq!(events[1]["params"]["textDocument"]["version"], 2);
    assert_eq!(
        events[1]["params"]["contentChanges"][0]["range"],
        json!({"start": {"line": 0, "character": 4}, "end": {"line": 0, "character": 5}})
    );
    assert_eq!(events[1]["params"]["contentChanges"][0]["text"], "y");

    assert_eq!(method(&events[2]), "textDocument/didSave");
    assert_eq!(events[2]["params"]["textDocument"]["uri"], uri);
    assert_eq!(events[2]["params"]["text"], "let y = 1;");

    assert_eq!(method(&events[3]), "textDocument/didClose");
    assert_eq!(events[3]["params"]["textDocument"]["uri"], uri);

    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn full_sync_sends_the_entire_updated_document() {
    let file = sync_file("full");
    let mut server = LspServer::start(fake_config("sync-full", &file))
        .await
        .unwrap();

    assert_eq!(server.sync_settings().change, TextDocumentSyncKind::Full);

    let uri = "file:///tmp/full.rs";
    server.open_document(uri, "rust", "abc").unwrap();
    server
        .change_document(uri, &[TextChange::new(0, 1, "x")], "conteúdo completo")
        .unwrap();

    barrier(&server).await;
    let events = read_events(&file);
    assert_eq!(events.len(), 2);
    assert_eq!(method(&events[1]), "textDocument/didChange");
    assert_eq!(events[1]["params"]["textDocument"]["version"], 2);
    assert_eq!(
        events[1]["params"]["contentChanges"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert!(
        events[1]["params"]["contentChanges"][0]
            .get("range")
            .is_none()
    );
    assert_eq!(
        events[1]["params"]["contentChanges"][0]["text"],
        "conteúdo completo"
    );

    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn server_without_sync_capability_receives_nothing() {
    let file = sync_file("none");
    let mut server = LspServer::start(fake_config("sync-none", &file))
        .await
        .unwrap();

    assert_eq!(server.sync_settings().change, TextDocumentSyncKind::None);
    assert!(!server.sync_settings().tracks_documents());

    let uri = "file:///tmp/none.rs";
    server.open_document(uri, "rust", "a").unwrap();
    server
        .change_document(uri, &[TextChange::new(1, 1, "b")], "ab")
        .unwrap();
    server.save_document(uri, "ab").unwrap();
    server.close_document(uri).unwrap();

    barrier(&server).await;
    assert!(read_events(&file).is_empty());

    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn open_close_disabled_is_respected_but_changes_are_sent() {
    let file = sync_file("no-open-close");
    let mut server = LspServer::start(fake_config("sync-no-open-close", &file))
        .await
        .unwrap();

    assert!(!server.sync_settings().open_close);
    assert_eq!(
        server.sync_settings().save,
        SaveSync::Enabled {
            include_text: false
        }
    );

    let uri = "file:///tmp/no-open-close.rs";
    server.open_document(uri, "rust", "a").unwrap();
    server
        .change_document(uri, &[TextChange::new(1, 1, "b")], "ab")
        .unwrap();
    server.save_document(uri, "ab").unwrap();
    server.close_document(uri).unwrap();

    barrier(&server).await;
    let events = read_events(&file);
    assert_eq!(events.len(), 2, "eventos: {events:#?}");
    assert_eq!(method(&events[0]), "textDocument/didChange");
    assert_eq!(method(&events[1]), "textDocument/didSave");
    assert!(events[1]["params"].get("text").is_none());

    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn save_false_disables_did_save_but_keeps_open_change_and_close() {
    let file = sync_file("save-disabled");
    let mut server = LspServer::start(fake_config("sync-save-disabled", &file))
        .await
        .unwrap();

    assert_eq!(server.sync_settings().save, SaveSync::Disabled);

    let uri = "file:///tmp/save-disabled.rs";
    server.open_document(uri, "rust", "a").unwrap();
    server
        .change_document(uri, &[TextChange::new(1, 1, "b")], "ab")
        .unwrap();
    server.save_document(uri, "ab").unwrap();
    server.close_document(uri).unwrap();

    barrier(&server).await;
    let events = read_events(&file);
    let methods: Vec<&str> = events.iter().map(method).collect();
    assert_eq!(
        methods,
        vec![
            "textDocument/didOpen",
            "textDocument/didChange",
            "textDocument/didClose"
        ],
        "eventos: {events:#?}"
    );

    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn save_true_sends_did_save_without_assuming_include_text() {
    let file = sync_file("save-true");
    let mut server = LspServer::start(fake_config("sync-save-no-text", &file))
        .await
        .unwrap();

    assert_eq!(
        server.sync_settings().save,
        SaveSync::Enabled {
            include_text: false
        }
    );

    let uri = "file:///tmp/save-true.rs";
    server.open_document(uri, "rust", "a").unwrap();
    server.save_document(uri, "ab").unwrap();

    barrier(&server).await;
    let events = read_events(&file);
    assert_eq!(method(&events[1]), "textDocument/didSave");
    assert!(
        events[1]["params"].get("text").is_none(),
        "save:true não deve presumir includeText: {:#?}",
        events[1]
    );

    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn send_failure_invalidates_state_and_rejects_incremental_changes() {
    let file = sync_file("send-failure");
    let mut server = LspServer::start(fake_config("sync-incremental", &file))
        .await
        .unwrap();

    let uri = "file:///tmp/send-failure.rs";
    server.open_document(uri, "rust", "abc").unwrap();
    assert_eq!(server.open_document_count(), 1);

    server.fail_next_send_for_test();
    let error = server
        .change_document(uri, &[TextChange::new(1, 2, "X")], "aXc")
        .unwrap_err();

    assert!(matches!(error, ServerError::Closed), "erro: {error:?}");
    assert_eq!(server.status(), ServerStatus::Failed);
    assert_eq!(
        server.open_document_count(),
        0,
        "o estado não pode continuar como sincronizado"
    );
    assert!(server.last_transport_error().is_some());
    assert!(matches!(
        server.change_document(uri, &[TextChange::new(1, 2, "Y")], "aYc"),
        Err(ServerError::NotRunning)
    ));
    assert!(matches!(
        server.open_document(uri, "rust", "abc"),
        Err(ServerError::NotRunning)
    ));

    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_fresh_server_reopens_documents_after_a_previous_failure() {
    let failed_file = sync_file("failed-before-recovery");
    let mut failed = LspServer::start(fake_config("sync-incremental", &failed_file))
        .await
        .unwrap();
    let uri = "file:///tmp/recovered.rs";
    failed.open_document(uri, "rust", "abc").unwrap();
    failed.fail_next_send_for_test();
    let _ = failed.change_document(uri, &[TextChange::new(1, 2, "X")], "aXc");
    failed.shutdown().await.unwrap();

    let recovered_file = sync_file("recovered");
    let mut recovered = LspServer::start(fake_config("sync-incremental", &recovered_file))
        .await
        .unwrap();
    recovered.open_document(uri, "rust", "abc").unwrap();
    barrier(&recovered).await;

    let events = read_events(&recovered_file);
    assert_eq!(events.len(), 1);
    assert_eq!(method(&events[0]), "textDocument/didOpen");
    assert_eq!(events[0]["params"]["textDocument"]["version"], 1);
    assert_eq!(events[0]["params"]["textDocument"]["text"], "abc");

    recovered.shutdown().await.unwrap();
}

#[tokio::test]
async fn closing_one_document_keeps_the_shared_server_and_other_documents() {
    let file = sync_file("shared");
    let mut server = LspServer::start(fake_config("sync-incremental", &file))
        .await
        .unwrap();

    let first = "file:///tmp/a/main.rs";
    let second = "file:///tmp/b/main.rs";
    server.open_document(first, "rust", "a").unwrap();
    server.open_document(second, "rust", "b").unwrap();
    assert_eq!(server.open_document_count(), 2);

    server.close_document(first).unwrap();
    assert_eq!(server.open_document_count(), 1);
    assert!(
        server.is_running(),
        "o servidor compartilhado não deve encerrar"
    );
    assert!(server.is_document_open(second));

    server
        .change_document(second, &[TextChange::new(1, 1, "!")], "b!")
        .unwrap();

    barrier(&server).await;
    let events = read_events(&file);
    assert_eq!(events.len(), 4, "eventos: {events:#?}");
    assert_eq!(method(&events[0]), "textDocument/didOpen");
    assert_eq!(method(&events[1]), "textDocument/didOpen");
    assert_eq!(events[0]["params"]["textDocument"]["uri"], first);
    assert_eq!(events[1]["params"]["textDocument"]["uri"], second);
    assert_eq!(method(&events[2]), "textDocument/didClose");
    assert_eq!(events[2]["params"]["textDocument"]["uri"], first);
    assert_eq!(method(&events[3]), "textDocument/didChange");
    assert_eq!(events[3]["params"]["textDocument"]["uri"], second);

    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn unicode_and_multiple_edits_use_utf16_ranges() {
    let file = sync_file("unicode");
    let mut server = LspServer::start(fake_config("sync-incremental", &file))
        .await
        .unwrap();

    let uri = "file:///tmp/unicode.rs";
    server.open_document(uri, "rust", "a😀b\ncafé").unwrap();
    server
        .change_document(
            uri,
            &[TextChange::new(1, 2, "Z"), TextChange::new(7, 8, "e")],
            "aZb\ncafe",
        )
        .unwrap();

    barrier(&server).await;
    let events = read_events(&file);
    assert_eq!(events.len(), 2);
    assert_eq!(
        events[1]["params"]["contentChanges"][0]["range"],
        json!({"start": {"line": 0, "character": 1}, "end": {"line": 0, "character": 3}})
    );
    assert_eq!(
        events[1]["params"]["contentChanges"][1]["range"],
        json!({"start": {"line": 1, "character": 3}, "end": {"line": 1, "character": 4}})
    );

    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn synchronization_fails_when_the_server_is_not_running() {
    let file = sync_file("failure");
    let mut server = LspServer::start(fake_config("sync-incremental", &file))
        .await
        .unwrap();

    server.shutdown().await.unwrap();

    let error = server
        .open_document("file:///tmp/failure.rs", "rust", "a")
        .unwrap_err();
    assert!(matches!(error, ServerError::NotRunning));
    assert_eq!(server.open_document_count(), 0);
}
