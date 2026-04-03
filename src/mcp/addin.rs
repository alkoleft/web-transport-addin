use std::collections::HashMap;
use std::error::Error;
use std::sync::{atomic::AtomicU64, Arc, RwLock};

use addin1c::{name, AddinResult, CStr1C, MethodInfo, Methods, PropInfo, SimpleAddin, Variant};
#[cfg(feature = "validate-schema")]
use jsonschema::{Draft, Validator};
use rmcp::service::ClientSink;
use serde_json::Value;
use tokio::runtime::Runtime;
use tokio::sync::{oneshot, Mutex};

use super::registry::Registry;
use super::server::{
    parse_allow_list, start_mcp_server, AllowList, McpResponse, McpServerInfo, McpServerState,
};
use crate::{addin_error::report_platform_error, parse_headers, VERSION};
pub struct McpAddIn {
    pub(super) connection: Option<&'static addin1c::Connection>,
    pub(super) runtime: Arc<Runtime>,
    pub(super) server: Option<McpServerState>,
    pub(super) response_map: Arc<Mutex<HashMap<String, oneshot::Sender<McpResponse>>>>,
    pub(super) request_counter: Arc<AtomicU64>,
    pub(super) allow_list: Arc<RwLock<AllowList>>,
    pub(super) registry: Arc<RwLock<Registry>>,
    pub(super) client_sinks: Arc<Mutex<Vec<ClientSink>>>,
    pub(super) server_info: Arc<RwLock<McpServerInfo>>,
    pub(super) subscriptions: Arc<Mutex<HashMap<String, Vec<ClientSink>>>>,
    pub(super) tasks: Arc<Mutex<HashMap<String, super::server::TaskEntry>>>,
    last_error: Option<Box<dyn Error>>,
}

impl McpAddIn {
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

    fn mcp_start(
        &mut self,
        address: &mut Variant,
        origins: &mut Variant,
        timeout_secs: &mut Variant,
        return_value: &mut Variant,
    ) -> AddinResult {
        if self.server.is_some() {
            return Err("MCP сервер уже запущен".to_owned().into());
        }

        let address = address.get_string()?;
        let addr = address
            .parse()
            .map_err(|err| format!("Некорректный адрес: {err}"))?;

        let origins = origins.get_string()?;
        let allow_list = parse_allow_list(origins.as_str())?;
        {
            let mut guard = self
                .allow_list
                .write()
                .map_err(|_| "Lock poisoned".to_owned())?;
            *guard = allow_list;
        }

        let timeout_secs = timeout_secs.get_i32()?;
        let timeout_secs = if timeout_secs <= 0 { 30 } else { timeout_secs };

        let tasks = self.tasks.clone();
        self.runtime.clone().block_on(async {
            let mut guard = tasks.lock().await;
            guard.clear();
        });

        let server = start_mcp_server(
            self.runtime.clone(),
            addr,
            self.connection,
            self.allow_list.clone(),
            self.response_map.clone(),
            self.request_counter.clone(),
            self.registry.clone(),
            std::time::Duration::from_secs(timeout_secs as u64),
            self.client_sinks.clone(),
            self.server_info.clone(),
            self.subscriptions.clone(),
            self.tasks.clone(),
        )?;

        self.server = Some(server);
        return_value.set_bool(true);
        Ok(())
    }

    fn mcp_stop(&mut self, return_value: &mut Variant) -> AddinResult {
        let Some(server) = self.server.take() else {
            return Err("MCP сервер не запущен".to_owned().into());
        };

        let response_map = self.response_map.clone();
        let tasks = self.tasks.clone();
        self.runtime.clone().block_on(async {
            let mut map = response_map.lock().await;
            map.clear();
            let mut tasks = tasks.lock().await;
            tasks.clear();
        });

        let _ = server.shutdown.send(());
        return_value.set_bool(true);
        Ok(())
    }

