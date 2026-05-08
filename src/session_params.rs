//! Резолвер параметров клиентской сессии для подключения к session-manager.
//!
//! Контракт зафиксирован в `v8-client-session-manager` ADR-0020 / ADR-0029:
//!
//! - `manager_url` — из константы расширения `WebTransportSessionManagerURL`;
//!   если константа пуста — fallback на default `ws://127.0.0.1:4000/sessions`;
//!   значение `manager_url=...` в `/C` ИГНОРИРУЕТСЯ (явно вне контракта).
//! - `client_uid` — генерируется на стороне 1С через `Новый УникальныйИдентификатор()`;
//!   для удобства тестов и harness'а здесь же есть [`fresh_client_uid`].
//! - `kind` — берётся из `/C"kind=..."`. По умолчанию `"client"`. Эвристика
//!   по `СтрокаЗапуска()` удалена (ADR-0005, нарушение №1): теперь явный
//!   `kind=...` обязан передаваться менеджером (LocalBackend инжектит его
//!   в /C через kind_hint), либо BSL-стороной. Никаких догадок по флагам
//!   `/TESTMANAGER` / `/TESTCLIENT` / `RunYaXUnit` в Rust больше нет.
//! - `correlation_id` — напрямую из `/C"correlation_id=..."`; если нет —
//!   `None`.
//! - `host_id` — приоритет: env `V8_HOST_ID` → `gethostname` → `"unknown"`.
//!   ADR-0029.
//! - `pid` — `std::process::id()`. ADR-0029.
//! - `capabilities` — поле удалено (ADR-0003 / ADR-0005). Spawn/kill живут
//!   как обычные tools в test_client; маршрутизация менеджера идёт по имени
//!   tool в `session.register.tools`.
//!
//! Парсер `ПараметрЗапуска` (`StartupParameter`) умышленно живёт здесь, в
//! Rust, а не в БСП‑обёртке: это снимает завязку реализации на конкретный
//! релиз БСП и даёт возможность unit‑тестировать всю выборку источников
//! одним прогоном `cargo test`. Со стороны 1С остаётся только тонкий
//! адаптер: прочитать константу, вызвать `СтрокаЗапуска()` и `ПараметрЗапуска`,
//! передать всё в этот резолвер.

use std::collections::HashMap;

use serde::Serialize;

/// Default `manager_url`, совпадает с default‑bind менеджера
/// (см. `v8-client-session-manager` SESSION_MANAGER.md §8.3).
pub const DEFAULT_MANAGER_URL: &str = "ws://127.0.0.1:4000/sessions";

/// Имя константы расширения, в которой хранится `manager_url`.
/// Используется только для документации/логов — чтение идёт на стороне 1С.
pub const MANAGER_URL_CONSTANT: &str = "WebTransportSessionManagerURL";

/// Имя переменной окружения для явного override `host_id` (ADR-0029).
pub const HOST_ID_ENV: &str = "V8_HOST_ID";

/// Известные значения `kind`, которые менеджер ожидает в `session.register`.
pub mod kinds {
    pub const CLIENT: &str = "client";
}

// ─── HostInfoProvider trait ────────────────────────────────────────────────

/// Абстракция над источниками host/pid для детерминированного тестирования.
/// Продакшн-реализация — [`OsHostInfo`], тестовая — [`MockHostInfo`].
pub trait HostInfoProvider {
    /// Возвращает `host_id`. Приоритет: env `V8_HOST_ID` → gethostname → `"unknown"`.
    fn host_id(&self) -> String;
    /// Возвращает PID текущего процесса.
    fn pid(&self) -> u32;
}

/// Продакшн-реализация [`HostInfoProvider`]: читает env и ОС.
pub struct OsHostInfo;

impl HostInfoProvider for OsHostInfo {
    fn host_id(&self) -> String {
        // Приоритет 1: явный override через env.
        if let Ok(val) = std::env::var(HOST_ID_ENV) {
            let trimmed = val.trim().to_owned();
            if !trimmed.is_empty() {
                return trimmed;
            }
        }
        // Приоритет 2: gethostname.
        let name = gethostname::gethostname().to_string_lossy().to_string();
        if !name.is_empty() {
            return name;
        }
        // Fallback.
        "unknown".to_owned()
    }

    fn pid(&self) -> u32 {
        std::process::id()
    }
}

