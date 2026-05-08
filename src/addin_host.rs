//! Абстракция над `addin1c::Connection.external_event` для доставки событий
//! в платформу 1С.
//!
//! Существует две причины для этого слоя:
//!
//! 1. Тестируемость. Логика, которая должна порождать событие в 1С (WS-tunnel
//!    из этапа 5, MCP/HTTP мостики), сейчас жёстко зависит от
//!    `addin1c::Connection`, который без живой 1С создать нельзя. Через
//!    [`AddinHost`] можно подменить хост на in-memory мок и проверить
//!    взаимодействие с session-manager без подключения к информационной базе.
//! 2. Однообразие. На стороне платформы внешнее событие из компоненты — это
//!    тройка `(имя_компоненты, имя_события, payload)`. У нас она встречается в
//!    нескольких местах (MCP server, HTTP server, MCP handler). Единая
//!    точка входа упрощает дальнейшее добавление correlation_id и аудита из
//!    плана этапа 7.
//!
//! Решение зафиксировано в [`docs/decisions/0004-mock-strategy-for-addin-host-tests.md`].
//!
//! Модуль умышленно не трогает существующие call-site'ы (`mcp::server`,
//! `http::server`, `http::mcp_handler`). Это сделано в подзадачах 5.3+ при
//! внедрении WS-tunnel — там же существующие вызовы будут переключены на
//! `AddinHost`. На этапе 5.1 модуль добавляется аддитивно: тип ещё не
//! используется в production-коде, но уже доступен и покрыт unit-тестами.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use addin1c::{name, CString1C, Connection};

/// Имя компоненты, под которым все события приходят в 1С.
pub const COMPONENT_NAME: &str = "WebTransport";

/// Минимальный интервал между двумя `external_event` к платформе.
///
/// На Linux 1С 8.3.27 платформа на практике принимает только первое событие
/// из последовательности, если они доставляются вплотную из background-потока
/// tokio runtime; последующие события молча теряются. На Windows очередь
/// работает корректно, но единый pacing не вредит и упрощает контракт.
///
/// 150 мс выбраны эмпирически на smoke‑прогоне `client_mcp ↔ v8-session-manager`
/// (см. `feedback_addin_external_event_linux.md` в memory): меньшие значения
/// (50 мс) на отдельных прогонах всё ещё теряли события, 100 мс на грани,
/// 150 мс даёт стабильный запас для платформы.
pub const EXTERNAL_EVENT_MIN_INTERVAL: Duration = Duration::from_millis(150);

/// Хост, способный доставить внешнее событие 1С.
///
/// Контракт повторяет `addin1c::Connection.external_event`:
/// - `event` — имя события (например, `"MCP_MESSAGE"`, `"WS_INCOMING"`);
/// - `payload` — произвольный UTF-8 текст (как правило, JSON);
/// - возвращает `false`, если очередь событий 1С переполнена.
///
/// Реализация ДОЛЖНА быть `Sync + Send`, чтобы её можно было использовать
/// из tokio-задач и WS-цикла tunnel'а.
pub trait AddinHost: Send + Sync {
    /// Доставить событие платформе. Имя компоненты подставляется реализацией.
    fn external_event(&self, event: &str, payload: &str) -> bool;
}

/// Реализация поверх `addin1c::Connection` — то, что используется внутри
/// загруженной в 1С компоненты.
///
/// `Connection` приходит от платформы как `&'static`, поэтому хост хранит
/// ссылку без владения.
/// Сериализующий pacer для emit'ов в платформу.
///
/// На каждый вызов [`EmitPacer::wait`] блокирует поток вплоть до
/// [`EXTERNAL_EVENT_MIN_INTERVAL`] от предыдущего emit'а. Pacer
/// инкапсулирует и lock, и sleep в одном месте — благодаря
/// удержанию мьютекса на время sleep'а одновременные вызовы
/// из разных потоков сериализуются, и платформа всегда видит
/// события с интервалом ≥ `EXTERNAL_EVENT_MIN_INTERVAL`.
#[derive(Debug, Default)]
pub struct EmitPacer {
    last_emit: Mutex<Option<Instant>>,
    min_interval: Duration,
}

impl EmitPacer {
    pub fn new(min_interval: Duration) -> Self {
        Self {
            last_emit: Mutex::new(None),
            min_interval,
        }
    }