    fn mcp_send_response(
        &mut self,
        request_id: &mut Variant,
        status_code: &mut Variant,
        json_headers: &mut Variant,
        body: &mut Variant,
        return_value: &mut Variant,
    ) -> AddinResult {
        let request_id = request_id.get_string()?;
        let status_code = status_code.get_i32()?;
        if !(100..=599).contains(&status_code) {
            return Err("Некорректный HTTP статус".to_owned().into());
        }
        let json_headers = json_headers.get_string()?;
        let body = body.get_string()?;

        let _ = parse_headers(json_headers)?;
        let response = McpResponse {
            status: status_code as u16,
            body,
        };

        let Some(_server) = self.server.as_ref() else {
            return Err("MCP сервер не запущен".to_owned().into());
        };

        self.runtime.clone().block_on(async {
            let mut map = self.response_map.lock().await;
            let sender = map
                .remove(request_id.as_str())
                .ok_or_else(|| "Не найден ожидающий ответ запрос".to_owned())?;
            sender.send(response).map_err(|_| -> Box<dyn Error> {
                "Не удалось отправить ответ".to_owned().into()
            })?;
            return_value.set_bool(true);
            Ok(())
        })
    }

    fn mcp_complete_task(
        &mut self,
        task_id: &mut Variant,
        status_code: &mut Variant,
        json_headers: &mut Variant,
        body: &mut Variant,
        return_value: &mut Variant,
    ) -> AddinResult {
        let task_id = task_id.get_string()?;
        let status_code = status_code.get_i32()?;
        if !(100..=599).contains(&status_code) {
            return Err("Некорректный HTTP статус".to_owned().into());
        }
        let json_headers = json_headers.get_string()?;
        let body = body.get_string()?;
        let _ = parse_headers(json_headers)?;
        let response = McpResponse {
            status: status_code as u16,
            body,
        };

        let Some(server) = self.server.as_ref() else {
            return Err("MCP сервер не запущен".to_owned().into());
        };

        self.runtime.clone().block_on(async {
            match server.complete_task(task_id.as_str(), response).await {
                Ok(()) => {
                    return_value.set_bool(true);
                }
                Err(err) if is_task_not_found_error(&err) => {
                    return_value.set_bool(false);
                }
                Err(err) => return Err(err.into()),
            }
            Ok(())
        })
    }

    fn mcp_set_task_status(
        &mut self,
        task_id: &mut Variant,
        status: &mut Variant,
        message: &mut Variant,
        return_value: &mut Variant,
    ) -> AddinResult {
        let task_id = task_id.get_string()?;
        let status = parse_task_status(status.get_string()?.as_str())?;
        let message = match message.get_string() {
            Ok(value) if !value.trim().is_empty() => Some(value),
            _ => None,
        };

        let Some(server) = self.server.as_ref() else {
            return Err("MCP сервер не запущен".to_owned().into());
        };

        self.runtime.clone().block_on(async {
            match server
                .update_task_status(task_id.as_str(), status, message)
                .await
            {
                Ok(()) => {
                    return_value.set_bool(true);
                }
                Err(err) if is_task_not_found_error(&err) => {
                    return_value.set_bool(false);
                }
                Err(err) => return Err(err.into()),
            }
            Ok(())
        })
    }

    fn mcp_notify_task_progress(
        &mut self,
        task_id: &mut Variant,
        progress: &mut Variant,
        total: &mut Variant,
        message: &mut Variant,
        return_value: &mut Variant,
    ) -> AddinResult {
        let task_id = task_id.get_string()?;
        let progress = parse_numeric_arg(progress)?;
        let total = match parse_numeric_arg(total) {
            Ok(value) if value >= 0.0 => Some(value),
            _ => None,
        };
        let message = match message.get_string() {
            Ok(value) if !value.trim().is_empty() => Some(value),
            _ => None,
        };

        let Some(server) = self.server.as_ref() else {
            return Err("MCP сервер не запущен".to_owned().into());
        };

        self.runtime.clone().block_on(async {
            match server
                .notify_task_progress(task_id.as_str(), progress, total, message)
                .await
            {
                Ok(()) => {
                    return_value.set_bool(true);
                }
                Err(err) if is_task_not_found_error(&err) => {
                    return_value.set_bool(false);
                }
                Err(err) => return Err(err.into()),
            }
            Ok(())
        })
    }

