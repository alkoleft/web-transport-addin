//! Авто-reconnect для WS-tunnel'а с экспоненциальным backoff и публикацией
//! состояния через [`AddinHost`] событием [`EVENT_RECONNECT_STATE`].
//!
//! Этап 5.4 backlog'а `v8-client-session-manager`.
//!
//! # Контракт состояний
//!
//! 1С‑код получает события вида `WS_RECONNECT_STATE` с JSON‑payload'ом:
//! ```json
//! {"state": "connecting", "attempt": 1}
//! {"state": "connected"}
//! {"state": "disconnected", "reason": "InboundError"}
//! {"state": "give_up", "reason": "max attempts exceeded"}
//! ```
//!
//! По ADR‑0022 (soft reconnect через `client_uid`) менеджер допускает
//! повторную регистрацию с тем же `client_uid` после обрыва — поэтому здесь
//! нет дополнительной логики «чистой» перерегистрации: после connect 1С‑код
//! сам шлёт `session.register` (или это делает harness в тестах).
//!
//! Транспорт абстрагирован через [`Connector`]: для unit‑тестов используется
//! mock‑connector с программируемой последовательностью успехов/ошибок,
//! для production будет адаптер поверх `tokio_tungstenite::connect_async`.

use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures_util::{Sink, Stream};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::addin_host::AddinHost;
use crate::tunnel::{run_tunnel_with_correlation, RunOutcome, TextOrClose};

/// Имя внешнего события 1С для смены состояния канала.
pub const EVENT_RECONNECT_STATE: &str = "WS_RECONNECT_STATE";

/// Политика backoff между попытками.
#[derive(Debug, Clone, Copy)]
pub struct BackoffPolicy {
    pub initial: Duration,
    pub max: Duration,
    pub multiplier: f64,
    /// `None` — бесконечно. `Some(n)` — отдать `GiveUp` после n‑й неудачи.
    pub max_attempts: Option<u32>,
}

impl Default for BackoffPolicy {
    fn default() -> Self {
        Self {
            initial: Duration::from_millis(500),
            max: Duration::from_secs(30),
            multiplier: 2.0,
            max_attempts: None,
        }
    }
}

impl BackoffPolicy {
    /// Сколько ждать после `attempt`‑й неудачи (1‑based).
    pub fn delay_for(&self, attempt: u32) -> Duration {
        let mut ms = self.initial.as_millis() as f64;
        for _ in 1..attempt {
            ms *= self.multiplier;
            if ms >= self.max.as_millis() as f64 {
                ms = self.max.as_millis() as f64;
                break;
            }
        }
        Duration::from_millis(ms as u64)
    }
}

/// Абстрактный коннектор: умеет открыть одну WS‑сессию и вернуть
/// двунаправленный транспорт. Не помнит состояние между вызовами.
pub trait Connector: Send + Sync {
    type Stream: Stream<Item = Result<TextOrClose, Self::InboundError>> + Unpin + Send;
    type Sink: Sink<String, Error = Self::OutboundError> + Unpin + Send;
    type ConnectError: std::fmt::Debug + Send;
    type InboundError: std::fmt::Debug + Send;
    type OutboundError: std::fmt::Debug + Send;
    type ConnectFut: Future<Output = Result<(Self::Stream, Self::Sink), Self::ConnectError>> + Send;

    fn connect(&self) -> Self::ConnectFut;
}

/// Финальное состояние orchestrator'а.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinalOutcome {
    /// Cancel‑токен сработал.
    Cancelled,
    /// Outbound‑sender уронили; продолжать reconnect бессмысленно.
    OutboundDropped,
    /// `BackoffPolicy.max_attempts` исчерпан без успешного connect/run.
    GiveUp,
}

struct StateEmitter {
    host: Arc<dyn AddinHost>,
}

impl StateEmitter {
    fn emit(&self, json: String) {
        let _ = self.host.external_event(EVENT_RECONNECT_STATE, &json);
    }

