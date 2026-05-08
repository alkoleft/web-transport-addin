//! End-to-end проверка [`session_integration`] против настоящего WS-сервера.
//!
//! Этап 5.7 backlog'а. Цель — выловить регрессы интеграции
//! `WsConnector` ↔ `tokio_tungstenite::WebSocketStream` ↔ tunnel ↔
//! `MockAddinHost`. Сервер здесь — миниатюрный stub поверх
//! `tokio_tungstenite::accept_async`, не претендующий на полноту контракта
//! session-manager'а: только базовые сценарии connect / send / receive /
//! close, достаточные чтобы доказать, что цепочка собирается.
//!
//! Полноценная интеграция с настоящим менеджером (запуск
//! `v8-client-session-manager` как процесс или библиотечная сборка) — этап 6.

#![cfg(test)]

use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc::unbounded_channel;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::protocol::Message as WsMessage;
use crate::addin_host::{AddinHost, MockAddinHost};
use crate::reconnect::{BackoffPolicy, EVENT_RECONNECT_STATE};
use crate::session_integration::SessionIntegration;
use crate::tunnel::EVENT_INCOMING;

/// Поднимает stub-сервер на 127.0.0.1:<random>, исполняет один сценарий
/// над первым принятым соединением и завершается.
async fn spawn_stub_server<F, Fut>(handler: F) -> (String, tokio::task::JoinHandle<()>)
where
    F: FnOnce(tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send,
{
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let url = format!("ws://{addr}/sessions");
    let task = tokio::spawn(async move {
        let (sock, _) = listener.accept().await.expect("accept");
        let ws = tokio_tungstenite::accept_async(sock).await.expect("upgrade");
        handler(ws).await;
    });
    (url, task)
}

#[tokio::test]
async fn end_to_end_text_frame_arrives_as_ws_incoming() {
    let host: Arc<MockAddinHost> = Arc::new(MockAddinHost::new());
    let host_dyn: Arc<dyn AddinHost> = host.clone();

    let (url, server) = spawn_stub_server(|mut ws| async move {
        ws.send(WsMessage::Text("from-manager".to_owned().into()))
            .await
            .expect("send");
        // Дать клиенту время прочитать.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let _ = ws.close(None).await;
    })
    .await;

    let policy = BackoffPolicy {
        initial: Duration::from_millis(10),
        max: Duration::from_millis(20),
        multiplier: 1.0,
        max_attempts: Some(2),
    };
    let integration = SessionIntegration::start(
        &tokio::runtime::Handle::current(),
        host_dyn,
        url,
        policy,
    );

    // Подождать пока пройдёт цикл connect → recv text → close → reconnect → close.
    timeout(Duration::from_secs(2), server).await.unwrap().unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    let _ = integration.shutdown();

    let evs = host.events();
    let incoming: Vec<_> = evs.iter().filter(|(n, _)| n == EVENT_INCOMING).collect();
    assert_eq!(incoming.len(), 1, "должно быть ровно одно WS_INCOMING, было: {evs:?}");
    assert_eq!(incoming[0].1, "from-manager");

    let states: Vec<_> = evs.iter().filter(|(n, _)| n == EVENT_RECONNECT_STATE).collect();
    assert!(states.iter().any(|(_, p)| p.contains("connecting")));
    assert!(states.iter().any(|(_, p)| p.contains("connected")));
}

#[tokio::test]
async fn end_to_end_outbound_message_reaches_server() {
    let host: Arc<MockAddinHost> = Arc::new(MockAddinHost::new());
    let host_dyn: Arc<dyn AddinHost> = host.clone();

    let (received_tx, mut received_rx) = unbounded_channel::<String>();
    let (url, server) = spawn_stub_server(move |mut ws| async move {
        // Прочитать одно текстовое сообщение от клиента и переложить в канал.
        if let Some(Ok(WsMessage::Text(t))) = ws.next().await {
            let _ = received_tx.send(t.to_string());
        }
        let _ = ws.close(None).await;
    })
    .await;

    let policy = BackoffPolicy {
        initial: Duration::from_millis(10),
        max: Duration::from_millis(20),
        multiplier: 1.0,
        max_attempts: Some(2),
    };
    let integration = SessionIntegration::start(
        &tokio::runtime::Handle::current(),
        host_dyn,
        url,
        policy,
    );

    // Ждать `connected`-состояние до 1с.
    let host_for_wait = host.clone();
    let waited = timeout(Duration::from_secs(1), async move {
        loop {
            let connected = host_for_wait
                .events()
                .iter()
                .any(|(n, p)| n == EVENT_RECONNECT_STATE && p.contains("connected"));
            if connected {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    waited.expect("connected state");

    integration
        .send("hello-from-1c".to_owned())
        .expect("queue accepts");

    let received = timeout(Duration::from_secs(1), received_rx.recv())
        .await
        .expect("got message")
        .expect("text");
    assert_eq!(received, "hello-from-1c");

    timeout(Duration::from_secs(2), server).await.unwrap().unwrap();
    let _ = integration.shutdown();
}

#[tokio::test]
async fn correlated_session_wraps_incoming_in_envelope() {
    let host: Arc<MockAddinHost> = Arc::new(MockAddinHost::new());
    let host_dyn: Arc<dyn AddinHost> = host.clone();

    let (url, server) = spawn_stub_server(|mut ws| async move {
        ws.send(WsMessage::Text("inner-payload".to_owned().into()))
            .await
            .expect("send");
        tokio::time::sleep(Duration::from_millis(50)).await;
        let _ = ws.close(None).await;
    })
    .await;

    let policy = BackoffPolicy {
        initial: Duration::from_millis(10),
        max: Duration::from_millis(20),
        multiplier: 1.0,
        max_attempts: Some(2),
    };
    let integration = SessionIntegration::start_correlated(
        &tokio::runtime::Handle::current(),
        host_dyn,
        url,
        policy,
        Some("trace-99".to_owned()),
    );

    timeout(Duration::from_secs(2), server).await.unwrap().unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    let _ = integration.shutdown();

    let evs = host.events();
    let incoming: Vec<_> = evs.iter().filter(|(n, _)| n == EVENT_INCOMING).collect();
    assert_eq!(incoming.len(), 1);
    let v: serde_json::Value = serde_json::from_str(&incoming[0].1).expect("json");
    assert_eq!(v["correlation_id"], "trace-99");
    assert_eq!(v["payload"], "inner-payload");
}

#[tokio::test]
async fn reconnect_state_emitted_when_first_attempt_fails() {
    let host: Arc<MockAddinHost> = Arc::new(MockAddinHost::new());
    let host_dyn: Arc<dyn AddinHost> = host.clone();

    // Заведомо невалидный порт → connect_async упадёт.
    let bad_url = "ws://127.0.0.1:1/sessions".to_owned();
    let policy = BackoffPolicy {
        initial: Duration::from_millis(10),
        max: Duration::from_millis(20),
        multiplier: 1.0,
        max_attempts: Some(2),
    };
    let integration = SessionIntegration::start(
        &tokio::runtime::Handle::current(),
        host_dyn,
        bad_url,
        policy,
    );

    // С max_attempts=2 цикл сам завершится через ~30мс через GiveUp,
    // не вмешиваемся cancel'ом до этого.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let task = integration.shutdown();
    let _ = timeout(Duration::from_secs(1), task).await;

    let evs = host.events();
    let states: Vec<_> = evs
        .iter()
        .filter(|(n, _)| n == EVENT_RECONNECT_STATE)
        .map(|(_, p)| p.clone())
        .collect();
    assert!(
        states.iter().any(|s| s.contains("connecting")),
        "ожидался connecting state, получено: {states:?}"
    );
    assert!(
        states.iter().any(|s| s.contains("disconnected")),
        "ожидался disconnected state, получено: {states:?}"
    );
}