/// Тестовая реализация [`HostInfoProvider`] с фиксированными значениями.
#[cfg(test)]
pub struct MockHostInfo {
    pub host_id: String,
    pub pid: u32,
}

#[cfg(test)]
impl HostInfoProvider for MockHostInfo {
    fn host_id(&self) -> String {
        self.host_id.clone()
    }
    fn pid(&self) -> u32 {
        self.pid
    }
}

// ─── ResolveInput / SessionParams ──────────────────────────────────────────

/// Срез входных данных, которые 1С‑адаптер передаёт в резолвер.
#[derive(Debug, Clone, Default)]
pub struct ResolveInput {
    /// Значение константы `WebTransportSessionManagerURL`. Если константа
    /// пуста / не существует — передавать пустую строку.
    pub constant_url: String,
    /// `СтрокаЗапуска()` — полная команда, например
    /// `"DESIGNER /TESTMANAGER /S\"server/db\""`. Может быть пустой.
    pub launch_string: String,
    /// `ПараметрЗапуска` (`StartupParameter`) — содержимое ключа `/C"..."`
    /// без обрамляющих кавычек, например `"correlation_id=abc kind=foo"`.
    pub startup_param: String,
    /// Уже сгенерированный `client_uid` (через `Новый УникальныйИдентификатор()`
    /// на стороне 1С). Если пусто — резолвер сам сгенерирует через
    /// [`fresh_client_uid`], но это compatibility‑путь для tests/harness'а;
    /// в production 1С обязан передать свой UUID.
    pub client_uid: String,
}

/// Результат резолва (ADR-0020, ADR-0029).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionParams {
    pub manager_url: String,
    pub client_uid: String,
    pub kind: String,
    /// `None`, если `correlation_id=` не задан в `/C`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
    /// Идентификатор хоста, в котором работает addin (ADR-0029).
    pub host_id: String,
    /// PID процесса 1cv8c, в который загружен addin (ADR-0029).
    pub pid: u32,
}

impl SessionParams {
    /// JSON‑сериализация для возврата 1С‑коду через addin (см. ADR‑0020 / ADR-0029).
    pub fn to_json(&self) -> String {
        // Поля заведомо валидный UTF‑8 без управляющих символов; serde_json
        // никогда здесь не падает.
        serde_json::to_string(self).expect("SessionParams serialization is infallible")
    }
}

/// Сгенерировать свежий `client_uid`. Используется harness'ом и как
/// fallback в [`resolve`] при пустом `ResolveInput::client_uid`.
pub fn fresh_client_uid() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Главный резолвер. Принимает `&dyn HostInfoProvider` для детерминированного
/// тестирования. В продакшн-коде используй [`resolve_default`].
pub fn resolve(input: &ResolveInput, host_info: &dyn HostInfoProvider) -> SessionParams {
    let manager_url = if input.constant_url.trim().is_empty() {
        DEFAULT_MANAGER_URL.to_owned()
    } else {
        input.constant_url.trim().to_owned()
    };

    let pairs = parse_startup_param(&input.startup_param);
    let kind_override = pairs.get("kind").map(String::as_str);
    let correlation_id = pairs.get("correlation_id").cloned();
    let uid_override = pairs.get("client_uid").map(String::as_str);

    let kind = match kind_override {
        Some(k) if !k.is_empty() => k.to_owned(),
        _ => kinds::CLIENT.to_owned(),
    };

    // ADR-0030: manager-spawned клиенты получают expected client_uid через
    // /C "client_uid=..." — этот override побеждает 1С-генерируемый UUID.
    let client_uid = match uid_override {
        Some(u) if !u.is_empty() => u.to_owned(),
        _ => {
            if input.client_uid.is_empty() {
                fresh_client_uid()
            } else {
                input.client_uid.clone()
            }
        }
    };

    SessionParams {
        manager_url,
        client_uid,
        kind,
        correlation_id,
        host_id: host_info.host_id(),
        pid: host_info.pid(),
    }
}

/// Удобная обёртка над [`resolve`], использующая [`OsHostInfo`] (продакшн).
pub fn resolve_default(input: &ResolveInput) -> SessionParams {
    resolve(input, &OsHostInfo)
}

