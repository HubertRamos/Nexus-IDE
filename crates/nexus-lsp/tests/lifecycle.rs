//! Testes de integração do ciclo de vida de servidores LSP.
//!
//! O servidor simulado é um binário de apoio (`nexus-lsp-fake-server`) e não
//! depende de `rust-analyzer` ou Pyright.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use nexus_lsp::jsonrpc::Id;
use nexus_lsp::manager::{
    ExitReason, LspServer, ServerConfig, ServerError, ServerEvent, ServerStatus,
};
use nexus_lsp::protocol::{
    NavigationMethod, Position, TextDocumentSyncCapability, TextDocumentSyncKind,
};
use nexus_lsp::service::{LspService, NavigationOperationId, NavigationOutcome, ServiceEvent};
use nexus_lsp::sync::TextChange;
use serde::Deserialize;
use serde_json::json;
use tokio::time::timeout;

fn fake_config(mode: &str) -> ServerConfig {
    ServerConfig::new("fake", env!("CARGO_BIN_EXE_nexus-lsp-fake-server"))
        .with_argument(mode)
        .with_startup_timeout(Duration::from_secs(5))
        .with_request_timeout(Duration::from_secs(5))
        .with_shutdown_timeout(Duration::from_secs(2))
}

fn pid_file(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("nexus-lsp-{name}-{}.pid", std::process::id()))
}

fn with_pid_file(config: ServerConfig, path: &Path) -> ServerConfig {
    config.with_environment("NEXUS_FAKE_LSP_PID_FILE", path.as_os_str())
}

fn with_marker_file(config: ServerConfig, path: &Path) -> ServerConfig {
    config.with_environment("NEXUS_FAKE_LSP_MARKER_FILE", path.as_os_str())
}

