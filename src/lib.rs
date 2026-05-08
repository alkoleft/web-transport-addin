mod addin_error;
mod addin_host;
mod session_params;
mod tunnel;
mod reconnect;
mod session_integration;
#[cfg(test)]
mod harness_tests;
mod http;
mod mcp;
mod session;
mod ws;
mod ws_client;
use std::{
    collections::HashMap,
    error::Error,
    ffi::{c_int, c_long, c_void},
    fs::OpenOptions,
    sync::{
        atomic::{AtomicI32, Ordering},
        OnceLock,
    },
};

use addin1c::{create_component, destroy_component, name, AttachType};
use tracing_subscriber::{
    fmt, layer::SubscriberExt, reload, util::SubscriberInitExt, EnvFilter, Registry,
};

static TRACING_INIT: OnceLock<()> = OnceLock::new();
/// Reload-handle для динамического изменения уровня логирования из BSL
/// через метод `session.НастроитьЛогирование`.
static RELOAD_HANDLE: OnceLock<reload::Handle<EnvFilter, Registry>> = OnceLock::new();

fn init_tracing() {
    TRACING_INIT.get_or_init(|| {
        let path = std::env::var("WEBTRANSPORT_LOG_FILE").unwrap_or_else(|_| {
            std::env::temp_dir()
                .join("web-transport.log")
                .to_string_lossy()
                .into_owned()
        });
        // Дефолт — off: без mcp_log_level из /C компонента молчит. Уровень меняется
        // в рантайме через session.НастроитьЛогирование (см. set_log_level).
        // Env WEBTRANSPORT_LOG оставлен как override для CLI/CI-сценариев.
        let filter = EnvFilter::try_from_env("WEBTRANSPORT_LOG")
            .unwrap_or_else(|_| EnvFilter::new("off"));
        let (filter_layer, handle) = reload::Layer::new(filter);

        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path);
        let init_ok = match file {
            Ok(f) => Registry::default()
                .with(filter_layer)
                .with(
                    fmt::layer()
                        .with_writer(std::sync::Mutex::new(f))
                        .with_ansi(false)
                        .with_target(true)
                        .with_thread_ids(true),
                )
                .try_init()
                .is_ok(),
            Err(_) => Registry::default()
                .with(filter_layer)
                .with(fmt::layer().with_ansi(false))
                .try_init()
                .is_ok(),
        };
        if init_ok {
            let _ = RELOAD_HANDLE.set(handle);
        }
        tracing::info!(version = VERSION, path = %path, "webtransport addin tracing initialized");
    });
}

/// Меняет уровень фильтра логирования в рантайме. Вызывается из BSL через
/// `session.НастроитьЛогирование(level)`. Принимает строку вида `off|error|warn|info|debug|trace`
/// либо EnvFilter‑совместимое выражение (`webtransport=debug,addin1c=info`). Возвращает true,
/// если фильтр валиден и применён, false при ошибке парсинга или если subscriber ещё не
/// инициализирован.
pub fn set_log_level(level: &str) -> bool {
    init_tracing();
    let trimmed = level.trim();
    if trimmed.is_empty() {
        return false;
    }
    let new_filter = match EnvFilter::try_new(trimmed) {
        Ok(f) => f,
        Err(_) => return false,
    };
    let Some(handle) = RELOAD_HANDLE.get() else {
        return false;
    };
    if handle.reload(new_filter).is_err() {
        return false;
    }
    tracing::info!(level = %trimmed, "webtransport tracing level changed");
    true
}

pub const VERSION: &str = "0.6.6";

pub(crate) fn parse_headers(
    json_headers: String,
) -> Result<HashMap<String, String>, Box<dyn Error>> {
    if json_headers.is_empty() {
        return Ok(HashMap::new());
    }
    let raw = serde_json::from_str::<HashMap<String, serde_json::Value>>(&json_headers)?;
    Ok(raw
        .into_iter()
        .map(|(key, value)| {
            let value = match value {
                serde_json::Value::Null => "".to_owned(),
                serde_json::Value::Bool(b) => b.to_string(),
                serde_json::Value::Number(n) => n.to_string(),
                serde_json::Value::String(s) => s,
                serde_json::Value::Array(_) | serde_json::Value::Object(_) => "".to_owned(),
            };
            (key, value)
        })
        .collect())
}

pub static PLATFORM_CAPABILITIES: AtomicI32 = AtomicI32::new(-1);

unsafe fn cstr1c_to_string(name: *const u16) -> String {
    if name.is_null() {
        return String::new();
    }
    let mut len = 0usize;
    loop {
        if *name.add(len) == 0 {
            break;
        }
        len += 1;
    }
    let slice = std::slice::from_raw_parts(name, len);
    String::from_utf16_lossy(slice)
}

#[allow(non_snake_case)]
#[no_mangle]
/// # Safety
/// This function should be called from 1C.
pub unsafe extern "C" fn GetClassObject(name: *const u16, component: *mut *mut c_void) -> c_long {
    init_tracing();
    let class_name = cstr1c_to_string(name);
    tracing::info!(class = %class_name, "GetClassObject");
    match class_name.as_str() {
        "ws" => {
            let addin = ws::WsAddIn::new();
            if let Ok(addin) = addin {
                create_component(component, addin)
            } else {
                0
            }
        }
        "http" => {
            let addin = http::HttpAddIn::new();
            if let Ok(addin) = addin {
                create_component(component, addin)
            } else {
                0
            }
        }
        "mcp" => {
            let addin = mcp::McpAddIn::new();
            if let Ok(addin) = addin {
                create_component(component, addin)
            } else {
                0
            }
        }
        "session" => {
            let addin = session::SessionAddIn::new();
            if let Ok(addin) = addin {
                create_component(component, addin)
            } else {
                0
            }
        }
        _ => 0,
    }
}

#[allow(non_snake_case)]
#[no_mangle]
/// # Safety
/// This function should be called from 1C.
pub unsafe extern "C" fn DestroyObject(component: *mut *mut c_void) -> c_long {
    destroy_component(component)
}

#[allow(non_snake_case)]
#[no_mangle]
pub extern "C" fn GetClassNames() -> *const u16 {
    name!("ws|http|mcp|session").as_ptr()
}

#[allow(non_snake_case)]
#[no_mangle]
/// # Safety
/// This function should be called from 1C.
pub unsafe extern "C" fn SetPlatformCapabilities(capabilities: c_int) -> c_int {
    PLATFORM_CAPABILITIES.store(capabilities, Ordering::Relaxed);
    3
}

#[allow(non_snake_case)]
#[no_mangle]
pub extern "C" fn GetAttachType() -> AttachType {
    AttachType::Any
}
