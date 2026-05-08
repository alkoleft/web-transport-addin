//! 1С‑addin класс `session` — двунаправленная интеграция клиента 1С с
//! `v8-client-session-manager`. Этап 6.
//!
//! Контракт под BSL:
//!
//! - `ПолучитьПараметрыСессии(КонстантаURL, СтрокаЗапуска, ПараметрЗапуска, ClientUID)`
//!   возвращает JSON `{manager_url, client_uid, kind, correlation_id?}`. См.
//!   ADR‑0020 в `v8-client-session-manager`.
//! - `ЗапуститьСессионнуюИнтеграцию(URL, ClientUID, Kind, CorrelationID)`
//!   стартует фоновый WS‑pump к manager'у. Хост ВнешнегоСобытия — текущий
//!   addin connection. События приходят как `WS_INCOMING` (с конвертом
//!   `{correlation_id, payload}`, если CorrelationID задан) и
//!   `WS_RECONNECT_STATE`.
//! - `ОтправитьСообщение(Текст)` кладёт текст в исходящую очередь.
//! - `ОстановитьСессионнуюИнтеграцию()` отменяет cancel‑токен и снимает
//!   handle.
//! - `Версия` — версия аддина.
//! - `ОписаниеОшибки` — последняя ошибка boundary‑слоя.
//!
//! Реализация делегирует [`crate::session_integration::SessionIntegration`]
//! и [`crate::session_params`].

use std::error::Error;
use std::sync::Arc;
use std::time::Duration;

use addin1c::{name, AddinResult, CStr1C, Connection, MethodInfo, Methods, PropInfo, SimpleAddin, Variant};
use tokio::runtime::Runtime;

use crate::addin_error::report_platform_error;
use crate::addin_host::{AddinHost, RealAddinHost};
use crate::reconnect::BackoffPolicy;
use crate::session_integration::SessionIntegration;
use crate::session_params::{resolve_default, ResolveInput};
use crate::VERSION;

pub struct SessionAddIn {
    connection: Option<&'static Connection>,
    runtime: Arc<Runtime>,
    integration: Option<SessionIntegration>,
    last_error: Option<Box<dyn Error>>,
}

impl SessionAddIn {
    pub fn new() -> Result<Self, Box<dyn std::error::Error>> {
        Ok(Self::default())
    }

    fn get_session_params(
        &mut self,
        constant_url: &mut Variant,
        launch_string: &mut Variant,
        startup_param: &mut Variant,
        client_uid: &mut Variant,
        return_value: &mut Variant,
    ) -> AddinResult {
        let input = ResolveInput {
            constant_url: constant_url.get_string()?,
            launch_string: launch_string.get_string()?,
            startup_param: startup_param.get_string()?,
            client_uid: client_uid.get_string()?,
        };
        let params = resolve_default(&input);
        return_value.set_str1c(params.to_json())?;
        Ok(())
    }

    fn start(
        &mut self,
        url: &mut Variant,
        _client_uid: &mut Variant,
        _kind: &mut Variant,
        correlation_id: &mut Variant,
        return_value: &mut Variant,
    ) -> AddinResult {
        // ClientUID и Kind на стороне Rust сейчас не нужны: 1С‑код сам
        // формирует session.register после `WS_RECONNECT_STATE=connected`
        // (см. ADR‑0020). Параметры приняты в API, чтобы зафиксировать
        // контракт и пригодились на этапе 7 (correlation в нотификациях).
        // #97: идемпотентность повторного `Запустить`. Если предыдущая интеграция
        // ещё жива (BSL не вызвал stop, либо вызвал слишком быстро), сначала
        // корректно завершаем её — cancel-токен + abort task, чтобы новый
        // connect стартовал на чистом состоянии, а не «поверх» зомби-task.
        if let Some(prev) = self.integration.take() {
            tracing::warn!("session.start: previous integration still alive, shutting it down");
            // Round-2 fix: drop вместо shutdown(). shutdown() вызывает .take()
            // у task, и возвращаемый JoinHandle сразу дропается — а Drop для
            // tokio::JoinHandle = detach, не abort. Это означало, что наш
            // impl Drop для SessionIntegration (с cancel.cancel() + abort())
            // не срабатывал, потому что task уже был None. drop(prev) запускает
            // полноценный Drop с отменой и abort.
            drop(prev);
        }
        let connection = self
            .connection
            .ok_or("Connection недоступен — addin не инициализирован")?;
        let host: Arc<dyn AddinHost> = Arc::new(RealAddinHost::new(connection));

        let url_str = url.get_string()?;
        let cid = correlation_id.get_string()?;
        let cid_opt = if cid.is_empty() { None } else { Some(cid) };

        tracing::info!(
            url = %url_str,
            correlation_id = ?cid_opt,
            "session.start: spawning SessionIntegration"
        );

        let policy = BackoffPolicy {
            initial: Duration::from_millis(500),
            max: Duration::from_secs(30),
            multiplier: 2.0,
            max_attempts: None,
        };

        let integration = SessionIntegration::start_correlated(
            &self.runtime.handle(),
            host,
            url_str,
            policy,
            cid_opt,
        );
        self.integration = Some(integration);
        return_value.set_bool(true);
        Ok(())
    }