    fn mcp_notify_progress(
        &mut self,
        progress_token: &mut Variant,
        progress: &mut Variant,
        total: &mut Variant,
        message: &mut Variant,
        return_value: &mut Variant,
    ) -> AddinResult {
        let progress_token = parse_progress_token(progress_token)?;
        let progress = parse_numeric_arg(progress)?;
        let total = match parse_numeric_arg(total) {
            Ok(value) if value >= 0.0 => Some(value),
            _ => None,
        };
        let message = match message.get_string() {
            Ok(value) if !value.trim().is_empty() => Some(value),
            _ => None,
        };

        let Some(server) = self.server.as_ref() else {
            return Err("MCP сервер не запущен".to_owned().into());
        };

        self.runtime.clone().block_on(async {
            server
                .notify_progress(progress_token, progress, total, message)
                .await?;
            return_value.set_bool(true);
            Ok(())
        })
    }

    fn mcp_set_allowed_origins(
        &mut self,
        origins: &mut Variant,
        return_value: &mut Variant,
    ) -> AddinResult {
        let origins = origins.get_string()?;
        let allow_list = parse_allow_list(origins.as_str())?;
        {
            let mut guard = self
                .allow_list
                .write()
                .map_err(|_| "Lock poisoned".to_owned())?;
            *guard = allow_list;
        }
        return_value.set_bool(true);
        Ok(())
    }

    fn register_tools(&mut self, json: &mut Variant, return_value: &mut Variant) -> AddinResult {
        let json = json.get_string()?;
        let items = parse_json_items(json.as_str())?;
        let mut guard = self
            .registry
            .write()
            .map_err(|_| "Lock poisoned".to_owned())?;
        for item in items {
            let tool = parse_tool(item)?;
            #[cfg(feature = "validate-schema")]
            {
                let schema_value = Value::Object(tool.input_schema.as_ref().clone());
                let schema = compile_schema(schema_value)?;
                guard.register_tool(tool, schema);
            }
            #[cfg(not(feature = "validate-schema"))]
            {
                guard.register_tool(tool);
            }
        }
        return_value.set_bool(true);
        Ok(())
    }

    fn unregister_tool(&mut self, name: &mut Variant, return_value: &mut Variant) -> AddinResult {
        let name = name.get_string()?;
        let mut guard = self
            .registry
            .write()
            .map_err(|_| "Lock poisoned".to_owned())?;
        let removed = guard.remove_tool(name.as_str());
        return_value.set_bool(removed);
        Ok(())
    }

    fn clear_tools(&mut self, return_value: &mut Variant) -> AddinResult {
        let mut guard = self
            .registry
            .write()
            .map_err(|_| "Lock poisoned".to_owned())?;
        guard.clear_tools();
        return_value.set_bool(true);
        Ok(())
    }

    fn register_resources(
        &mut self,
        json: &mut Variant,
        return_value: &mut Variant,
    ) -> AddinResult {
        let json = json.get_string()?;
        let items = parse_json_items(json.as_str())?;
        let mut guard = self
            .registry
            .write()
            .map_err(|_| "Lock poisoned".to_owned())?;
        for item in items {
            let resource = serde_json::from_value(item)
                .map_err(|err| format!("Некорректный ресурс: {err}"))?;
            guard.register_resource(resource);
        }
        return_value.set_bool(true);
        Ok(())
    }

    fn unregister_resource(
        &mut self,
        uri: &mut Variant,
        return_value: &mut Variant,
    ) -> AddinResult {
        let uri = uri.get_string()?;
        let mut guard = self
            .registry
            .write()
            .map_err(|_| "Lock poisoned".to_owned())?;
        let removed = guard.remove_resource(uri.as_str());
        return_value.set_bool(removed);
        Ok(())
    }