/// Парсер строки вида `key=value key=value` (значения без пробелов и кавычек,
/// в стиле `ПараметрЗапуска`). Дубли — побеждает последний.
pub fn parse_startup_param(raw: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for token in raw.split_whitespace() {
        if let Some((k, v)) = token.split_once('=') {
            if !k.is_empty() {
                out.insert(k.to_owned(), v.to_owned());
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mock() -> MockHostInfo {
        MockHostInfo {
            host_id: "test-host".into(),
            pid: 12345,
        }
    }

    fn input(constant: &str, launch: &str, startup: &str, uid: &str) -> ResolveInput {
        ResolveInput {
            constant_url: constant.to_owned(),
            launch_string: launch.to_owned(),
            startup_param: startup.to_owned(),
            client_uid: uid.to_owned(),
        }
    }

    fn resolve_t(inp: &ResolveInput) -> SessionParams {
        resolve(inp, &mock())
    }

    #[test]
    fn empty_constant_falls_back_to_default_url() {
        let p = resolve_t(&input("", "", "", "uid-1"));
        assert_eq!(p.manager_url, DEFAULT_MANAGER_URL);
    }

    #[test]
    fn whitespace_constant_treated_as_empty() {
        let p = resolve_t(&input("   ", "", "", "uid-1"));
        assert_eq!(p.manager_url, DEFAULT_MANAGER_URL);
    }

    #[test]
    fn explicit_constant_wins() {
        let p = resolve_t(&input("ws://10.0.0.5:4000/sessions", "", "", "uid-1"));
        assert_eq!(p.manager_url, "ws://10.0.0.5:4000/sessions");
    }

    #[test]
    fn manager_url_in_startup_param_is_ignored() {
        // По ADR‑0020 ключ manager_url=... в /C явно вне контракта.
        let p = resolve_t(&input("", "", "manager_url=ws://hijack/", "uid-1"));
        assert_eq!(p.manager_url, DEFAULT_MANAGER_URL);
    }

    #[test]
    fn kind_defaults_to_client_without_override() {
        // ADR-0005: эвристика по СтрокаЗапуска удалена. Без явного /C"kind=..."
        // kind становится "client" независимо от флагов в launch_string.
        let p = resolve_t(&input("", "DESIGNER /TESTMANAGER", "", "uid-1"));
        assert_eq!(p.kind, kinds::CLIENT);
    }

    #[test]
    fn kind_override_from_startup_param_wins() {
        let p = resolve_t(&input(
            "",
            "DESIGNER /TESTMANAGER",
            "kind=yaxunit_runner",
            "uid-1",
        ));
        assert_eq!(p.kind, "yaxunit_runner");
    }

    #[test]
    fn empty_kind_override_falls_back_to_client() {
        let p = resolve_t(&input("", "DESIGNER /TESTMANAGER", "kind=", "uid-1"));
        assert_eq!(p.kind, kinds::CLIENT);
    }

    #[test]
    fn correlation_id_extracted_from_startup_param() {
        let p = resolve_t(&input("", "", "correlation_id=trace-42", "uid-1"));
        assert_eq!(p.correlation_id.as_deref(), Some("trace-42"));
    }

    #[test]
    fn correlation_id_absent_yields_none() {
        let p = resolve_t(&input("", "", "kind=client", "uid-1"));
        assert!(p.correlation_id.is_none());
    }

    #[test]
    fn multiple_startup_pairs_parsed_independently() {
        let p = resolve_t(&input(
            "",
            "DESIGNER",
            "correlation_id=trace-7 kind=vanessa_manager other=ignored",
            "uid-1",
        ));
        assert_eq!(p.correlation_id.as_deref(), Some("trace-7"));
        assert_eq!(p.kind, "vanessa_manager");
    }

    #[test]
    fn duplicate_keys_last_wins() {
        let pairs = parse_startup_param("kind=a kind=b");
        assert_eq!(pairs.get("kind").map(String::as_str), Some("b"));
    }

    #[test]
    fn parse_skips_tokens_without_equals() {
        let pairs = parse_startup_param("garbage kind=client also-garbage");
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs.get("kind").map(String::as_str), Some("client"));
    }

    #[test]
    fn empty_client_uid_generates_fresh_uuid_v4() {
        let p = resolve_t(&input("", "", "", ""));
        // Формат UUID v4: 36 символов с дефисами в правильных позициях.
        assert_eq!(p.client_uid.len(), 36);
        assert_eq!(p.client_uid.as_bytes()[8], b'-');
        assert_eq!(p.client_uid.as_bytes()[14], b'4'); // version
    }

    #[test]
    fn provided_client_uid_passes_through() {
        let p = resolve_t(&input("", "", "", "custom-uid"));
        assert_eq!(p.client_uid, "custom-uid");
    }

    #[test]
    fn fresh_client_uid_is_unique_between_calls() {
        let a = fresh_client_uid();
        let b = fresh_client_uid();
        assert_ne!(a, b);
    }

    #[test]
    fn json_serialization_drops_absent_correlation_id() {
        let p = SessionParams {
            manager_url: DEFAULT_MANAGER_URL.to_owned(),
            client_uid: "uid-1".to_owned(),
            kind: "client".to_owned(),
            correlation_id: None,
            host_id: "test-host".into(),
            pid: 12345,
        };
        let json = p.to_json();
        assert!(!json.contains("correlation_id"));
        assert!(json.contains("\"manager_url\":\"ws://127.0.0.1:4000/sessions\""));
        assert!(json.contains("\"kind\":\"client\""));
    }

    #[test]
    fn json_serialization_includes_correlation_id_when_present() {
        let p = SessionParams {
            manager_url: "ws://x/y".to_owned(),
            client_uid: "uid-2".to_owned(),
            kind: "vanessa_manager".to_owned(),
            correlation_id: Some("trace-9".to_owned()),
            host_id: "test-host".into(),
            pid: 12345,
        };
        let json = p.to_json();
        assert!(json.contains("\"correlation_id\":\"trace-9\""));
    }

    #[test]
    fn end_to_end_realistic_yaxunit_runner_invocation() {
        // Менеджер спавнит yaxunit_runner: kind override + correlation_id.
        let p = resolve_t(&input(
            "ws://onec-infra:4000/sessions",
            "1cv8c ENTERPRISE /Sserver /CRunYaXUnit",
            "correlation_id=spawn-42 kind=yaxunit_runner",
            "fixed-uid-7",
        ));
        assert_eq!(p.manager_url, "ws://onec-infra:4000/sessions");
        assert_eq!(p.kind, "yaxunit_runner");
        assert_eq!(p.correlation_id.as_deref(), Some("spawn-42"));
        assert_eq!(p.client_uid, "fixed-uid-7");
    }

    // ─── Новые тесты для ADR-0029 ────────────────────────────────────────

    #[test]
    fn host_id_from_mock_appears_in_params() {
        let p = resolve_t(&input("", "", "", "uid-1"));
        assert_eq!(p.host_id, "test-host");
    }

    #[test]
    fn pid_from_mock_appears_in_params() {
        let p = resolve_t(&input("", "", "", "uid-1"));
        assert_eq!(p.pid, 12345u32);
    }

    #[test]
    fn capabilities_field_no_longer_serialised() {
        // ADR-0003 / ADR-0005: spawn/kill — это tools в test_client.
        // Поле capabilities удалено из SessionParams целиком.
        let p = resolve_t(&input("", "", "", "uid-1"));
        let json = p.to_json();
        assert!(!json.contains("capabilities"));
    }

    #[test]
    fn host_id_json_field_present() {
        let p = resolve_t(&input("", "", "", "uid-1"));
        let json = p.to_json();
        assert!(json.contains("\"host_id\":\"test-host\""));
    }

    #[test]
    fn pid_json_field_present() {
        let p = resolve_t(&input("", "", "", "uid-1"));
        let json = p.to_json();
        assert!(json.contains("\"pid\":12345"));
    }

    #[test]
    fn v8_host_id_env_override_wins_over_gethostname() {
        // Устанавливаем env, вызываем OsHostInfo, убираем env.
        std::env::set_var(HOST_ID_ENV, "custom-env-host");
        let result = OsHostInfo.host_id();
        std::env::remove_var(HOST_ID_ENV);
        assert_eq!(result, "custom-env-host");
    }

    #[test]
    fn gethostname_returns_nonempty_string() {
        // Sanity-check: gethostname должен вернуть непустое имя.
        let name = gethostname::gethostname().to_string_lossy().to_string();
        assert!(!name.is_empty(), "gethostname returned empty string");
    }
}
