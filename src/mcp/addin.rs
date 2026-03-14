use std::collections::HashMap;
use std::error::Error;
use std::sync::{atomic::AtomicU64, Arc, RwLock};

use addin1c::{name, AddinResult, CStr1C, MethodInfo, Methods, PropInfo, SimpleAddin, Variant};
use jsonschema::{Draft, Validator};
use rmcp::service::ClientSink;
use serde_json::Value;
use tokio::runtime::Runtime;
use tokio::sync::{oneshot, Mutex};

use super::registry::Registry;
use super::server::{parse_allow_list, start_mcp_server, AllowList, McpResponse, McpServerState};
use crate::VERSION;
pub struct McpAddIn {
    pub(super) connection: Option<&'static addin1c::Connection>,
    pub(super) runtime: Arc<Runtime>,
    pub(super) server: Option<McpServerState>,
    pub(super) response_map: Arc<Mutex<HashMap<String, oneshot::Sender<McpResponse>>>>,
    pub(super) request_counter: Arc<AtomicU64>,
    pub(super) allow_list: Arc<RwLock<AllowList>>,
    pub(super) registry: Arc<RwLock<Registry>>,
    pub(super) client_sinks: Arc<Mutex<Vec<ClientSink>>>,
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
        self.runtime.clone().block_on(async {
            let mut map = response_map.lock().await;
            map.clear();
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

        let headers = parse_headers(json_headers)?;
        let response = McpResponse {
            status: status_code as u16,
            headers,
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
            let schema_value = Value::Object(tool.input_schema.as_ref().clone());
            let schema = compile_schema(schema_value)?;
            guard.register_tool(tool, schema);
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
        self.runtime.block_on(
            server.broadcast_notification(
                rmcp::model::ServerNotification::ToolListChangedNotification(Default::default()),
            ),
        );
        return_value.set_bool(true);
        Ok(())
    }

    fn notify_resources_changed(&mut self, return_value: &mut Variant) -> AddinResult {
        let Some(server) = self.server.as_ref() else {
            return Err("MCP сервер не запущен".to_owned().into());
        };
        self.runtime.block_on(
            server.broadcast_notification(
                rmcp::model::ServerNotification::ResourceListChangedNotification(
                    Default::default(),
                ),
            ),
        );
        return_value.set_bool(true);
        Ok(())
    }

    fn notify_prompts_changed(&mut self, return_value: &mut Variant) -> AddinResult {
        let Some(server) = self.server.as_ref() else {
            return Err("MCP сервер не запущен".to_owned().into());
        };
        self.runtime.block_on(
            server.broadcast_notification(
                rmcp::model::ServerNotification::PromptListChangedNotification(Default::default()),
            ),
        );
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
        self.runtime.block_on(
            server.broadcast_notification(
                rmcp::model::ServerNotification::ResourceUpdatedNotification(
                    rmcp::model::ResourceUpdatedNotification::new(
                        rmcp::model::ResourceUpdatedNotificationParam::new(uri),
                    ),
                ),
            ),
        );
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
        self.last_error = err;
    }
    fn methods() -> &'static [MethodInfo<Self>] {
        &[
            MethodInfo {
                name: name!("ЗапуститьMCP"),
                method: Methods::Method3(Self::mcp_start),
            },
            MethodInfo {
                name: name!("ОстановитьMCP"),
                method: Methods::Method0(Self::mcp_stop),
            },
            MethodInfo {
                name: name!("ОтправитьMCPОтвет"),
                method: Methods::Method4(Self::mcp_send_response),
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
            runtime: Arc::new(Runtime::new().unwrap()),
        }
    }
}

fn parse_headers(json_headers: String) -> Result<HashMap<String, String>, Box<dyn Error>> {
    if json_headers.is_empty() {
        return Ok(HashMap::new());
    }
    let raw = serde_json::from_str::<HashMap<String, serde_json::Value>>(&json_headers)?;
    Ok(raw
        .into_iter()
        .map(|(key, value)| {
            let value = match value {
                serde_json::Value::Null => "".to_owned(),
                serde_json::Value::Bool(bool) => bool.to_string(),
                serde_json::Value::Number(number) => number.to_string(),
                serde_json::Value::String(str) => str,
                serde_json::Value::Array(_) => "".to_owned(),
                serde_json::Value::Object(_) => "".to_owned(),
            };
            (key, value)
        })
        .collect())
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
    use super::{parse_json_items, parse_resource_template};
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
}