    fn clear_resources(&mut self, return_value: &mut Variant) -> AddinResult {
        let mut guard = self
            .registry
            .write()
            .map_err(|_| "Lock poisoned".to_owned())?;
        guard.clear_resources();
        return_value.set_bool(true);
        Ok(())
    }

    fn register_resource_templates(
        &mut self,
        json: &mut Variant,
        return_value: &mut Variant,
    ) -> AddinResult {
        let json = json.get_string()?;
        let items = parse_json_items(json.as_str())?;
        let mut guard = self
            .registry
            .write()
            .map_err(|_| "Lock poisoned".to_owned())?;
        for item in items {
            let template = parse_resource_template(item)?;
            guard.register_resource_template(template)?;
        }
        return_value.set_bool(true);
        Ok(())
    }

    fn unregister_resource_template(
        &mut self,
        uri_template: &mut Variant,
        return_value: &mut Variant,
    ) -> AddinResult {
        let uri_template = uri_template.get_string()?;
        let mut guard = self
            .registry
            .write()
            .map_err(|_| "Lock poisoned".to_owned())?;
        let removed = guard.remove_resource_template(uri_template.as_str());
        return_value.set_bool(removed);
        Ok(())
    }

    fn clear_resource_templates(&mut self, return_value: &mut Variant) -> AddinResult {
        let mut guard = self
            .registry
            .write()
            .map_err(|_| "Lock poisoned".to_owned())?;
        guard.clear_resource_templates();
        return_value.set_bool(true);
        Ok(())
    }

    fn register_prompts(&mut self, json: &mut Variant, return_value: &mut Variant) -> AddinResult {
        let json = json.get_string()?;
        let items = parse_json_items(json.as_str())?;
        let mut guard = self
            .registry
            .write()
            .map_err(|_| "Lock poisoned".to_owned())?;
        for item in items {
            let prompt = serde_json::from_value(item)
                .map_err(|err| format!("Некорректный промпт: {err}"))?;
            guard.register_prompt(prompt);
        }
        return_value.set_bool(true);
        Ok(())
    }

    fn unregister_prompt(&mut self, name: &mut Variant, return_value: &mut Variant) -> AddinResult {
        let name = name.get_string()?;
        let mut guard = self
            .registry
            .write()
            .map_err(|_| "Lock poisoned".to_owned())?;
        let removed = guard.remove_prompt(name.as_str());
        return_value.set_bool(removed);
        Ok(())
    }

    fn notify_tools_changed(&mut self, return_value: &mut Variant) -> AddinResult {
        let Some(server) = self.server.as_ref() else {
            return Err("MCP сервер не запущен".to_owned().into());
        };
        self.runtime.block_on(server.broadcast_notification(
            rmcp::model::ServerNotification::ToolListChangedNotification(Default::default()),
        ));
        return_value.set_bool(true);
        Ok(())
    }

    fn notify_resources_changed(&mut self, return_value: &mut Variant) -> AddinResult {
        let Some(server) = self.server.as_ref() else {
            return Err("MCP сервер не запущен".to_owned().into());
        };
        self.runtime.block_on(server.broadcast_notification(
            rmcp::model::ServerNotification::ResourceListChangedNotification(Default::default()),
        ));
        return_value.set_bool(true);
        Ok(())
    }

    fn notify_prompts_changed(&mut self, return_value: &mut Variant) -> AddinResult {
        let Some(server) = self.server.as_ref() else {
            return Err("MCP сервер не запущен".to_owned().into());
        };
        self.runtime.block_on(server.broadcast_notification(
            rmcp::model::ServerNotification::PromptListChangedNotification(Default::default()),
        ));
        return_value.set_bool(true);
        Ok(())
    }

