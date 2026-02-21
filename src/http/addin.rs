use std::collections::HashMap;
use std::error::Error;
use std::sync::{atomic::AtomicU64, Arc};

use addin1c::{name, AddinResult, CStr1C, MethodInfo, Methods, PropInfo, SimpleAddin, Variant};
use tokio::runtime::Runtime;
use tokio::sync::{mpsc, Mutex};

use super::server::HttpServerState;
use crate::VERSION;

pub struct HttpAddIn {
    pub(super) connection: Option<&'static addin1c::Connection>,
    pub(super) runtime: Arc<Runtime>,
    pub(super) http_server: Option<HttpServerState>,
    pub(super) http_request_counter: Arc<AtomicU64>,
    pub(super) sse_sessions: Arc<Mutex<HashMap<String, mpsc::UnboundedSender<String>>>>,
    pub(super) sse_session_counter: Arc<AtomicU64>,
    last_error: Option<Box<dyn Error>>,
}

impl HttpAddIn {
    pub fn new() -> Result<Self, Box<dyn std::error::Error>> {
        Ok(Self::default())
    }

    fn version(&mut self, return_value: &mut Variant) -> AddinResult {
        return_value.set_str1c(VERSION.to_owned())?;
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

impl SimpleAddin for HttpAddIn {
    fn name() -> &'static CStr1C {
        name!("http")
    }
    fn init(&mut self, interface: &'static addin1c::Connection) -> bool {
        self.connection = Some(interface);
        interface.set_event_buffer_depth(128);
        true
    }
    fn save_error(&mut self, err: Option<Box<dyn Error>>) {
        self.last_error = err;
    }
    fn methods() -> &'static [MethodInfo<Self>] {
        &[
            MethodInfo {
                name: name!("ЗапуститьHTTP"),
                method: Methods::Method1(Self::http_start),
            },
            MethodInfo {
                name: name!("ОстановитьHTTP"),
                method: Methods::Method0(Self::http_stop),
            },
            MethodInfo {
                name: name!("ОтправитьHTTPОтвет"),
                method: Methods::Method4(Self::http_send_response),
            },
            MethodInfo {
                name: name!("ОтправитьSSE"),
                method: Methods::Method2(Self::sse_send),
            },
            MethodInfo {
                name: name!("ЗакрытьSSE"),
                method: Methods::Method1(Self::sse_close),
            },
            MethodInfo {
                name: name!("Версия"),
                method: Methods::Method0(Self::version),
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

impl Default for HttpAddIn {
    fn default() -> Self {
        Self {
            connection: None,
            last_error: None,
            http_server: None,
            http_request_counter: Arc::new(AtomicU64::new(1)),
            sse_sessions: Arc::new(Mutex::new(HashMap::new())),
            sse_session_counter: Arc::new(AtomicU64::new(1)),
            runtime: Arc::new(Runtime::new().unwrap()),
        }
    }
}

pub struct McpServerAddIn {
    inner: HttpAddIn,
}

impl McpServerAddIn {
    pub fn new() -> Result<Self, Box<dyn std::error::Error>> {
        Ok(Self {
            inner: HttpAddIn::default(),
        })
    }

    fn http_start(&mut self, address: &mut Variant, return_value: &mut Variant) -> AddinResult {
        self.inner.http_start(address, return_value)
    }

    fn http_stop(&mut self, return_value: &mut Variant) -> AddinResult {
        self.inner.http_stop(return_value)
    }

    fn http_send_response(
        &mut self,
        request_id: &mut Variant,
        status_code: &mut Variant,
        json_headers: &mut Variant,
        body: &mut Variant,
        return_value: &mut Variant,
    ) -> AddinResult {
        self.inner
            .http_send_response(request_id, status_code, json_headers, body, return_value)
    }

    fn sse_send(
        &mut self,
        session_id: &mut Variant,
        data: &mut Variant,
        return_value: &mut Variant,
    ) -> AddinResult {
        self.inner.sse_send(session_id, data, return_value)
    }

    fn sse_close(&mut self, session_id: &mut Variant, return_value: &mut Variant) -> AddinResult {
        self.inner.sse_close(session_id, return_value)
    }

    fn version(&mut self, return_value: &mut Variant) -> AddinResult {
        self.inner.version(return_value)
    }

    fn last_error(&mut self, return_value: &mut Variant) -> AddinResult {
        self.inner.last_error(return_value)
    }
}

impl SimpleAddin for McpServerAddIn {
    fn name() -> &'static CStr1C {
        name!("mcp")
    }
    fn init(&mut self, interface: &'static addin1c::Connection) -> bool {
        self.inner.init(interface)
    }
    fn save_error(&mut self, err: Option<Box<dyn Error>>) {
        self.inner.save_error(err);
    }
    fn methods() -> &'static [MethodInfo<Self>] {
        &[
            MethodInfo {
                name: name!("ЗапуститьHTTP"),
                method: Methods::Method1(Self::http_start),
            },
            MethodInfo {
                name: name!("ОстановитьHTTP"),
                method: Methods::Method0(Self::http_stop),
            },
            MethodInfo {
                name: name!("ОтправитьHTTPОтвет"),
                method: Methods::Method4(Self::http_send_response),
            },
            MethodInfo {
                name: name!("ОтправитьSSE"),
                method: Methods::Method2(Self::sse_send),
            },
            MethodInfo {
                name: name!("ЗакрытьSSE"),
                method: Methods::Method1(Self::sse_close),
            },
            MethodInfo {
                name: name!("Версия"),
                method: Methods::Method0(Self::version),
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
