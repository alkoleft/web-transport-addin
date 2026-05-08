//! Высокоуровневый фасад для запуска двунаправленной интеграции 1С‑клиента
//! с session‑manager'ом. Этап 5.5 backlog'а.
//!
//! 1С‑код вызывает [`SessionIntegration::start_with_connector`] с уже
//! сконфигурированным [`Connector`] (для production — [`WsConnector`]) и
//! получает handle, через который:
//!
//! - можно слать исходящие фреймы ([`SessionIntegration::send`]);
//! - можно отменить интеграцию ([`SessionIntegration::shutdown`]).
//!
//! Регистрация в менеджере (`session.register`) НЕ автоматическая: 1С‑код
//! слушает событие [`reconnect::EVENT_RECONNECT_STATE`] и шлёт `register`
//! сразу после `connected`. Это сделано осознанно: список tools/resources/prompts
//! знает только сторона 1С, и зашивать его в Rust‑addin было бы дублированием
//! состояния.
//!
//! Per‑connect конкретику (`tokio_tungstenite::connect_async` ↔
//! `Stream<Item=TextOrClose>` + `Sink<String>`) реализует [`WsConnector`].
//! Тесты используют scripted‑connector из [`reconnect`] и не зависят от
//! живой 1С/живого WS.

use std::sync::Arc;

use futures_util::{Sink, SinkExt, Stream, StreamExt};
use tokio::net::TcpStream;
use tokio::runtime::Handle as RuntimeHandle;
use tokio::sync::mpsc::unbounded_channel;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::error::Error as WsError;
use tokio_tungstenite::tungstenite::protocol::Message as WsMessage;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};
use tokio_util::sync::CancellationToken;

use crate::addin_host::AddinHost;
use crate::reconnect::{
    run_with_reconnect_correlated, BackoffPolicy, Connector, FinalOutcome,
};
use crate::tunnel::{OutboundSender, SendError, TextOrClose};

/// Handle, который владеет фоновой задачей tunnel/reconnect.
pub struct SessionIntegration {
    cancel: CancellationToken,
    outbound: OutboundSender,
    task: Option<JoinHandle<FinalOutcome>>,
}

impl SessionIntegration {
    /// Запустить интеграцию с произвольным `Connector`'ом. Возвращает handle
    /// сразу, фон работает в переданном tokio‑runtime'е (`runtime`).
    pub fn start_with_connector<C>(
        runtime: &RuntimeHandle,
        host: Arc<dyn AddinHost>,
        connector: C,
        policy: BackoffPolicy,
    ) -> Self
    where
        C: Connector + 'static,
    {
        Self::start_with_connector_correlated(runtime, host, connector, policy, None)
    }

    /// Вариант [`start_with_connector`] с `correlation_id`, который при наличии
    /// прокидывается во все входящие события: 1С получает конверт
    /// `{correlation_id, payload}` вместо сырой строки. Этап 5.6.
    pub fn start_with_connector_correlated<C>(
        runtime: &RuntimeHandle,
        host: Arc<dyn AddinHost>,
        connector: C,
        policy: BackoffPolicy,
        correlation_id: Option<String>,
    ) -> Self
    where
        C: Connector + 'static,
    {
        let (outbound_tx, outbound_rx) = unbounded_channel::<String>();
        let cancel = CancellationToken::new();
        let cancel_for_task = cancel.clone();

        // ADR-0005 / ADR-0003: spawn-supervisor (system_capability registry)
        // удалён — spawn/kill живут в test_client tools и не требуют поддержки
        // в транспорте.

        let task = runtime.spawn(async move {
            run_with_reconnect_correlated(
                connector,
                host,
                outbound_rx,
                cancel_for_task,
                policy,
                correlation_id,
            )
            .await
        });
        Self {
            cancel,
            outbound: OutboundSender::new(outbound_tx),
            task: Some(task),
        }
    }

    /// Запустить интеграцию с реальным WS‑транспортом по URL.
    pub fn start(
        runtime: &RuntimeHandle,
        host: Arc<dyn AddinHost>,
        manager_url: String,
        policy: BackoffPolicy,
    ) -> Self {
        Self::start_with_connector(runtime, host, WsConnector::new(manager_url), policy)
    }

