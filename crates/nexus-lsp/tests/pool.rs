//! Testes do compartilhamento de servidores por workspace e linguagem.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use nexus_lsp::manager::ServerConfig;
use nexus_lsp::pool::{ServerKey, ServerPool};

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn sync_file(label: &str) -> PathBuf {
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "nexus-lsp-pool-{label}-{}-{counter}.jsonl",
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

#[tokio::test]
async fn same_workspace_and_language_reuses_the_process() {
    let file = sync_file("reuse");
    let config = fake_config("sync-incremental", &file);
    let key = ServerKey::new("/tmp/workspace", "rust");
    let mut pool = ServerPool::new();

    let first_pid = pool
        .ensure(key.clone(), config.clone())
        .await
        .unwrap()
        .process_id()
        .unwrap();
    let second_pid = pool
        .ensure(key.clone(), config)
        .await
        .unwrap()
        .process_id()
        .unwrap();

    assert_eq!(first_pid, second_pid);
    assert_eq!(pool.len(), 1);
    assert!(pool.contains(&key));

    let errors = pool.shutdown_all().await;
    assert!(errors.is_empty(), "falhas ao encerrar: {errors:?}");
    assert!(pool.is_empty());
}

#[tokio::test]
async fn different_languages_get_independent_servers() {
    let file = sync_file("languages");
    let config = fake_config("sync-incremental", &file);
    let rust = ServerKey::new("/tmp/workspace", "rust");
    let python = ServerKey::new("/tmp/workspace", "python");
    let mut pool = ServerPool::new();

    let rust_pid = pool
        .ensure(rust.clone(), config.clone())
        .await
        .unwrap()
        .process_id()
        .unwrap();
    let python_pid = pool
        .ensure(python.clone(), config)
        .await
        .unwrap()
        .process_id()
        .unwrap();

    assert_ne!(rust_pid, python_pid);
    assert_eq!(pool.len(), 2);
    assert!(pool.contains(&rust));
    assert!(pool.contains(&python));

    let errors = pool.shutdown_all().await;
    assert!(errors.is_empty(), "falhas ao encerrar: {errors:?}");
}

#[tokio::test]
async fn closing_one_document_keeps_the_shared_server_alive() {
    let file = sync_file("close-one");
    let config = fake_config("sync-incremental", &file);
    let key = ServerKey::new("/tmp/workspace", "rust");
    let mut pool = ServerPool::new();

    {
        let server = pool.ensure(key.clone(), config).await.unwrap();
        server
            .open_document("file:///tmp/a/main.rs", "rust", "a")
            .unwrap();
        server
            .open_document("file:///tmp/b/main.rs", "rust", "b")
            .unwrap();
        assert_eq!(server.open_document_count(), 2);

        server.close_document("file:///tmp/a/main.rs").unwrap();
        assert!(server.is_running());
        assert_eq!(server.open_document_count(), 1);
        assert!(server.is_document_open("file:///tmp/b/main.rs"));
    }

    assert_eq!(pool.len(), 1);
    let errors = pool.shutdown_all().await;
    assert!(errors.is_empty(), "falhas ao encerrar: {errors:?}");
}

#[tokio::test]
async fn shutdown_all_removes_every_server() {
    let file = sync_file("shutdown");
    let config = fake_config("sync-incremental", &file);
    let mut pool = ServerPool::new();

    pool.ensure(ServerKey::new("/tmp/a", "rust"), config.clone())
        .await
        .unwrap();
    pool.ensure(ServerKey::new("/tmp/b", "rust"), config)
        .await
        .unwrap();
    assert_eq!(pool.len(), 2);

    let errors = pool.shutdown_all().await;
    assert!(errors.is_empty(), "falhas ao encerrar: {errors:?}");
    assert!(pool.is_empty());
}