    /// Подождать и выполнить emit под удержанным локом.
    ///
    /// Лок удерживается ВО ВРЕМЯ sleep'а и FFI-вызова, чтобы:
    /// 1. Параллельные emit'ы из разных потоков сериализовались.
    /// 2. Время «последнего emit'а» фиксировалось ПОСЛЕ его завершения,
    ///    а не до — это гарантирует, что FFI-длительность всегда покрыта
    ///    интервалом.
    ///
    /// Возвращает фактическую задержку и результат FFI.
    pub fn run<S, F, R>(&self, sleeper: &S, op: F) -> (Duration, R)
    where
        S: PacingSleeper,
        F: FnOnce() -> R,
    {
        let mut guard = self
            .last_emit
            .lock()
            .expect("EmitPacer mutex poisoned");
        let waited = if let Some(prev) = *guard {
            let elapsed = prev.elapsed();
            if elapsed < self.min_interval {
                let remaining = self.min_interval - elapsed;
                sleeper.sleep(remaining);
                remaining
            } else {
                Duration::ZERO
            }
        } else {
            Duration::ZERO
        };
        let result = op();
        *guard = Some(Instant::now());
        (waited, result)
    }
}

/// Абстракция над sleep'ом, чтобы pacer можно было детерминированно
/// проверить unit-тестом без реальной задержки.
pub trait PacingSleeper {
    fn sleep(&self, duration: Duration);
}

/// Production-реализация — синхронный `std::thread::sleep`. На multi-thread
/// tokio runtime блокировка одного worker'а на ≤ `EXTERNAL_EVENT_MIN_INTERVAL`
/// не критична для других задач; pacer сериализует только сами emit'ы.
pub struct ThreadSleeper;
impl PacingSleeper for ThreadSleeper {
    fn sleep(&self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

pub struct RealAddinHost {
    connection: &'static Connection,
    pacer: EmitPacer,
}

impl RealAddinHost {
    pub fn new(connection: &'static Connection) -> Self {
        Self {
            connection,
            pacer: EmitPacer::new(EXTERNAL_EVENT_MIN_INTERVAL),
        }
    }
}

/// Сколько раз мы ретраим `external_event`, если 1С отвергла доставку (вернула false).
///
/// Платформа возвращает false при «временно нельзя»: переполнение очереди событий,
/// race при переинициализации компоненты и т.п. Ретрай через pacer-интервал
/// (≥ EXTERNAL_EVENT_MIN_INTERVAL) даёт платформе время проглотить событие.
/// 3 попытки = 1 первая + 2 ретрая = ≥ 300 мс на доставку.
const EXTERNAL_EVENT_MAX_ATTEMPTS: u32 = 3;

impl AddinHost for RealAddinHost {
    fn external_event(&self, event: &str, payload: &str) -> bool {
        let preview: String = if payload.chars().count() <= 200 {
            payload.to_owned()
        } else {
            payload.chars().take(200).collect::<String>() + "…"
        };
        let mut total_waited = Duration::ZERO;
        for attempt in 1..=EXTERNAL_EVENT_MAX_ATTEMPTS {
            let (waited, ok) = self.pacer.run(&ThreadSleeper, || {
                let event_c = CString1C::from(event);
                let payload_c = CString1C::from(payload);
                self.connection
                    .external_event(name!("WebTransport"), event_c, payload_c)
            });
            total_waited += waited;
            if ok {
                tracing::debug!(
                    event = %event,
                    ok = true,
                    attempt,
                    waited_ms = total_waited.as_millis() as u64,
                    payload = %preview,
                    "RealAddinHost.external_event"
                );
                return true;
            }
            if attempt < EXTERNAL_EVENT_MAX_ATTEMPTS {
                tracing::warn!(
                    event = %event,
                    attempt,
                    "RealAddinHost.external_event: ok=false, retrying"
                );
            }
        }
        tracing::error!(
            event = %event,
            ok = false,
            attempts = EXTERNAL_EVENT_MAX_ATTEMPTS,
            waited_ms = total_waited.as_millis() as u64,
            payload = %preview,
            "RealAddinHost.external_event: всё ещё ok=false после ретраев"
        );
        false
    }
}

/// In-memory реализация для тестов. Накапливает все вызовы в виде
/// `(event, payload)` и возвращает заранее настроенный признак успеха.
///
/// Для проверки в тесте: см. [`MockAddinHost::events`].
#[derive(Debug, Default)]
pub struct MockAddinHost {
    inner: Mutex<MockState>,
}

#[derive(Debug, Default)]
struct MockState {
    events: Vec<(String, String)>,
    queue_full: bool,
}

impl MockAddinHost {
    pub fn new() -> Self {
        Self::default()
    }

    /// Имитировать переполнение очереди — следующие вызовы будут возвращать
    /// `false`. Сбрасывается через [`MockAddinHost::set_queue_full`] с
    /// `false`.
    pub fn set_queue_full(&self, value: bool) {
        let mut guard = self.inner.lock().expect("mock host poisoned");
        guard.queue_full = value;
    }

    /// Снять снапшот текущих событий. Не очищает буфер.
    pub fn events(&self) -> Vec<(String, String)> {
        self.inner
            .lock()
            .expect("mock host poisoned")
            .events
            .clone()
    }

    /// Очистить накопленные события (удобно между фазами теста).
    pub fn clear(&self) {
        self.inner
            .lock()
            .expect("mock host poisoned")
            .events
            .clear();
    }
}

impl AddinHost for MockAddinHost {
    fn external_event(&self, event: &str, payload: &str) -> bool {
        let mut guard = self.inner.lock().expect("mock host poisoned");
        if guard.queue_full {
            return false;
        }
        guard.events.push((event.to_owned(), payload.to_owned()));
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_records_events_in_order() {
        let host = MockAddinHost::new();
        assert!(host.external_event("WS_INCOMING", "{\"a\":1}"));
        assert!(host.external_event("MCP_MESSAGE", "{\"b\":2}"));

        let events = host.events();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].0, "WS_INCOMING");
        assert_eq!(events[0].1, "{\"a\":1}");
        assert_eq!(events[1].0, "MCP_MESSAGE");
    }

    #[test]
    fn mock_returns_false_when_queue_full() {
        let host = MockAddinHost::new();
        host.set_queue_full(true);
        assert!(!host.external_event("WS_INCOMING", "ignored"));
        assert!(host.events().is_empty());

        host.set_queue_full(false);
        assert!(host.external_event("WS_INCOMING", "{}"));
        assert_eq!(host.events().len(), 1);
    }

    #[test]
    fn mock_clear_drops_buffered_events() {
        let host = MockAddinHost::new();
        host.external_event("E1", "p1");
        host.external_event("E2", "p2");
        host.clear();
        assert!(host.events().is_empty());

        host.external_event("E3", "p3");
        assert_eq!(host.events(), vec![("E3".to_owned(), "p3".to_owned())]);
    }

    #[test]
    fn mock_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<MockAddinHost>();
        assert_send_sync::<RealAddinHost>();
    }