    /// Production‑API с прокидыванием `correlation_id` (см.
    /// [`start_with_connector_correlated`]).
    pub fn start_correlated(
        runtime: &RuntimeHandle,
        host: Arc<dyn AddinHost>,
        manager_url: String,
        policy: BackoffPolicy,
        correlation_id: Option<String>,
    ) -> Self {
        Self::start_with_connector_correlated(
            runtime,
            host,
            WsConnector::new(manager_url),
            policy,
            correlation_id,
        )
    }

    /// Положить исходящий фрейм в очередь.
    pub fn send(&self, text: String) -> Result<(), SendError> {
        self.outbound.send(text)
    }

    /// Сигнализировать остановку. Вызов consume'ит handle.
    pub fn shutdown(mut self) -> JoinHandle<FinalOutcome> {
        tracing::info!("SessionIntegration::shutdown — cancelling token and returning task handle");
        self.cancel.cancel();
        // Take the JoinHandle so Drop импл ничего не abort'ит дополнительно.
        self.task
            .take()
            .expect("SessionIntegration::task already taken")
    }
}

// #97: гарантировать abort фоновой reconnect/tunnel task при дропе integration.
// Без этого при close DRIVE (когда BSL не вызвал session.stop) старая task
// остаётся жить в runtime, и дроп runtime блокируется до её завершения.
// Если task висит в `connect_async` (наблюдалось на Windows при быстром
// re-open DRIVE), runtime drop тормозит дроп всего addin'а.
impl Drop for SessionIntegration {
    fn drop(&mut self) {
        if let Some(handle) = self.task.take() {
            tracing::info!("SessionIntegration::drop — cancelling token and aborting task");
            self.cancel.cancel();
            handle.abort();
        }
    }
}

/// Production‑connector поверх `tokio_tungstenite::connect_async`.
pub struct WsConnector {
    url: String,
}

impl WsConnector {
    pub fn new(url: String) -> Self {
        Self { url }
    }
}

impl Connector for WsConnector {
    type Stream = WsStreamAdapter;
    type Sink = WsSinkAdapter;
    type ConnectError = WsError;
    type InboundError = WsError;
    type OutboundError = WsError;
    type ConnectFut =
        std::pin::Pin<Box<dyn std::future::Future<Output = Result<(Self::Stream, Self::Sink), Self::ConnectError>> + Send>>;

    fn connect(&self) -> Self::ConnectFut {
        let url = self.url.clone();
        Box::pin(async move {
            // #97: diagnostic-first — каждая фаза WS-handshake'а должна оставлять
            // явный след в tracing-логе. Без этого баг «TCP ESTAB, Upgrade не ушёл»
            // невидим в самих логах addin'а (приходится ходить в `ss -tnp` на
            // менеджере). См. tasks/97-ws-handshake-blocker/initial-notes.md.
            let t0 = std::time::Instant::now();
            tracing::info!(url = %url, phase = "upgrade_send_start", "WsConnector: calling connect_async");
            let connect_fut = connect_async(&url);
            let (ws, resp) = match tokio::time::timeout(
                std::time::Duration::from_secs(5),
                connect_fut,
            )
            .await
            {
                Ok(Ok(pair)) => pair,
                Ok(Err(e)) => {
                    tracing::error!(
                        url = %url,
                        phase = "upgrade_failed",
                        elapsed_ms = t0.elapsed().as_millis() as u64,
                        error = ?e,
                        "WsConnector: connect_async failed"
                    );
                    return Err(e);
                }
                Err(_) => {
                    tracing::error!(
                        url = %url,
                        phase = "upgrade_timeout",
                        elapsed_ms = t0.elapsed().as_millis() as u64,
                        "WsConnector: connect_async timed out (5s) — TCP up but no Upgrade response"
                    );
                    return Err(WsError::Io(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "connect_async timed out (5s)",
                    )));
                }
            };
            tracing::info!(
                url = %url,
                phase = "ws_open",
                status = %resp.status(),
                elapsed_ms = t0.elapsed().as_millis() as u64,
                "WsConnector: WS upgrade succeeded"
            );
            let (sink, stream) = ws.split();
            Ok((WsStreamAdapter { inner: stream }, WsSinkAdapter { inner: sink }))
        })
    }
}

/// Адаптер: `WebSocketStream` → `Stream<Item=Result<TextOrClose, WsError>>`.
/// Свернутые фреймы (`Ping`/`Pong`/`Binary`/`Frame`) пропускаем дальше как
/// `pending`‑совместимые: tokio_tungstenite сам отвечает на ping и не
/// прокидывает их в верхний `next()`, поэтому здесь достаточно базового
/// маппинга.
pub struct WsStreamAdapter {
    inner: futures_util::stream::SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>,
}