    fn notify_resource_updated(
        &mut self,
        uri: &mut Variant,
        return_value: &mut Variant,
    ) -> AddinResult {
        let Some(server) = self.server.as_ref() else {
            return Err("MCP сервер не запущен".to_owned().into());
        };
        let uri = uri.get_string()?;
        self.runtime.block_on(server.notify_resource_updated(uri));
        return_value.set_bool(true);
        Ok(())
    }

    fn clear_prompts(&mut self, return_value: &mut Variant) -> AddinResult {
        let mut guard = self
            .registry
            .write()
            .map_err(|_| "Lock poisoned".to_owned())?;
        guard.clear_prompts();
        return_value.set_bool(true);
        Ok(())
    }

    fn set_server_info(&mut self, json: &mut Variant, return_value: &mut Variant) -> AddinResult {
        let json = json.get_string()?;
        let value: serde_json::Value =
            serde_json::from_str(json.as_str()).map_err(|e| format!("Некорректный JSON: {e}"))?;
        let obj = value
            .as_object()
            .ok_or_else(|| "Ожидается JSON объект".to_owned())?;

        let name = obj
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "Отсутствует поле name".to_owned())?;
        let version = obj
            .get("version")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "Отсутствует поле version".to_owned())?;

        let info = McpServerInfo {
            name: Some(name.to_owned()),
            version: Some(version.to_owned()),
            title: obj.get("title").and_then(|v| v.as_str()).map(str::to_owned),
            description: obj
                .get("description")
                .and_then(|v| v.as_str())
                .map(str::to_owned),
            instructions: obj
                .get("instructions")
                .and_then(|v| v.as_str())
                .map(str::to_owned),
        };

        let mut guard = self
            .server_info
            .write()
            .map_err(|_| "Lock poisoned".to_owned())?;
        *guard = info;
        return_value.set_bool(true);
        Ok(())
    }
}

