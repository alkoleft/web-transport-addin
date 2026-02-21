use std::error::Error;
use std::sync::Arc;

use addin1c::{name, AddinResult, CStr1C, MethodInfo, Methods, PropInfo, SimpleAddin, Variant};
use tokio::runtime::Runtime;

use crate::ws_client;
use crate::ws_client::WebSocketConnection;
use crate::VERSION;

pub struct WsAddIn {
    pub(super) runtime: Arc<Runtime>,
    pub(super) websocket: Option<WebSocketConnection>,
    last_error: Option<Box<dyn Error>>,
}

impl WsAddIn {
    pub fn new() -> Result<Self, Box<dyn std::error::Error>> {
        Ok(Self::default())
    }

    pub(super) fn connect(
        &mut self,
        address: &mut Variant,
        json_headers: &mut Variant,
        return_value: &mut Variant,
    ) -> AddinResult {
        ws_client::connect(&self.runtime, &mut self.websocket, address, json_headers, return_value)
    }

    pub(super) fn send(&mut self, message: &mut Variant, return_value: &mut Variant) -> AddinResult {
        ws_client::send(&self.runtime, &mut self.websocket, message, return_value)
    }

    pub(super) fn receive(&mut self, timeout: &mut Variant, return_value: &mut Variant) -> AddinResult {
        ws_client::receive(&self.runtime, &mut self.websocket, timeout, return_value)
    }

    pub(super) fn disconnect(&mut self, return_value: &mut Variant) -> AddinResult {
        ws_client::disconnect(&mut self.websocket, return_value)
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

impl SimpleAddin for WsAddIn {
    fn name() -> &'static CStr1C {
        name!("ws")
    }
    fn init(&mut self, _interface: &'static addin1c::Connection) -> bool {
        true
    }
    fn save_error(&mut self, err: Option<Box<dyn Error>>) {
        self.last_error = err;
    }
    fn methods() -> &'static [MethodInfo<Self>] {
        &[
            MethodInfo {
                name: name!("Подключиться"),
                method: Methods::Method2(Self::connect),
            },
            MethodInfo {
                name: name!("ОтправитьСообщение"),
                method: Methods::Method1(Self::send),
            },
            MethodInfo {
                name: name!("ПолучитьСообщение"),
                method: Methods::Method1(Self::receive),
            },
            MethodInfo {
                name: name!("Отключиться"),
                method: Methods::Method0(Self::disconnect),
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

impl Default for WsAddIn {
    fn default() -> Self {
        Self {
            last_error: None,
            websocket: None,
            runtime: Arc::new(Runtime::new().unwrap()),
        }
    }
}