    #[test]
    fn dyn_addin_host_dispatch_works() {
        let host: Box<dyn AddinHost> = Box::new(MockAddinHost::new());
        assert!(host.external_event("X", "y"));
    }

    /// Sleeper, который не спит, а только записывает запрошенные интервалы.
    /// Позволяет проверить логику pacer'а детерминированно.
    #[derive(Default)]
    struct RecordingSleeper {
        calls: Mutex<Vec<Duration>>,
    }
    impl RecordingSleeper {
        fn calls(&self) -> Vec<Duration> {
            self.calls.lock().unwrap().clone()
        }
    }
    impl PacingSleeper for RecordingSleeper {
        fn sleep(&self, duration: Duration) {
            self.calls.lock().unwrap().push(duration);
        }
    }

    #[test]
    fn pacer_first_call_does_not_sleep() {
        let pacer = EmitPacer::new(Duration::from_millis(100));
        let sleeper = RecordingSleeper::default();
        let (waited, _) = pacer.run(&sleeper, || ());
        assert_eq!(waited, Duration::ZERO);
        assert!(sleeper.calls().is_empty());
    }

    #[test]
    fn pacer_second_call_sleeps_remaining_interval() {
        let pacer = EmitPacer::new(Duration::from_secs(60));
        let sleeper = RecordingSleeper::default();
        pacer.run(&sleeper, || ()); // первый — без sleep
        let (waited, _) = pacer.run(&sleeper, || ());
        assert!(waited > Duration::ZERO);
        assert!(waited <= Duration::from_secs(60));
        assert_eq!(sleeper.calls().len(), 1);
    }

    #[test]
    fn pacer_skips_sleep_when_interval_already_passed() {
        let pacer = EmitPacer::new(Duration::ZERO);
        let sleeper = RecordingSleeper::default();
        pacer.run(&sleeper, || ());
        std::thread::sleep(Duration::from_millis(1));
        let (waited, _) = pacer.run(&sleeper, || ());
        assert_eq!(waited, Duration::ZERO);
        assert!(sleeper.calls().is_empty());
    }

    #[test]
    fn pacer_op_result_is_returned() {
        let pacer = EmitPacer::new(Duration::from_millis(10));
        let sleeper = RecordingSleeper::default();
        let (_, value) = pacer.run(&sleeper, || 42);
        assert_eq!(value, 42);
    }

#[test]
    fn pacer_serializes_concurrent_emits() {
        use std::sync::Arc;
        use std::thread;

        let pacer = Arc::new(EmitPacer::new(Duration::from_millis(50)));
        let sleeper = Arc::new(RecordingSleeper::default());

        let p1 = pacer.clone();
        let s1 = sleeper.clone();
        let h1 = thread::spawn(move || p1.run(&*s1, || ()).0);

        let p2 = pacer.clone();
        let s2 = sleeper.clone();
        let h2 = thread::spawn(move || p2.run(&*s2, || ()).0);

        let w1 = h1.join().unwrap();
        let w2 = h2.join().unwrap();

        assert_eq!(sleeper.calls().len(), 1, "ровно один поток должен был спать");
        assert!(w1 == Duration::ZERO || w2 == Duration::ZERO);
        assert!(w1 > Duration::ZERO || w2 > Duration::ZERO);
    }
}