impl SimpleAddin for McpAddIn {
    fn name() -> &'static CStr1C {
        name!("mcp")
    }
    fn init(&mut self, interface: &'static addin1c::Connection) -> bool {
        self.connection = Some(interface);
        interface.set_event_buffer_depth(128);
        true
    }
    fn save_error(&mut self, err: Option<Box<dyn Error>>) {
        if let Some(ref error) = err {
            report_platform_error(self.connection, "WebTransport.MCP", error.as_ref());
        }
        self.last_error = err;
    }
    fn methods() -> &'static [MethodInfo<Self>] {
        &[
            MethodInfo {
                name: name!("Запустить"),
                method: Methods::Method3(Self::mcp_start),
            },
            MethodInfo {
                name: name!("Остановить"),
                method: Methods::Method0(Self::mcp_stop),
            },
            MethodInfo {
                name: name!("ОтправитьОтвет"),
                method: Methods::Method4(Self::mcp_send_response),
            },
            MethodInfo {
                name: name!("ЗавершитьЗадачу"),
                method: Methods::Method4(Self::mcp_complete_task),
            },
            MethodInfo {
                name: name!("УстановитьСтатусЗадачи"),
                method: Methods::Method3(Self::mcp_set_task_status),
            },
            MethodInfo {
                name: name!("УведомитьОПрогрессеЗадачи"),
                method: Methods::Method4(Self::mcp_notify_task_progress),
            },
            MethodInfo {
                name: name!("УведомитьОПрогрессе"),
                method: Methods::Method4(Self::mcp_notify_progress),
            },
            MethodInfo {
                name: name!("УстановитьРазрешенныеOrigins"),
                method: Methods::Method1(Self::mcp_set_allowed_origins),
            },
            MethodInfo {
                name: name!("ЗарегистрироватьИнструмент"),
                method: Methods::Method1(Self::register_tools),
            },
            MethodInfo {
                name: name!("СнятьРегистрациюИнструмента"),
                method: Methods::Method1(Self::unregister_tool),
            },
            MethodInfo {
                name: name!("ОчиститьИнструменты"),
                method: Methods::Method0(Self::clear_tools),
            },
            MethodInfo {
                name: name!("ЗарегистрироватьРесурс"),
                method: Methods::Method1(Self::register_resources),
            },
            MethodInfo {
                name: name!("СнятьРегистрациюРесурса"),
                method: Methods::Method1(Self::unregister_resource),
            },
            MethodInfo {
                name: name!("ОчиститьРесурсы"),
                method: Methods::Method0(Self::clear_resources),
            },
            MethodInfo {
                name: name!("ЗарегистрироватьШаблонРесурса"),
                method: Methods::Method1(Self::register_resource_templates),
            },
            MethodInfo {
                name: name!("СнятьРегистрациюШаблонаРесурса"),
                method: Methods::Method1(Self::unregister_resource_template),
            },
            MethodInfo {
                name: name!("ОчиститьШаблоныРесурсов"),
                method: Methods::Method0(Self::clear_resource_templates),
            },
            MethodInfo {
                name: name!("ЗарегистрироватьПромпт"),
                method: Methods::Method1(Self::register_prompts),
            },
            MethodInfo {
                name: name!("СнятьРегистрациюПромпта"),
                method: Methods::Method1(Self::unregister_prompt),
            },
            MethodInfo {
                name: name!("ОчиститьПромпты"),
                method: Methods::Method0(Self::clear_prompts),
            },
            MethodInfo {
                name: name!("УведомитьОбИзмененииИнструментов"),
                method: Methods::Method0(Self::notify_tools_changed),
            },
            MethodInfo {
                name: name!("УведомитьОбИзмененииРесурсов"),
                method: Methods::Method0(Self::notify_resources_changed),
            },
            MethodInfo {
                name: name!("УведомитьОбИзмененииПромптов"),
                method: Methods::Method0(Self::notify_prompts_changed),
            },
            MethodInfo {
                name: name!("УведомитьОбОбновленииРесурса"),
                method: Methods::Method1(Self::notify_resource_updated),
            },
            MethodInfo {
                name: name!("Версия"),
                method: Methods::Method0(Self::version),
            },
            MethodInfo {
                name: name!("УстановитьИнформациюОСервере"),
                method: Methods::Method1(Self::set_server_info),
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

impl Default for McpAddIn {
    fn default() -> Self {
        Self {
            connection: None,
            last_error: None,
            server: None,
            response_map: Arc::new(Mutex::new(HashMap::new())),
            request_counter: Arc::new(AtomicU64::new(1)),
            allow_list: Arc::new(RwLock::new(AllowList::default_local())),
            registry: Arc::new(RwLock::new(Registry::default())),
            client_sinks: Arc::new(Mutex::new(Vec::new())),
            server_info: Arc::new(RwLock::new(McpServerInfo::default())),
            subscriptions: Arc::new(Mutex::new(HashMap::new())),
            tasks: Arc::new(Mutex::new(HashMap::new())),
            runtime: Arc::new(Runtime::new().unwrap()),
        }
    }
}

fn parse_json_items(raw: &str) -> Result<Vec<Value>, Box<dyn Error>> {
    if raw.trim().is_empty() {
        return Err("Ожидается JSON-строка".to_owned().into());
    }
    let value: Value = serde_json::from_str(raw)?;
    match value {
        Value::Array(items) => Ok(items),
        Value::Object(_) => Ok(vec![value]),
        _ => Err("Ожидается JSON объект или массив".to_owned().into()),
    }
}

fn is_task_not_found_error(err: &str) -> bool {
    err == "Не найдена MCP задача"
}

fn parse_tool(value: Value) -> Result<rmcp::model::Tool, Box<dyn Error>> {
    if value.get("outputSchema").is_some() {
        return Err("outputSchema не поддерживается".to_owned().into());
    }
    if value.get("inputSchema").is_none() {
        return Err("Отсутствует inputSchema".to_owned().into());
    }
    serde_json::from_value(value).map_err(|err| format!("Некорректный инструмент: {err}").into())
}

fn parse_resource_template(value: Value) -> Result<rmcp::model::ResourceTemplate, Box<dyn Error>> {
    serde_json::from_value(value)
        .map_err(|err| format!("Некорректный шаблон ресурса: {err}").into())
}

fn parse_task_status(raw: &str) -> Result<rmcp::model::TaskStatus, Box<dyn Error>> {
    match raw.trim() {
        "working" => Ok(rmcp::model::TaskStatus::Working),
        "input_required" => Ok(rmcp::model::TaskStatus::InputRequired),
        "completed" => Ok(rmcp::model::TaskStatus::Completed),
        "failed" => Ok(rmcp::model::TaskStatus::Failed),
        "cancelled" => Ok(rmcp::model::TaskStatus::Cancelled),
        _ => Err("Некорректный статус MCP задачи".to_owned().into()),
    }
}

fn parse_numeric_arg(value: &Variant) -> Result<f64, Box<dyn Error>> {
    value
        .get_f64()
        .or_else(|_| value.get_i32().map(|number| number as f64))
        .map_err(|err| -> Box<dyn Error> { err.into() })
}

fn parse_progress_token(value: &Variant) -> Result<rmcp::model::ProgressToken, Box<dyn Error>> {
    if let Ok(number) = value.get_i32() {
        return Ok(rmcp::model::ProgressToken(
            rmcp::model::NumberOrString::Number(i64::from(number)),
        ));
    }

    if let Ok(number) = value.get_f64() {
        if number.is_finite()
            && number.fract() == 0.0
            && number >= i64::MIN as f64
            && number <= i64::MAX as f64
        {
            return Ok(rmcp::model::ProgressToken(
                rmcp::model::NumberOrString::Number(number as i64),
            ));
        }
    }

    if let Ok(token) = value.get_string() {
        if !token.trim().is_empty() {
            return Ok(rmcp::model::ProgressToken(
                rmcp::model::NumberOrString::String(token.into()),
            ));
        }
    }

    Err("Некорректный progressToken".to_owned().into())
}

#[cfg(feature = "validate-schema")]
fn compile_schema(schema: Value) -> Result<Validator, Box<dyn Error>> {
    let options = jsonschema::options()
        .with_draft(Draft::Draft202012)
        .should_validate_formats(true)
        .should_ignore_unknown_formats(false);
    options
        .build(&schema)
        .map_err(|err| format!("Некорректный JSON Schema: {err}").into())
}

#[cfg(test)]
mod tests {
    use super::{parse_json_items, parse_resource_template, parse_task_status};
    use serde_json::json;

    #[test]
    fn parse_json_items_accepts_single_object_and_arrays() {
        let one = parse_json_items(r#"{"uriTemplate":"str://users/{id}","name":"users"}"#)
            .expect("single object should parse");
        assert_eq!(one.len(), 1);

        let many = parse_json_items(
            r#"[{"uriTemplate":"str://users/{id}","name":"users"},{"uriTemplate":"str://posts/{id}","name":"posts"}]"#,
        )
        .expect("array should parse");
        assert_eq!(many.len(), 2);
    }

    #[test]
    fn parse_resource_template_requires_rmcp_shape() {
        let template = parse_resource_template(json!({
            "uriTemplate": "str://users/{id}",
            "name": "users",
        }))
        .expect("template should parse");
        assert_eq!(template.uri_template, "str://users/{id}");

        let error = parse_resource_template(json!({
            "name": "users",
        }))
        .expect_err("invalid template should fail");
        assert!(error.to_string().contains("Некорректный шаблон ресурса"));
    }

    #[test]
    fn parse_task_status_accepts_known_values() {
        assert_eq!(
            parse_task_status("working").unwrap(),
            rmcp::model::TaskStatus::Working
        );
        assert_eq!(
            parse_task_status("input_required").unwrap(),
            rmcp::model::TaskStatus::InputRequired
        );
        assert_eq!(
            parse_task_status("completed").unwrap(),
            rmcp::model::TaskStatus::Completed
        );
        assert!(parse_task_status("unknown").is_err());
    }
}
