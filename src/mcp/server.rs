use std::collections::{HashMap, HashSet};
use std::convert::Infallible;
use std::error::Error;
use std::net::SocketAddr;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, RwLock,
};
use std::time::Duration;

use addin1c::{name, CString1C};
use axum::http::{HeaderMap, HeaderValue, Method, Request, Response, StatusCode};
use axum::routing::get;
use axum::Router;
use bytes::Bytes;
use futures_util::future::BoxFuture;
use http_body_util::{BodyExt, Full};
use http_body_util::combinators::BoxBody;
use rmcp::ErrorData as McpError;
use rmcp::handler::server::ServerHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, CustomNotification, CustomRequest, CustomResult,
    ErrorCode, GetPromptRequestParams, GetPromptResult, InitializeRequestParams, InitializeResult,
    ListPromptsResult, ListResourcesResult, ListResourceTemplatesResult, ListToolsResult,
    PaginatedRequestParams, ReadResourceRequestParams, ReadResourceResult, ServerCapabilities,
};
use rmcp::service::{NotificationContext, RequestContext, RoleServer};
use rmcp::transport::streamable_http_server::session::never::NeverSessionManager;
use rmcp::transport::streamable_http_server::tower::{
    StreamableHttpServerConfig, StreamableHttpService,
};
use tokio::sync::{oneshot, Mutex};
use tokio_util::sync::CancellationToken;
use tower::{Layer, Service};

use super::registry::Registry;

pub(super) struct McpServerState {
    pub(super) shutdown: oneshot::Sender<()>,
    pub(super) _join: tokio::task::JoinHandle<()>,
}

#[derive(Debug, Clone)]
pub(super) enum AllowList {
    Any,
    List(HashSet<String>),
}

impl AllowList {
    pub(super) fn default_local() -> Self {
        let mut set = HashSet::new();
        set.insert("http://localhost".to_owned());
        set.insert("http://127.0.0.1".to_owned());
        AllowList::List(set)
    }

    fn allows(&self, origin: &str) -> bool {
        match self {
            AllowList::Any => true,
            AllowList::List(list) => list.contains(origin),
        }
    }

    fn is_any(&self) -> bool {
        matches!(self, AllowList::Any)
    }
}

pub(super) fn parse_allow_list(raw: &str) -> Result<AllowList, Box<dyn Error>> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(AllowList::default_local());
    }
    if trimmed == "*" {
        return Ok(AllowList::Any);
    }
    let list = serde_json::from_str::<Vec<String>>(trimmed)?;
    if list.iter().any(|value| value == "*") {
        return Ok(AllowList::Any);
    }
    let set = list
        .into_iter()
        .filter(|value| !value.trim().is_empty())
        .collect::<HashSet<_>>();
    Ok(AllowList::List(set))
}

#[derive(Debug)]
pub(super) struct McpResponse {
    pub(super) status: u16,
    pub(super) headers: HashMap<String, String>,
    pub(super) body: String,
}

pub(super) fn start_mcp_server(
    runtime: Arc<tokio::runtime::Runtime>,
    address: SocketAddr,
    connection: Option<&'static addin1c::Connection>,
    allow_list: Arc<RwLock<AllowList>>,
    response_map: Arc<Mutex<HashMap<String, oneshot::Sender<McpResponse>>>>,
    request_counter: Arc<AtomicU64>,
    registry: Arc<RwLock<Registry>>,
    response_timeout: Duration,
) -> Result<McpServerState, Box<dyn Error>> {
    let (shutdown_tx, shutdown_rx) = oneshot::channel();

    let handler = Arc::new(McpBridgeHandler {
        connection,
        response_map,
        request_counter,
        registry,
        response_timeout,
    });

    let service = StreamableHttpService::new(
        move || Ok(handler.clone()),
        Arc::new(NeverSessionManager::default()),
        StreamableHttpServerConfig {
            stateful_mode: false,
            json_response: true,
            sse_keep_alive: None,
            sse_retry: None,
            cancellation_token: CancellationToken::new(),
        },
    );

    let service = AllowListLayer { allow_list }.layer(service);

    let app = Router::new()
        .route("/", get(|| async { "MCP server" }))
        .route_service("/mcp", service);

    let listener = runtime.block_on(async { tokio::net::TcpListener::bind(address).await })?;

    let join = runtime.spawn(async move {
        let server = axum::serve(listener, app).with_graceful_shutdown(async {
            let _ = shutdown_rx.await;
        });
        let _ = server.await;
    });

    Ok(McpServerState {
        shutdown: shutdown_tx,
        _join: join,
    })
}