fn wait_service_event(
    service: &LspService,
    mut predicate: impl FnMut(&ServiceEvent) -> bool,
) -> ServiceEvent {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        for event in service.drain() {
            if predicate(&event) {
                return event;
            }
        }
        assert!(Instant::now() < deadline, "evento do serviço não chegou");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn wait_server_started(service: &LspService) -> i64 {
    match wait_service_event(service, |event| {
        matches!(event, ServiceEvent::ServerStarted { .. })
    }) {
        ServiceEvent::ServerStarted { session_id, .. } => session_id,
        _ => unreachable!(),
    }
}

fn wait_navigation_events(
    service: &LspService,
    count: usize,
) -> Vec<(NavigationOperationId, i64, Option<i64>, NavigationOutcome)> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut completions = Vec::new();
    while completions.len() < count {
        for event in service.drain() {
            if let ServiceEvent::NavigationCompleted { context, outcome } = event {
                completions.push((
                    context.operation_id,
                    context.session_id.unwrap_or_default(),
                    context.document_version,
                    outcome,
                ));
            }
        }
        assert!(
            Instant::now() < deadline,
            "resposta de navegação não chegou"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    completions
}

fn read_pid(path: &Path) -> u32 {
    std::fs::read_to_string(path)
        .expect("arquivo de PID não foi criado")
        .trim()
        .parse()
        .expect("PID inválido")
}

#[cfg(unix)]
fn process_exists(pid: u32) -> bool {
    std::process::Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

async fn next_matching<F>(server: &mut LspServer, mut predicate: F) -> ServerEvent
where
    F: FnMut(&ServerEvent) -> bool,
{
    let deadline = Instant::now() + Duration::from_secs(5);

    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let event = timeout(remaining, server.next_event())
            .await
            .expect("evento esperado não chegou")
            .expect("canal de eventos fechado");

        if predicate(&event) {
            return event;
        }
    }
}

#[tokio::test]
async fn start_completes_handshake_and_exposes_capabilities() {
    let mut server = LspServer::start(fake_config("normal")).await.unwrap();

    assert_eq!(server.status(), ServerStatus::Running);
    assert!(server.is_running());
    assert_eq!(server.language_id(), "fake");
    assert!(server.process_id().is_some());

    let capabilities = server.capabilities();
    let Some(TextDocumentSyncCapability::Options(sync)) = &capabilities.text_document_sync else {
        panic!("esperado objeto textDocumentSync");
    };
    assert_eq!(sync.open_close, Some(true));
    assert_eq!(sync.change, Some(TextDocumentSyncKind::Incremental));
    assert_eq!(capabilities.position_encoding.as_deref(), Some("utf-16"));
    assert_eq!(
        capabilities.extra.get("fakeCapability"),
        Some(&json!({"enabled": true}))
    );
    assert_eq!(server.server_info().unwrap().name, "fake-lsp");

    let mut saw_ready = false;
    let mut saw_request = false;
    let mut saw_stderr = false;

    while !(saw_ready && saw_request && saw_stderr) {
        let event = next_matching(&mut server, |event| {
            matches!(
                event,
                ServerEvent::Notification(_) | ServerEvent::Request(_) | ServerEvent::Stderr(_)
            )
        })
        .await;

        match event {
            ServerEvent::Notification(notification) if notification.method == "fake/ready" => {
                saw_ready = true;
                assert_eq!(notification.params.unwrap()["mode"], "normal");
            }
            ServerEvent::Request(request) if request.method == "fake/ping" => {
                saw_request = true;
                server
                    .respond_ok(request.id, json!({"pong": true}))
                    .unwrap();
            }
            ServerEvent::Stderr(line) if line.contains("initialized") => {
                saw_stderr = true;
            }
            _ => {}
        }
    }

    assert!(
        server
            .stderr_snapshot()
            .iter()
            .any(|line| line.contains("initialized"))
    );

    server.shutdown().await.unwrap();
    assert_eq!(server.status(), ServerStatus::Exited);
    assert!(server.process_id().is_none());
}

#[tokio::test]
async fn requests_are_matched_by_id_even_out_of_order() {
    let mut server = LspServer::start(fake_config("normal")).await.unwrap();

    let slow = server.request(
        "fake/delayed",
        Some(json!({"value": "slow", "delay_ms": 120})),
    );
    let fast = server.request(
        "fake/delayed",
        Some(json!({"value": "fast", "delay_ms": 10})),
    );

    let (slow, fast) = tokio::join!(slow, fast);
    assert_eq!(slow.unwrap()["value"], "slow");
    assert_eq!(fast.unwrap()["value"], "fast");

    #[derive(Deserialize)]
    struct Echo {
        value: String,
    }

    let echo: Echo = server
        .request_typed("fake/echo", json!({"value": "ação"}))
        .await
        .unwrap();
    assert_eq!(echo.value, "ação");

    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn error_responses_are_reported_without_losing_the_id() {
    let mut server = LspServer::start(fake_config("normal")).await.unwrap();

    let error = server
        .request("fake/error", Some(json!({"detail": "x"})))
        .await
        .unwrap_err();

    let ServerError::Response(error) = error else {
        panic!("esperado erro JSON-RPC, recebido {error:?}");
    };
    assert_eq!(error.code, -32000);
    assert_eq!(error.message, "erro simulado");
    assert_eq!(error.data, Some(json!({"detail": "x"})));

    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn server_notifications_and_requests_are_distinct_events() {
    let mut server = LspServer::start(fake_config("normal")).await.unwrap();

    let notification = next_matching(&mut server, |event| {
        matches!(event, ServerEvent::Notification(_))
    })
    .await;
    let ServerEvent::Notification(notification) = notification else {
        unreachable!();
    };
    assert_eq!(notification.method, "fake/ready");

    let request = next_matching(&mut server, |event| {
        matches!(event, ServerEvent::Request(_))
    })
    .await;
    let ServerEvent::Request(request) = request else {
        unreachable!();
    };
    assert_eq!(request.method, "fake/ping");
    assert_eq!(request.id, Id::String("server-request-1".to_owned()));

    server
        .respond_ok(request.id, json!({"pong": true}))
        .unwrap();

    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn stderr_is_drained_without_blocking_requests() {
    let mut server = LspServer::start(fake_config("stderr-init")).await.unwrap();

    next_matching(&mut server, |event| {
        matches!(event, ServerEvent::Stderr(line) if line.contains("stderr durante initialize"))
    })
    .await;

    server
        .request("fake/stderr", Some(json!("linha sob demanda")))
        .await
        .unwrap();

    next_matching(
        &mut server,
        |event| matches!(event, ServerEvent::Stderr(line) if line.contains("stderr sob demanda")),
    )
    .await;

    assert!(
        server
            .stderr_snapshot()
            .iter()
            .any(|line| line.contains("linha sob demanda"))
    );

    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn missing_executable_is_reported_clearly() {
    let path = std::env::temp_dir().join(format!(
        "nexus-lsp-missing-{}-{}",
        std::process::id(),
        Instant::now().elapsed().as_nanos()
    ));
    let _ = std::fs::remove_file(&path);

    let config = ServerConfig::new("fake", path).with_startup_timeout(Duration::from_secs(1));
    let error = LspServer::start(config).await.unwrap_err();

    assert!(matches!(error, ServerError::Spawn { .. }));
    assert!(error.is_not_found());
    assert!(error.to_string().contains("servidor LSP não encontrado"));
}

#[cfg(unix)]
#[tokio::test]
async fn initialize_failure_is_reported_and_process_is_cleaned() {
    let pid_path = pid_file("fail-init");
    let config = with_pid_file(fake_config("fail-init"), &pid_path);

    let error = LspServer::start(config).await.unwrap_err();
    let ServerError::Response(error) = error else {
        panic!("esperado erro de initialize, recebido {error:?}");
    };
    assert_eq!(error.code, -32603);

    let pid = read_pid(&pid_path);
    assert!(
        !process_exists(pid),
        "processo de inicialização falhou e ficou vivo"
    );
    let _ = std::fs::remove_file(&pid_path);
}

#[cfg(unix)]
#[tokio::test]
async fn initialize_timeout_kills_process() {
    let pid_path = pid_file("no-response");
    let config = with_pid_file(
        fake_config("no-response-init").with_startup_timeout(Duration::from_millis(200)),
        &pid_path,
    );

    let error = LspServer::start(config).await.unwrap_err();
    assert!(matches!(
        error,
        ServerError::Timeout {
            operation: "initialize",
            ..
        }
    ));

    let pid = read_pid(&pid_path);
    assert!(
        !process_exists(pid),
        "processo sem resposta não foi encerrado"
    );
    let _ = std::fs::remove_file(&pid_path);
}

#[tokio::test]
async fn process_exiting_before_initialize_is_reported() {
    let error = LspServer::start(fake_config("crash-init"))
        .await
        .unwrap_err();
    assert!(matches!(error, ServerError::Closed));
}

#[tokio::test]
async fn unexpected_exit_is_detected() {
    let mut server = LspServer::start(fake_config("crash-after-init"))
        .await
        .unwrap();

    let event = next_matching(&mut server, |event| matches!(event, ServerEvent::Exited(_))).await;
    assert!(matches!(event, ServerEvent::Exited(ExitReason::Unexpected)));
    assert_eq!(server.status(), ServerStatus::Exited);

    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn pending_request_fails_when_process_exits() {
    let mut server = LspServer::start(fake_config("normal")).await.unwrap();

    let result = server.request("fake/exit", None).await;
    assert!(matches!(result, Err(ServerError::Closed)));

    let event = next_matching(&mut server, |event| matches!(event, ServerEvent::Exited(_))).await;
    assert!(matches!(event, ServerEvent::Exited(ExitReason::Unexpected)));

    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn request_timeout_is_reported() {
    let mut server =
        LspServer::start(fake_config("normal").with_request_timeout(Duration::from_millis(100)))
            .await
            .unwrap();

    let error = server.request("fake/never", None).await.unwrap_err();
    assert!(matches!(
        error,
        ServerError::Timeout {
            operation: "requisição LSP",
            ..
        }
    ));

    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn late_response_is_reported_as_unknown_not_as_notification() {
    let mut server =
        LspServer::start(fake_config("normal").with_request_timeout(Duration::from_millis(100)))
            .await
            .unwrap();

    let error = server
        .request(
            "fake/delayed",
            Some(json!({"value": "late", "delay_ms": 300})),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, ServerError::Timeout { .. }));

    let event = next_matching(&mut server, |event| {
        matches!(event, ServerEvent::UnknownResponse(_))
    })
    .await;
    let ServerEvent::UnknownResponse(response) = event else {
        unreachable!();
    };
    assert_eq!(response.id, Id::Number(2));

    server.shutdown().await.unwrap();
}

#[test]
fn service_forwards_real_publish_diagnostics_with_session_identity() {
    let service = LspService::with_provider(|_, language| {
        (language == "fake").then(|| fake_config("publish-diagnostics"))
    })
    .unwrap();
    service.open("/workspace", "fake", "file:///workspace/a.fake", "x");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        for event in service.drain() {
            if let ServiceEvent::DiagnosticsPublished {
                workspace,
                language_id,
                session_id,
                current_document_version,
                publication,
            } = event
            {
                assert_eq!(workspace, "/workspace");
                assert_eq!(language_id, "fake");
                assert!(session_id > 0);
                assert_eq!(current_document_version, Some(1));
                assert_eq!(publication.uri, "file:///workspace/a.fake");
                assert_eq!(publication.diagnostics[0].message, "diagnóstico simulado");
                return;
            }
        }
        assert!(Instant::now() < deadline, "notificação tipada não chegou");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn service_navigation_runs_concurrently_and_keeps_original_document_context() {
    let service = LspService::with_provider(|_, language| {
        (language == "fake").then(|| fake_config("navigation"))
    })
    .unwrap();
    service.open("/workspace", "fake", "file:///workspace/main.fake", "x");
    let session_id = wait_server_started(&service);

    let slow = service.request_navigation(
        "/workspace",
        "fake",
        NavigationMethod::Definition,
        "file:///workspace/main.fake",
        Position {
            line: 0,
            character: 1,
        },
    );
    // A alteração deve ser processada enquanto a primeira resposta está pendente.
    service.change(
        "/workspace",
        "fake",
        "file:///workspace/main.fake",
        vec![TextChange::new(1, 1, "y")],
        "xy",
    );
    let fast = service.request_navigation(
        "/workspace",
        "fake",
        NavigationMethod::References {
            include_declaration: false,
        },
        "file:///workspace/main.fake",
        Position {
            line: 0,
            character: 2,
        },
    );

    let completions = wait_navigation_events(&service, 2);
    assert_eq!(
        completions[0].0, fast,
        "a resposta rápida deve chegar primeiro"
    );
    assert_eq!(completions[1].0, slow);
    assert_eq!(completions[0].1, session_id);
    assert_eq!(completions[1].1, session_id);
    assert_eq!(completions[0].2, Some(2));
    assert_eq!(completions[1].2, Some(1));
    assert!(matches!(
        completions[0].3,
        NavigationOutcome::Result(nexus_lsp::protocol::NavigationResult::Destinations(_))
    ));
    assert!(matches!(
        completions[1].3,
        NavigationOutcome::Result(nexus_lsp::protocol::NavigationResult::Destinations(_))
    ));
}

#[test]
fn service_reports_unsupported_navigation_capability() {
    let service = LspService::with_provider(|_, language| {
        (language == "fake").then(|| fake_config("normal"))
    })
    .unwrap();
    service.open("/workspace", "fake", "file:///workspace/main.fake", "x");
    wait_server_started(&service);
    service.request_navigation(
        "/workspace",
        "fake",
        NavigationMethod::Definition,
        "file:///workspace/main.fake",
        Position {
            line: 0,
            character: 0,
        },
    );
    assert!(matches!(
        wait_service_event(&service, |event| matches!(
            event,
            ServiceEvent::NavigationCompleted { .. }
        )),
        ServiceEvent::NavigationCompleted {
            outcome: NavigationOutcome::Unsupported,
            ..
        }
    ));
}

#[test]
fn navigation_timeout_is_reported_and_late_response_is_not_delivered() {
    let marker = std::env::temp_dir().join(format!(
        "nexus-lsp-navigation-timeout-{}.txt",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&marker);
    let service = LspService::with_provider({
        let marker = marker.clone();
        move |_, language| {
            (language == "fake").then(|| {
                with_marker_file(
                    fake_config("navigation-slow")
                        .with_navigation_timeout(Duration::from_millis(50)),
                    &marker,
                )
            })
        }
    })
    .unwrap();
    service.open("/workspace", "fake", "file:///workspace/main.fake", "x");
    wait_server_started(&service);
    service.request_navigation(
        "/workspace",
        "fake",
        NavigationMethod::Definition,
        "file:///workspace/main.fake",
        Position {
            line: 0,
            character: 0,
        },
    );
    assert!(matches!(
        wait_service_event(&service, |event| matches!(event, ServiceEvent::NavigationCompleted { .. })),
        ServiceEvent::NavigationCompleted {
            outcome: NavigationOutcome::TimedOut { duration },
            ..
        } if duration == Duration::from_millis(50)
    ));
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        service
            .drain()
            .into_iter()
            .all(|event| { !matches!(event, ServiceEvent::NavigationCompleted { .. }) })
    );
    let marker_contents = std::fs::read_to_string(&marker).unwrap_or_default();
    assert!(marker_contents.lines().any(|line| line == "cancel:2"));
    let _ = std::fs::remove_file(marker);
}

#[test]
fn navigation_cancel_uses_original_id_once_and_completed_requests_ignore_cancel() {
    let marker = std::env::temp_dir().join(format!(
        "nexus-lsp-navigation-cancel-{}.txt",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&marker);
    let service = LspService::with_provider({
        let marker = marker.clone();
        move |_, language| {
            (language == "fake").then(|| with_marker_file(fake_config("navigation"), &marker))
        }
    })
    .unwrap();
    service.open("/workspace", "fake", "file:///workspace/main.fake", "x");
    wait_server_started(&service);

    let cancelled = service.request_navigation(
        "/workspace",
        "fake",
        NavigationMethod::Definition,
        "file:///workspace/main.fake",
        Position {
            line: 0,
            character: 1,
        },
    );
    service.cancel_navigation(cancelled);
    service.cancel_navigation(cancelled);
    assert!(matches!(
        wait_service_event(&service, |event| matches!(event, ServiceEvent::NavigationCompleted { .. })),
        ServiceEvent::NavigationCompleted {
            context,
            outcome: NavigationOutcome::Cancelled,
        } if context.operation_id == cancelled
    ));

    let completed = service.request_navigation(
        "/workspace",
        "fake",
        NavigationMethod::Definition,
        "file:///workspace/main.fake",
        Position {
            line: 0,
            character: 2,
        },
    );
    assert!(matches!(
        wait_service_event(
            &service,
            |event| matches!(event, ServiceEvent::NavigationCompleted { context, .. } if context.operation_id == completed)
        ),
        ServiceEvent::NavigationCompleted {
            outcome: NavigationOutcome::Result(_),
            ..
        }
    ));
    service.cancel_navigation(completed);
    // A primeira resposta tardia do servidor simulada deve ser ignorada.
    std::thread::sleep(Duration::from_millis(250));

    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let contents = std::fs::read_to_string(&marker).unwrap_or_default();
        if contents.contains("cancel:2") {
            assert_eq!(
                contents.lines().filter(|line| *line == "cancel:2").count(),
                1
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "cancelRequest não chegou ao fake server"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(service.drain().into_iter().all(|event| {
        !matches!(event, ServiceEvent::NavigationCompleted { context, .. } if context.operation_id == completed)
    }));
    let _ = std::fs::remove_file(marker);
}

#[test]
fn navigation_pending_on_server_exit_is_invalidated_with_old_session_context() {
    let service = LspService::with_provider(|_, language| {
        (language == "fake").then(|| fake_config("navigation-exit"))
    })
    .unwrap();
    service.open("/workspace", "fake", "file:///workspace/main.fake", "x");
    let session_id = wait_server_started(&service);
    let operation_id = service.request_navigation(
        "/workspace",
        "fake",
        NavigationMethod::Definition,
        "file:///workspace/main.fake",
        Position {
            line: 0,
            character: 0,
        },
    );
    assert!(matches!(
        wait_service_event(&service, |event| matches!(event, ServiceEvent::NavigationCompleted { .. })),
        ServiceEvent::NavigationCompleted {
            context,
            outcome: NavigationOutcome::SessionEnded,
        } if context.operation_id == operation_id && context.session_id == Some(session_id)
    ));

    service.retry("/workspace", "fake");
    service.open("/workspace", "fake", "file:///workspace/next.fake", "x");
    let replacement_session = wait_server_started(&service);
    assert_ne!(replacement_session, session_id);
}

#[test]
fn navigation_context_keeps_workspace_and_document_identity_separate() {
    let service = LspService::with_provider(|_, language| {
        (language == "fake").then(|| fake_config("navigation"))
    })
    .unwrap();
    service.open("/workspace-a", "fake", "file:///workspace-a/main.fake", "a");
    let first_session = wait_server_started(&service);
    service.open("/workspace-b", "fake", "file:///workspace-b/main.fake", "b");
    let second_session = wait_server_started(&service);
    assert_ne!(first_session, second_session);

    let slow = service.request_navigation(
        "/workspace-a",
        "fake",
        NavigationMethod::Definition,
        "file:///workspace-a/main.fake",
        Position {
            line: 0,
            character: 1,
        },
    );
    let fast = service.request_navigation(
        "/workspace-b",
        "fake",
        NavigationMethod::Definition,
        "file:///workspace-b/main.fake",
        Position {
            line: 2,
            character: 2,
        },
    );
    let fast_event = wait_service_event(
        &service,
        |event| matches!(event, ServiceEvent::NavigationCompleted { context, .. } if context.operation_id == fast),
    );
    let slow_event = wait_service_event(
        &service,
        |event| matches!(event, ServiceEvent::NavigationCompleted { context, .. } if context.operation_id == slow),
    );

    let ServiceEvent::NavigationCompleted {
        context: fast_context,
        outcome:
            NavigationOutcome::Result(nexus_lsp::protocol::NavigationResult::Destinations(
                fast_destinations,
            )),
    } = fast_event
    else {
        panic!("resultado rápido esperado");
    };
    assert_eq!(fast_context.workspace, "/workspace-b");
    assert_eq!(fast_context.uri, "file:///workspace-b/main.fake");
    assert_eq!(
        fast_context.position,
        Position {
            line: 2,
            character: 2
        }
    );
    assert_eq!(fast_destinations[0].uri, "file:///workspace-b/main.fake");

    let ServiceEvent::NavigationCompleted {
        context: slow_context,
        outcome:
            NavigationOutcome::Result(nexus_lsp::protocol::NavigationResult::Destinations(
                slow_destinations,
            )),
    } = slow_event
    else {
        panic!("resultado lento esperado");
    };
    assert_eq!(slow_context.workspace, "/workspace-a");
    assert_eq!(slow_context.uri, "file:///workspace-a/main.fake");
    assert_eq!(slow_destinations[0].uri, "file:///workspace-a/main.fake");
}

#[test]
fn closing_document_cancels_only_its_pending_navigation() {
    let service = LspService::with_provider(|_, language| {
        (language == "fake").then(|| fake_config("navigation"))
    })
    .unwrap();
    service.open("/workspace", "fake", "file:///workspace/a.fake", "a");
    service.open("/workspace", "fake", "file:///workspace/b.fake", "b");
    wait_server_started(&service);

    let closed = service.request_navigation(
        "/workspace",
        "fake",
        NavigationMethod::Definition,
        "file:///workspace/a.fake",
        Position {
            line: 0,
            character: 1,
        },
    );
    let retained = service.request_navigation(
        "/workspace",
        "fake",
        NavigationMethod::Definition,
        "file:///workspace/b.fake",
        Position {
            line: 0,
            character: 2,
        },
    );
    service.close("/workspace", "fake", "file:///workspace/a.fake");

    assert!(matches!(
        wait_service_event(
            &service,
            |event| matches!(event, ServiceEvent::NavigationCompleted { context, .. } if context.operation_id == closed)
        ),
        ServiceEvent::NavigationCompleted {
            outcome: NavigationOutcome::Cancelled,
            ..
        }
    ));
    assert!(matches!(
        wait_service_event(
            &service,
            |event| matches!(event, ServiceEvent::NavigationCompleted { context, .. } if context.operation_id == retained)
        ),
        ServiceEvent::NavigationCompleted {
            outcome: NavigationOutcome::Result(_),
            ..
        }
    ));
}

#[tokio::test]
async fn cancelled_request_is_cleaned_up_and_late_response_is_unknown() {
    let mut server = LspServer::start(fake_config("normal")).await.unwrap();

    {
        let future = server.request(
            "fake/delayed",
            Some(json!({"value": "cancelada", "delay_ms": 300})),
        );
        tokio::pin!(future);

        // Sonda a requisição para que ela registre o `Id` e envie a mensagem,
        // depois cancela a espera descartando o future.
        let _ = timeout(Duration::from_millis(50), &mut future).await;
    }

    let event = next_matching(&mut server, |event| {
        matches!(event, ServerEvent::UnknownResponse(_))
    })
    .await;
    let ServerEvent::UnknownResponse(response) = event else {
        unreachable!();
    };
    assert_eq!(response.id, Id::Number(2));

    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn navigation_cancellation_clears_pending_id_and_discards_late_response() {
    let mut server = LspServer::start(fake_config("navigation")).await.unwrap();
    let pending = server
        .begin_navigation_request(
            "textDocument/definition",
            Some(json!({
                "textDocument":{"uri":"file:///workspace/main.fake"},
                "position":{"line":0,"character":1}
            })),
        )
        .unwrap();
    let request_id = pending.id().clone();
    let cancellation = pending.cancellation_handle();
    let waiting = tokio::spawn(pending.wait(Duration::from_secs(2)));

    assert!(cancellation.cancel());
    assert!(!cancellation.cancel());
    assert!(matches!(
        waiting.await.unwrap(),
        Err(ServerError::Cancelled)
    ));
    let event = next_matching(&mut server, |event| {
        matches!(event, ServerEvent::UnknownResponse(_))
    })
    .await;
    let ServerEvent::UnknownResponse(response) = event else {
        unreachable!();
    };
    assert_eq!(response.id, request_id);

    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn shutdown_is_graceful_and_idempotent() {
    let marker_path = std::env::temp_dir().join(format!(
        "nexus-lsp-shutdown-marker-{}.txt",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&marker_path);

    let mut server = LspServer::start(with_marker_file(fake_config("normal"), &marker_path))
        .await
        .unwrap();
    let pid = server.process_id().unwrap();

    server.shutdown().await.unwrap();
    assert_eq!(server.status(), ServerStatus::Exited);
    assert!(server.process_id().is_none());

    let markers =
        std::fs::read_to_string(&marker_path).expect("marcadores de encerramento ausentes");
    assert!(markers.lines().any(|line| line == "shutdown"));
    assert!(markers.lines().any(|line| line == "exit"));
    let _ = std::fs::remove_file(&marker_path);

    server.shutdown().await.unwrap();
    assert_eq!(server.status(), ServerStatus::Exited);

    #[cfg(unix)]
    assert!(!process_exists(pid), "processo ficou vivo após shutdown");
}

#[tokio::test]
async fn shutdown_timeout_kills_unresponsive_server() {
    let mut server = LspServer::start(
        fake_config("ignore-shutdown").with_shutdown_timeout(Duration::from_millis(200)),
    )
    .await
    .unwrap();
    let pid = server.process_id().unwrap();

    let started = Instant::now();
    server.shutdown().await.unwrap();
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(server.status(), ServerStatus::Exited);

    #[cfg(unix)]
    assert!(
        !process_exists(pid),
        "processo que ignorou shutdown não foi morto"
    );
}

#[tokio::test]
async fn server_announcing_non_utf16_position_encoding_is_rejected() {
    let error = LspServer::start(fake_config("position-utf8"))
        .await
        .unwrap_err();

    match error {
        ServerError::Protocol(message) => {
            assert!(
                message.contains("positionEncoding") && message.contains("utf-16"),
                "mensagem inesperada: {message}"
            );
        }
        other => panic!("erro inesperado: {other:?}"),
    }
}

#[tokio::test]
async fn unexpected_exit_during_change_invalidates_further_sync() {
    let mut server = LspServer::start(fake_config("exit-on-change"))
        .await
        .unwrap();

    let uri = "file:///tmp/exit-on-change.rs";
    server.open_document(uri, "rust", "abc").unwrap();
    // O envio em si é aceito; o processo morre ao processar a notificação.
    server
        .change_document(uri, &[TextChange::new(0, 1, "x")], "xbc")
        .unwrap();

    let event = next_matching(&mut server, |event| matches!(event, ServerEvent::Exited(_))).await;
    assert!(matches!(event, ServerEvent::Exited(ExitReason::Unexpected)));
    assert_eq!(server.status(), ServerStatus::Exited);

    let error = server
        .change_document(uri, &[TextChange::new(0, 1, "y")], "ybc")
        .unwrap_err();
    assert!(
        matches!(error, ServerError::NotRunning),
        "o estado incremental não pode continuar após a saída: {error:?}"
    );
}