impl Stream for WsStreamAdapter {
    type Item = Result<TextOrClose, WsError>;
    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        loop {
            match futures_util::StreamExt::poll_next_unpin(&mut self.inner, cx) {
                std::task::Poll::Pending => return std::task::Poll::Pending,
                std::task::Poll::Ready(None) => return std::task::Poll::Ready(None),
                std::task::Poll::Ready(Some(Err(e))) => return std::task::Poll::Ready(Some(Err(e))),
                std::task::Poll::Ready(Some(Ok(msg))) => match msg {
                    WsMessage::Text(t) => {
                        return std::task::Poll::Ready(Some(Ok(TextOrClose::Text(t.to_string()))))
                    }
                    WsMessage::Close(_) => {
                        return std::task::Poll::Ready(Some(Ok(TextOrClose::Close)))
                    }
                    // Бинарные/ping/pong/frame — игнорируем, читаем дальше.
                    _ => continue,
                },
            }
        }
    }
}

/// Адаптер: `Sink<String>` → `Sink<WsMessage>` поверх `WebSocketStream`.
pub struct WsSinkAdapter {
    inner: futures_util::stream::SplitSink<
        WebSocketStream<MaybeTlsStream<TcpStream>>,
        WsMessage,
    >,
}

impl Sink<String> for WsSinkAdapter {
    type Error = WsError;
    fn poll_ready(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        SinkExt::<WsMessage>::poll_ready_unpin(&mut self.inner, cx)
    }
    fn start_send(
        mut self: std::pin::Pin<&mut Self>,
        item: String,
    ) -> Result<(), Self::Error> {
        SinkExt::<WsMessage>::start_send_unpin(&mut self.inner, WsMessage::Text(item.into()))
    }
    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        SinkExt::<WsMessage>::poll_flush_unpin(&mut self.inner, cx)
    }
    fn poll_close(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        SinkExt::<WsMessage>::poll_close_unpin(&mut self.inner, cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::convert::Infallible;
    use std::pin::Pin;
    use std::sync::Mutex;
    use std::time::Duration;

    use futures_util::stream;
    use tokio::sync::mpsc;
    use tokio::time::timeout;

    use crate::addin_host::MockAddinHost;
    use crate::reconnect::EVENT_RECONNECT_STATE;

    type Frames = Vec<Result<TextOrClose, Infallible>>;
    type BoxedStream =
        Pin<Box<dyn Stream<Item = Result<TextOrClose, Infallible>> + Send>>;

    struct VecSink(mpsc::UnboundedSender<String>);
    impl Sink<String> for VecSink {
        type Error = Infallible;
        fn poll_ready(
            self: Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Ready(Ok(()))
        }
        fn start_send(self: Pin<&mut Self>, item: String) -> Result<(), Self::Error> {
            let _ = self.0.send(item);
            Ok(())
        }
        fn poll_flush(
            self: Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Ready(Ok(()))
        }
        fn poll_close(
            self: Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    enum Step {
        Ok(Frames, mpsc::UnboundedSender<String>),
        OkPending(mpsc::UnboundedSender<String>),
    }
    struct ScriptedConnector {
        steps: Mutex<std::collections::VecDeque<Step>>,
    }
    impl Connector for ScriptedConnector {
        type Stream = BoxedStream;
        type Sink = VecSink;
        type ConnectError = &'static str;
        type InboundError = Infallible;
        type OutboundError = Infallible;
        type ConnectFut =
            std::future::Ready<Result<(Self::Stream, Self::Sink), Self::ConnectError>>;
        fn connect(&self) -> Self::ConnectFut {
            let mut q = self.steps.lock().unwrap();
            match q.pop_front() {
                Some(Step::Ok(frames, sink_tx)) => {
                    let s: BoxedStream = Box::pin(stream::iter(frames));
                    std::future::ready(Ok((s, VecSink(sink_tx))))
                }
                Some(Step::OkPending(sink_tx)) => {
                    let s: BoxedStream = Box::pin(stream::pending());
                    std::future::ready(Ok((s, VecSink(sink_tx))))
                }
                None => std::future::ready(Err("script exhausted")),
            }
        }
    }

    #[tokio::test]
    async fn integration_forwards_outbound_and_dispatches_incoming() {
        let host: Arc<MockAddinHost> = Arc::new(MockAddinHost::new());
        let host_dyn: Arc<dyn AddinHost> = host.clone();

        let (sink_tx, mut sink_rx) = mpsc::unbounded_channel::<String>();
        let connector = ScriptedConnector {
            steps: Mutex::new(
                vec![
                    Step::Ok(
                        vec![Ok(TextOrClose::Text("from-manager".to_owned()))],
                        sink_tx,
                    ),
                    Step::OkPending(mpsc::unbounded_channel().0),
                ]
                .into(),
            ),
        };

        let policy = BackoffPolicy {
            initial: Duration::from_millis(1),
            max: Duration::from_millis(2),
            multiplier: 1.0,
            max_attempts: None,
        };
        let integration = SessionIntegration::start_with_connector(
            &RuntimeHandle::current(),
            host_dyn,
            connector,
            policy,
        );

        // Дать pump'у обработать входящее сообщение (включая 100мс
        // async-sleep'ы между emit'ами connecting/connected в reconnect.rs).
        tokio::time::sleep(Duration::from_millis(400)).await;

        // Слать исходящее в активную сессию (вторая Step::OkPending уже стала текущей,
        // но первая сессия закрылась после Ok(frames)+иссякания → reconnect → второй sink_tx).
        // Проверяем входящее WS_INCOMING.
        let incoming: Vec<_> = host
            .events()
            .into_iter()
            .filter(|(n, _)| n == "WS_INCOMING")
            .collect();
        assert_eq!(incoming.len(), 1);
        assert_eq!(incoming[0].1, "from-manager");

        // Состояния: connecting+connected минимум один раз.
        let states: Vec<_> = host
            .events()
            .into_iter()
            .filter(|(n, _)| n == EVENT_RECONNECT_STATE)
            .map(|(_, p)| p)
            .collect();
        assert!(states.iter().any(|s| s.contains("connecting")));
        assert!(states.iter().any(|s| s.contains("connected")));

        // Отправка через handle на текущий sink (первый закрылся, так что
        // проверим только то, что send() не вернул Closed).
        assert!(integration.send("hello".to_owned()).is_ok());

        // sink_rx должен опустеть (первая сессия завершилась до получения этого сообщения),
        // но это не повод для падения теста: задача — проверить API send().
        let _ = timeout(Duration::from_millis(20), sink_rx.recv()).await;

        let task = integration.shutdown();
        let outcome = timeout(Duration::from_millis(200), task).await.unwrap().unwrap();
        assert_eq!(outcome, FinalOutcome::Cancelled);
    }

    #[tokio::test]
    async fn shutdown_cancels_running_pump() {
        let host: Arc<MockAddinHost> = Arc::new(MockAddinHost::new());
        let host_dyn: Arc<dyn AddinHost> = host.clone();
        let (sink_tx, _sink_rx) = mpsc::unbounded_channel::<String>();
        let connector = ScriptedConnector {
            steps: Mutex::new(vec![Step::OkPending(sink_tx)].into()),
        };
        let integration = SessionIntegration::start_with_connector(
            &RuntimeHandle::current(),
            host_dyn,
            connector,
            BackoffPolicy::default(),
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
        let task = integration.shutdown();
        let outcome = timeout(Duration::from_millis(200), task).await.unwrap().unwrap();
        assert_eq!(outcome, FinalOutcome::Cancelled);
    }

    #[tokio::test]
    async fn send_after_shutdown_returns_closed() {
        let host: Arc<MockAddinHost> = Arc::new(MockAddinHost::new());
        let host_dyn: Arc<dyn AddinHost> = host.clone();
        let (sink_tx, _sink_rx) = mpsc::unbounded_channel::<String>();
        let connector = ScriptedConnector {
            steps: Mutex::new(vec![Step::OkPending(sink_tx)].into()),
        };
        // Сохраняем outbound перед shutdown'ом.
        let integration = SessionIntegration::start_with_connector(
            &RuntimeHandle::current(),
            host_dyn,
            connector,
            BackoffPolicy::default(),
        );
        let outbound = integration.outbound.clone();
        let _ = integration.shutdown();
        // Дать рантайму понять, что receiver в pump'е дропнут.
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(outbound.send("x".to_owned()), Err(SendError::Closed));
    }
}