#[derive(Clone)]
struct AllowListLayer {
    allow_list: Arc<RwLock<AllowList>>,
}

impl<S> Layer<S> for AllowListLayer {
    type Service = AllowListService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        AllowListService {
            inner,
            allow_list: self.allow_list.clone(),
        }
    }
}

#[derive(Clone)]
struct AllowListService<S> {
    inner: S,
    allow_list: Arc<RwLock<AllowList>>,
}

type BoxResponse = Response<BoxBody<Bytes, Infallible>>;

type BoxFutureResponse = BoxFuture<'static, Result<BoxResponse, Infallible>>;

impl<S, B> Service<Request<B>> for AllowListService<S>
where
    S: Service<Request<B>, Response = BoxResponse, Error = Infallible> + Clone + Send + 'static,
    S::Future: Send + 'static,
    B: Send + 'static,
{
    type Response = BoxResponse;
    type Error = Infallible;
    type Future = BoxFutureResponse;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<B>) -> Self::Future {
        let mut req = req;
        normalize_protocol_version(&mut req);

        let origin = req
            .headers()
            .get("origin")
            .and_then(|value| value.to_str().ok())
            .map(|value| value.to_owned());
        let is_options = req.method() == Method::OPTIONS;

        let (allowed, allow_any) = {
            let guard = self.allow_list.read();
            match guard {
                Ok(guard) => {
                    let allow_any = guard.is_any();
                    let allowed = match origin.as_deref() {
                        Some(value) => guard.allows(value),
                        None => true,
                    };
                    (allowed, allow_any)
                }
                Err(_) => (false, false),
            }
        };

        let mut inner = self.inner.clone();
        Box::pin(async move {
            if !allowed {
                return Ok(forbidden_response());
            }

            if is_options {
                let mut response = Response::builder()
                    .status(StatusCode::NO_CONTENT)
                    .body(Full::new(Bytes::new()).boxed())
                    .expect("valid response");
                add_cors_headers(response.headers_mut(), origin.as_deref(), allow_any);
                return Ok(response);
            }

            let mut response = inner.call(req).await?;
            add_cors_headers(response.headers_mut(), origin.as_deref(), allow_any);
            Ok(response)
        })
    }
}

fn forbidden_response() -> BoxResponse {
    Response::builder()
        .status(StatusCode::FORBIDDEN)
        .body(Full::new(Bytes::from("Forbidden: Origin is not allowed")).boxed())
        .expect("valid response")
}

fn add_cors_headers(headers: &mut HeaderMap, origin: Option<&str>, allow_any: bool) {
    let allow_origin = if allow_any {
        "*"
    } else {
        origin.unwrap_or("*")
    };

    headers.insert(
        "Access-Control-Allow-Origin",
        HeaderValue::from_str(allow_origin).unwrap_or_else(|_| HeaderValue::from_static("*")),
    );
    headers.insert(
        "Access-Control-Allow-Methods",
        HeaderValue::from_static("GET, POST, OPTIONS"),
    );
    headers.insert(
        "Access-Control-Allow-Headers",
        HeaderValue::from_static(
            "content-type, authorization, mcp-protocol-version, mcp-session-id",
        ),
    );
    headers.insert(
        "Access-Control-Max-Age",
        HeaderValue::from_static("86400"),
    );
}

fn normalize_protocol_version<B>(req: &mut Request<B>) {
    const SUPPORTED_VERSION: &str = "2025-06-18";
    const KNOWN_VERSIONS: [&str; 3] = ["2024-11-05", "2025-03-26", "2025-06-18"];
    if let Some(value) = req.headers().get("mcp-protocol-version") {
        let value = value.to_str().ok().unwrap_or_default();
        if value == "2025-11-25" || value.contains("2025-11-25") {
            let _ = req
                .headers_mut()
                .insert("mcp-protocol-version", HeaderValue::from_static(SUPPORTED_VERSION));
            return;
        }
        if !KNOWN_VERSIONS.iter().any(|known| value.contains(known)) {
            req.headers_mut().remove("mcp-protocol-version");
        }
    }
}

#[derive(Clone)]
struct McpBridgeHandler {
    connection: Option<&'static addin1c::Connection>,
    response_map: Arc<Mutex<HashMap<String, oneshot::Sender<McpResponse>>>>,
    request_counter: Arc<AtomicU64>,
    registry: Arc<RwLock<Registry>>,
    response_timeout: Duration,
}