    fn send(&mut self, text: &mut Variant, return_value: &mut Variant) -> AddinResult {
        let msg = text.get_string()?;
        tracing::debug!(len = msg.len(), preview = %preview(&msg, 200), "session.send");
        let integration = self
            .integration
            .as_ref()
            .ok_or("Сессионная интеграция не запущена")?;
        integration
            .send(msg)
            .map_err(|e| format!("send: {e:?}"))?;
        return_value.set_bool(true);
        Ok(())
    }

    fn stop(&mut self, return_value: &mut Variant) -> AddinResult {
        tracing::info!("session.stop: shutting down integration");
        if let Some(integration) = self.integration.take() {
            // Round-2 fix (W-1): унифицированный путь остановки через Drop,
            // см. комментарий в start(). shutdown()-путь оставлять нельзя —
            // detach JoinHandle, abort не срабатывает.
            drop(integration);
        }
        return_value.set_bool(true);
        Ok(())
    }

    fn version(&mut self, return_value: &mut Variant) -> AddinResult {
        return_value.set_str1c(VERSION.to_owned())?;
        Ok(())
    }

    fn set_log_level(&mut self, level: &mut Variant, return_value: &mut Variant) -> AddinResult {
        let level_str = level.get_string()?;
        let ok = crate::set_log_level(&level_str);
        tracing::info!(level = %level_str, ok, "session.НастроитьЛогирование");
        return_value.set_bool(ok);
        Ok(())
    }

    fn last_error(&mut self, return_value: &mut Variant) -> AddinResult {
        match self.last_error.as_ref() {
            Some(err) => return_value
                .set_str1c(err.to_string().as_str())
                .map_err(|e| e.into()),
            None => return_value.set_str1c("").map_err(|e| e.into()),
        }
    }
}

impl SimpleAddin for SessionAddIn {
    fn name() -> &'static CStr1C {
        name!("session")
    }
    fn init(&mut self, interface: &'static Connection) -> bool {
        self.connection = Some(interface);
        true
    }
    fn save_error(&mut self, err: Option<Box<dyn Error>>) {
        if let Some(ref error) = err {
            report_platform_error(self.connection, "WebTransport.Session", error.as_ref());
        }
        self.last_error = err;
    }
    fn methods() -> &'static [MethodInfo<Self>] {
        &[
            MethodInfo {
                name: name!("ПолучитьПараметрыСессии"),
                method: Methods::Method4(Self::get_session_params),
            },
            MethodInfo {
                name: name!("ЗапуститьСессионнуюИнтеграцию"),
                method: Methods::Method4(Self::start),
            },
            MethodInfo {
                name: name!("ОтправитьСообщение"),
                method: Methods::Method1(Self::send),
            },
            MethodInfo {
                name: name!("ОстановитьСессионнуюИнтеграцию"),
                method: Methods::Method0(Self::stop),
            },
            MethodInfo {
                name: name!("Версия"),
                method: Methods::Method0(Self::version),
            },
            MethodInfo {
                name: name!("НастроитьЛогирование"),
                method: Methods::Method1(Self::set_log_level),
            },
        ]
    }
    fn properties() -> &'static [PropInfo<Self>] {
        &[PropInfo {
            name: name!("ОписаниеОшибки"),
            getter: Some(Self::last_error),
            setter: None,
        }]
    }
}

fn preview(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_owned()
    } else {
        let head: String = s.chars().take(n).collect();
        format!("{head}…")
    }
}

impl Default for SessionAddIn {
    fn default() -> Self {
        Self {
            connection: None,
            runtime: Arc::new(Runtime::new().unwrap()),
            integration: None,
            last_error: None,
        }
    }
}