    fn connecting(&self, attempt: u32) {
        tracing::info!(attempt, "state -> connecting");
        self.emit(format!("{{\"state\":\"connecting\",\"attempt\":{attempt}}}"));
    }
    fn connected(&self) {
        tracing::info!("state -> connected");
        self.emit("{\"state\":\"connected\"}".to_owned());
    }
    fn disconnected(&self, reason: &str) {
        tracing::warn!(reason, "state -> disconnected");
        self.emit(format!("{{\"state\":\"disconnected\",\"reason\":\"{reason}\"}}"));
    }
    fn give_up(&self, reason: &str) {
        tracing::error!(reason, "state -> give_up");
        self.emit(format!("{{\"state\":\"give_up\",\"reason\":\"{reason}\"}}"));
    }
}

/// Главный orchestrator: connect → run_tunnel → backoff → repeat.
pub async fn run_with_reconnect<C>(
    connector: C,
    host: Arc<dyn AddinHost>,
    outbound: mpsc::UnboundedReceiver<String>,
    cancel: CancellationToken,
    policy: BackoffPolicy,
) -> FinalOutcome
where
    C: Connector,
{
    run_with_reconnect_correlated(connector, host, outbound, cancel, policy, None).await
}

/// Вариант [`run_with_reconnect`] с `correlation_id`, который пробрасывается
/// в каждое входящее событие через [`tunnel::dispatch_incoming_correlated`].
///
/// ADR-0005 / ADR-0003: addin — только транспорт, поэтому `system_capability`
/// больше не нужен. Spawn/kill живут в test_client tools и идут обычными
/// `tools/call`-фреймами через `external_event`.
pub async fn run_with_reconnect_correlated<C>(
    connector: C,
    host: Arc<dyn AddinHost>,
    mut outbound: mpsc::UnboundedReceiver<String>,
    cancel: CancellationToken,
    policy: BackoffPolicy,
    correlation_id: Option<String>,
) -> FinalOutcome
where
    C: Connector,
{
    let emitter = StateEmitter { host: host.clone() };
    let mut attempt: u32 = 0;
    // #97: уникальный run_id для трассировки отдельной reconnect-task в
    // tracing-логе — иначе при наличии «зомби» старой task их события в
    // одном файле не различишь. Round-2 fix: монотонный счётчик вместо
    // Instant::now().elapsed() (который у только что созданного Instant
    // даёт ~единицы наносекунд → почти константа).
    static RUN_COUNTER: AtomicU64 = AtomicU64::new(0);
    let run_id: u64 = RUN_COUNTER.fetch_add(1, Ordering::Relaxed);
    tracing::info!(run_id, "run_with_reconnect: started");

    loop {
        if cancel.is_cancelled() {
            tracing::info!(run_id, attempt, "run_with_reconnect: cancelled at loop top");
            return FinalOutcome::Cancelled;
        }

        attempt = attempt.saturating_add(1);
        tracing::info!(run_id, attempt, "reconnect_fired");
        emitter.connecting(attempt);
        // Async-yield после emit'а — на Linux 1С 8.3.27 платформа теряет
        // быстрые подряд external_event из background-потока tokio runtime,
        // если между ними не было передачи управления планировщику. См.
        // `feedback_addin_external_event_linux.md` в memory.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let connect_result = tokio::select! {
            biased;
            _ = cancel.cancelled() => return FinalOutcome::Cancelled,
            r = connector.connect() => r,
        };

        match connect_result {
            Ok((inbound, sink)) => {
                attempt = 0; // перезапускаем счётчик на следующую серию неудач
                emitter.connected();
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;

                let outcome = run_tunnel_with_correlation(
                    inbound,
                    sink,
                    &mut outbound,
                    host.clone(),
                    cancel.clone(),
                    correlation_id.as_deref(),
                )
                .await;

                let reason = match outcome {
                    RunOutcome::Cancelled => return FinalOutcome::Cancelled,
                    RunOutcome::OutboundDropped => {
                        emitter.disconnected("OutboundDropped");
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                        return FinalOutcome::OutboundDropped;
                    }
                    RunOutcome::Closed => "Closed",
                    RunOutcome::InboundError => "InboundError",
                    RunOutcome::SinkError => "SinkError",
                };
                emitter.disconnected(reason);
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                // переход к новой попытке connect — без backoff,
                // потому что attempt сброшен и delay_for(1) применится ниже.
            }
            Err(err) => {
                emitter.disconnected(&format!("ConnectError: {err:?}"));
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                if let Some(max) = policy.max_attempts {
                    if attempt >= max {
                        emitter.give_up("max attempts exceeded");
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                        return FinalOutcome::GiveUp;
                    }
                }
            }
        }

        let delay = policy.delay_for(attempt);
        tracing::info!(run_id, attempt, delay_ms = delay.as_millis() as u64, "reconnect_scheduled");
        tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                tracing::info!(run_id, attempt, "run_with_reconnect: cancelled during backoff");
                return FinalOutcome::Cancelled;
            }
            _ = tokio::time::sleep(delay) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::convert::Infallible;
    use std::pin::Pin;
    use std::sync::Mutex;

    use futures_util::stream;
    use tokio::sync::mpsc::unbounded_channel;
    use tokio::time::timeout;

    use crate::addin_host::MockAddinHost;

    type Frames = Vec<Result<TextOrClose, Infallible>>;

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

    /// Программируемый коннектор: каждый вызов connect выдаёт следующий сценарий.
    /// Боксированные стримы дают единый тип независимо от Iter/Pending.
    enum Step {
        Fail,
        Ok(Frames),
        /// Stream остаётся pending — единственный способ выйти: cancel.
        OkPending,
    }
    type BoxedStream =
        Pin<Box<dyn Stream<Item = Result<TextOrClose, Infallible>> + Send>>;
    struct ScriptedConnector {
        steps: Mutex<std::collections::VecDeque<Step>>,
    }
    impl ScriptedConnector {
        fn new(steps: Vec<Step>) -> Self {
            Self {
                steps: Mutex::new(steps.into_iter().collect()),
            }
        }
    }

    impl Connector for ScriptedConnector {
        type Stream = BoxedStream;
        type Sink = VecSink;
        type ConnectError = &'static str;
        type InboundError = Infallible;
        type OutboundError = Infallible;
        type ConnectFut = std::future::Ready<Result<(Self::Stream, Self::Sink), Self::ConnectError>>;

        fn connect(&self) -> Self::ConnectFut {
            let mut q = self.steps.lock().unwrap();
            let step = q.pop_front().unwrap_or(Step::Fail);
            match step {
                Step::Fail => std::future::ready(Err("connect refused")),
                Step::Ok(frames) => {
                    let (tx, _rx) = unbounded_channel::<String>();
                    let s: BoxedStream = Box::pin(stream::iter(frames));
                    std::future::ready(Ok((s, VecSink(tx))))
                }
                Step::OkPending => {
                    let (tx, _rx) = unbounded_channel::<String>();
                    let s: BoxedStream = Box::pin(stream::pending());
                    std::future::ready(Ok((s, VecSink(tx))))
                }
            }
        }
    }

    fn host() -> Arc<MockAddinHost> {
        Arc::new(MockAddinHost::new())
    }

    fn states_of(h: &MockAddinHost) -> Vec<String> {
        h.events()
            .into_iter()
            .filter(|(name, _)| name == EVENT_RECONNECT_STATE)
            .map(|(_, payload)| payload)
            .collect()
    }

    #[test]
    fn backoff_grows_exponentially_then_caps() {
        let p = BackoffPolicy {
            initial: Duration::from_millis(100),
            max: Duration::from_millis(800),
            multiplier: 2.0,
            max_attempts: None,
        };
        assert_eq!(p.delay_for(1), Duration::from_millis(100));
        assert_eq!(p.delay_for(2), Duration::from_millis(200));
        assert_eq!(p.delay_for(3), Duration::from_millis(400));
        assert_eq!(p.delay_for(4), Duration::from_millis(800));
        assert_eq!(p.delay_for(5), Duration::from_millis(800));
        assert_eq!(p.delay_for(50), Duration::from_millis(800));
    }

    #[tokio::test]
    async fn give_up_after_max_attempts() {
        let h = host();
        let connector = ScriptedConnector::new(vec![Step::Fail, Step::Fail, Step::Fail]);
        let policy = BackoffPolicy {
            initial: Duration::from_millis(1),
            max: Duration::from_millis(2),
            multiplier: 2.0,
            max_attempts: Some(3),
        };
        let cancel = CancellationToken::new();
        let (_tx, outbound) = unbounded_channel::<String>();

        let host_dyn: Arc<dyn AddinHost> = h.clone();
        let outcome = timeout(
            Duration::from_secs(1),
            run_with_reconnect(connector, host_dyn, outbound, cancel, policy),
        )
        .await
        .unwrap();
        assert_eq!(outcome, FinalOutcome::GiveUp);

        let states = states_of(&h);
        // 3× connecting + 3× disconnected + 1× give_up
        assert_eq!(states.iter().filter(|s| s.contains("connecting")).count(), 3);
        assert_eq!(states.iter().filter(|s| s.contains("disconnected")).count(), 3);
        assert_eq!(states.iter().filter(|s| s.contains("give_up")).count(), 1);
    }

    #[tokio::test]
    async fn reconnect_after_close_then_cancel() {
        let h = host();
        // Первая сессия: один text + Close. Вторая сессия: pending → cancel.
        let connector = ScriptedConnector::new(vec![
            Step::Ok(vec![
                Ok(TextOrClose::Text("hello".to_owned())),
                Ok(TextOrClose::Close),
            ]),
            Step::OkPending,
        ]);
        let policy = BackoffPolicy {
            initial: Duration::from_millis(1),
            max: Duration::from_millis(2),
            multiplier: 2.0,
            max_attempts: Some(5),
        };
        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();
        let (_tx, outbound) = unbounded_channel::<String>();

        let host_dyn: Arc<dyn AddinHost> = h.clone();
        let pump = tokio::spawn(async move {
            run_with_reconnect(connector, host_dyn, outbound, cancel_clone, policy).await
        });

        // Дать достаточно времени на цикл reconnect (включая 100мс
        // async-sleep'ы между emit'ами в `run_with_reconnect_correlated`).
        tokio::time::sleep(Duration::from_millis(500)).await;
        cancel.cancel();
        let outcome = pump.await.unwrap();
        assert_eq!(outcome, FinalOutcome::Cancelled);

        // Должно быть как минимум: connecting/connected/disconnected(Closed)/connecting/connected
        let evs = h.events();
        // WS_INCOMING был ровно один.
        let incoming = evs.iter().filter(|(n, _)| n == "WS_INCOMING").count();
        assert_eq!(incoming, 1);

        let states = states_of(&h);
        assert!(states.iter().any(|s| s.contains("connecting") && s.contains("attempt\":1")));
        assert!(states.iter().any(|s| s.contains("connected")));
        assert!(states.iter().any(|s| s.contains("disconnected") && s.contains("Closed")));
    }

    #[tokio::test]
    async fn cancel_during_initial_connect_loop() {
        let h = host();
        let connector =
            ScriptedConnector::new((0..100).map(|_| Step::Fail).collect());
        let policy = BackoffPolicy {
            initial: Duration::from_millis(50),
            max: Duration::from_millis(50),
            multiplier: 1.0,
            max_attempts: None,
        };
        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();
        let (_tx, outbound) = unbounded_channel::<String>();

        let host_dyn: Arc<dyn AddinHost> = h.clone();
        let pump = tokio::spawn(async move {
            run_with_reconnect(connector, host_dyn, outbound, cancel_clone, policy).await
        });
        tokio::time::sleep(Duration::from_millis(30)).await;
        cancel.cancel();
        let outcome = timeout(Duration::from_secs(1), pump).await.unwrap().unwrap();
        assert_eq!(outcome, FinalOutcome::Cancelled);
    }
}