impl McpBridgeHandler {
    async fn dispatch_request<T>(&self, event: &str, payload: serde_json::Value) -> Result<T, McpError>
    where
        T: serde::de::DeserializeOwned,
    {
        let request_id = self.request_counter.fetch_add(1, Ordering::Relaxed).to_string();
        let payload = insert_request_id(payload, request_id.clone())?;

        let (tx, rx) = oneshot::channel();
        {
            let mut map = self.response_map.lock().await;
            map.insert(request_id.clone(), tx);
        }
        if let Err(err) = self.emit_event(event, payload) {
            let mut map = self.response_map.lock().await;
            map.remove(&request_id);
            return Err(err);
        }

        let response = match tokio::time::timeout(self.response_timeout, rx).await {
            Ok(Ok(response)) => response,
            Ok(Err(_)) => {
                let mut map = self.response_map.lock().await;
                map.remove(&request_id);
                return Err(McpError::internal_error("Response channel closed", None));
            }
            Err(_) => {
                let mut map = self.response_map.lock().await;
                map.remove(&request_id);
                return Err(McpError::internal_error("Handler timeout", None));
            }
        };

        parse_mcp_response::<T>(response)
    }

    fn emit_event(&self, event: &str, payload: serde_json::Value) -> Result<(), McpError> {
        let Some(connection) = self.connection else {
            return Err(McpError::internal_error(
                "Event connection is unavailable",
                None,
            ));
        };

        let data = CString1C::from(payload.to_string().as_str());
        let event = CString1C::from(event);
        if !connection.external_event(name!("WebTransport"), event, data) {
            return Err(McpError::internal_error("Event queue is full", None));
        }

        Ok(())
    }

    async fn dispatch_notification(
        &self,
        method: &str,
        params: Option<serde_json::Value>,
    ) {
        let payload = serde_json::json!({
            "method": method,
            "params": params.unwrap_or(serde_json::Value::Null),
        });

        let _ = self.emit_event("MCP_NOTIFICATION", payload);
    }
}

impl ServerHandler for McpBridgeHandler {
    fn initialize(
        &self,
        request: InitializeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<InitializeResult, McpError>> + Send + '_ {
        if context.peer.peer_info().is_none() {
            context.peer.set_peer_info(request.clone());
        }
        async move {
            let capabilities = ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .enable_prompts()
                .build();
            Ok(InitializeResult::new(capabilities))
        }
    }

    fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<ListToolsResult, McpError>> + Send + '_ {
        async move {
            let tools = {
                let guard = self
                    .registry
                    .read()
                    .map_err(|_| McpError::internal_error("Registry lock poisoned", None))?;
                guard.list_tools()
            };
            Ok(ListToolsResult::with_all_items(tools))
        }
    }

    fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<CallToolResult, McpError>> + Send + '_ {
        async move {
            let entry = {
                let guard = self
                    .registry
                    .read()
                    .map_err(|_| McpError::internal_error("Registry lock poisoned", None))?;
                guard.get_tool(request.name.as_ref())
            }
            .ok_or_else(|| McpError::invalid_params("tool not found", None))?;

            let validation_value = match request.arguments.clone() {
                Some(map) => serde_json::Value::Object(map),
                None => serde_json::Value::Object(serde_json::Map::new()),
            };
            let errors = entry
                .schema
                .iter_errors(&validation_value)
                .map(|err| err.to_string())
                .collect::<Vec<_>>();
            if !errors.is_empty() {
                let message = errors.join("; ");
                let message = if message.is_empty() {
                    "Invalid params".to_owned()
                } else {
                    message
                };
                return Err(McpError::invalid_params(message, None));
            }

            let args_payload = match request.arguments {
                Some(map) => serde_json::Value::Object(map),
                None => serde_json::Value::Null,
            };
            let payload = serde_json::json!({
                "name": request.name,
                "arguments": args_payload,
            });

            self.dispatch_request("MCP_TOOL_CALL", payload).await
        }
    }

    fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<ListResourcesResult, McpError>> + Send + '_ {
        async move {
            let resources = {
                let guard = self
                    .registry
                    .read()
                    .map_err(|_| McpError::internal_error("Registry lock poisoned", None))?;
                guard.list_resources()
            };
            Ok(ListResourcesResult::with_all_items(resources))
        }
    }

    fn list_resource_templates(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<ListResourceTemplatesResult, McpError>> + Send + '_
    {
        async move { Ok(ListResourceTemplatesResult::with_all_items(Vec::new())) }
    }

    fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<ReadResourceResult, McpError>> + Send + '_ {
        async move {
            let uri = request.uri.clone();
            let exists = {
                let guard = self
                    .registry
                    .read()
                    .map_err(|_| McpError::internal_error("Registry lock poisoned", None))?;
                guard.get_resource(uri.as_str())
            };
            if exists.is_none() {
                return Err(McpError::resource_not_found("resource not found", None));
            }
            let payload = serde_json::json!({ "uri": uri });
            self.dispatch_request("MCP_RESOURCE_READ", payload).await
        }
    }

    fn list_prompts(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<ListPromptsResult, McpError>> + Send + '_ {
        async move {
            let prompts = {
                let guard = self
                    .registry
                    .read()
                    .map_err(|_| McpError::internal_error("Registry lock poisoned", None))?;
                guard.list_prompts()
            };
            Ok(ListPromptsResult::with_all_items(prompts))
        }
    }

    fn get_prompt(
        &self,
        request: GetPromptRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<GetPromptResult, McpError>> + Send + '_ {
        async move {
            let name = request.name.clone();
            let exists = {
                let guard = self
                    .registry
                    .read()
                    .map_err(|_| McpError::internal_error("Registry lock poisoned", None))?;
                guard.get_prompt(name.as_str())
            };
            if exists.is_none() {
                return Err(McpError::invalid_params("prompt not found", None));
            }
            let args_payload = match request.arguments {
                Some(map) => serde_json::Value::Object(map),
                None => serde_json::Value::Null,
            };
            let payload = serde_json::json!({
                "name": name,
                "arguments": args_payload,
            });
            self.dispatch_request("MCP_PROMPT_GET", payload).await
        }
    }

    fn on_custom_request(
        &self,
        request: CustomRequest,
        _context: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<CustomResult, McpError>> + Send + '_ {
        async move { Err(McpError::new(ErrorCode::METHOD_NOT_FOUND, request.method, None)) }
    }

    fn on_initialized(
        &self,
        _context: NotificationContext<RoleServer>,
    ) -> impl std::future::Future<Output = ()> + Send + '_ {
        async move { self.dispatch_notification("notifications/initialized", None).await }
    }

    fn on_progress(
        &self,
        notification: rmcp::model::ProgressNotificationParam,
        _context: NotificationContext<RoleServer>,
    ) -> impl std::future::Future<Output = ()> + Send + '_ {
        async move {
            let params = serde_json::to_value(notification).ok();
            self.dispatch_notification("notifications/progress", params)
                .await
        }
    }

    fn on_cancelled(
        &self,
        notification: rmcp::model::CancelledNotificationParam,
        _context: NotificationContext<RoleServer>,
    ) -> impl std::future::Future<Output = ()> + Send + '_ {
        async move {
            let params = serde_json::to_value(notification).ok();
            self.dispatch_notification("notifications/cancelled", params)
                .await
        }
    }

    fn on_roots_list_changed(
        &self,
        _context: NotificationContext<RoleServer>,
    ) -> impl std::future::Future<Output = ()> + Send + '_ {
        async move { self.dispatch_notification("notifications/roots/list_changed", None).await }
    }

    fn on_custom_notification(
        &self,
        notification: CustomNotification,
        _context: NotificationContext<RoleServer>,
    ) -> impl std::future::Future<Output = ()> + Send + '_ {
        async move {
            self.dispatch_notification(notification.method.as_str(), notification.params)
                .await
        }
    }
}

fn insert_request_id(
    payload: serde_json::Value,
    request_id: String,
) -> Result<serde_json::Value, McpError> {
    match payload {
        serde_json::Value::Object(mut map) => {
            map.insert("id".to_owned(), serde_json::Value::String(request_id));
            Ok(serde_json::Value::Object(map))
        }
        _ => Err(McpError::internal_error(
            "Invalid request payload",
            None,
        )),
    }
}

fn parse_mcp_response<T>(response: McpResponse) -> Result<T, McpError>
where
    T: serde::de::DeserializeOwned,
{
    let _ = response.headers;
    let status = response.status;
    if response.body.trim().is_empty() {
        return Err(McpError::internal_error("Empty response body", None));
    }

    let value: serde_json::Value = serde_json::from_str(response.body.as_str())
        .map_err(|err| McpError::internal_error(err.to_string(), None))?;

    if let Some(error_value) = value.get("error") {
        let error = serde_json::from_value::<rmcp::model::ErrorData>(error_value.clone())
            .unwrap_or_else(|err| McpError::internal_error(err.to_string(), None));
        return Err(error);
    }

    if status >= 400 {
        return Err(McpError::internal_error(
            format!("Handler returned HTTP {}", status),
            Some(value),
        ));
    }

    if let Some(result_value) = value.get("result") {
        return serde_json::from_value::<T>(result_value.clone())
            .map_err(|err| McpError::internal_error(err.to_string(), None));
    }

    serde_json::from_value::<T>(value)
        .map_err(|err| McpError::internal_error(err.to_string(), None))
}
