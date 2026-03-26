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
use axum::body::Body;
use axum::extract::Request as AxumRequest;
use axum::http::{HeaderMap, HeaderValue, Method, Request, Response, StatusCode};
use axum::middleware::{self, Next};
use axum::routing::get;
use axum::Router;
use bytes::Bytes;
use futures_util::future::BoxFuture;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full};
use rmcp::handler::server::ServerHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, CancelTaskParams, CancelTaskResult,
    CustomNotification, CustomRequest, CustomResult, ErrorCode, GetPromptRequestParams,
    GetPromptResult, GetTaskInfoParams, GetTaskPayloadResult, GetTaskResult, GetTaskResultParams,
    Implementation, InitializeRequestParams, InitializeResult, ListPromptsResult,
    ListResourceTemplatesResult, ListResourcesResult, ListTasksResult, ListToolsResult,
    PaginatedRequestParams, ProgressNotification, ProgressNotificationParam, ProgressToken,
    ReadResourceRequestParams, ReadResourceResult, ResourceUpdatedNotificationParam,
    ServerCapabilities, SubscribeRequestParams, Task, TaskRequestsCapability, TaskStatus,
    TasksCapability, ToolsTaskCapability, UnsubscribeRequestParams,
};
use rmcp::service::{ClientSink, NotificationContext, RequestContext, RoleServer};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::tower::{
    StreamableHttpServerConfig, StreamableHttpService,
};
use rmcp::ErrorData as McpError;
use tokio::sync::{oneshot, Mutex};
use tokio_util::sync::CancellationToken;
use tower::{Layer, Service};

use super::registry::{Registry, ResolveResourceError, ResolvedResource};

/// Server identity and instructions, settable from 1C before starting the server.
#[derive(Clone, Debug, Default)]
pub(super) struct McpServerInfo {
    pub name: Option<String>,
    pub version: Option<String>,
    pub title: Option<String>,
    pub description: Option<String>,
    pub instructions: Option<String>,
}

pub(super) struct McpServerState {
    pub(super) shutdown: oneshot::Sender<()>,
    pub(super) _join: tokio::task::JoinHandle<()>,
    handler: Arc<McpBridgeHandler>,
}

impl McpServerState {
    pub(super) async fn broadcast_notification(
        &self,
        notification: rmcp::model::ServerNotification,
    ) {
        self.handler.broadcast_notification(notification).await;
    }

    pub(super) async fn notify_resource_updated(&self, uri: String) {
        self.handler.notify_resource_updated(uri).await;
    }

    pub(super) async fn complete_task(
        &self,
        task_id: &str,
        response: McpResponse,
    ) -> Result<(), String> {
        self.handler.complete_task(task_id, response).await
    }

    pub(super) async fn update_task_status(
        &self,
        task_id: &str,
        status: TaskStatus,
        message: Option<String>,
    ) -> Result<(), String> {
        self.handler.update_task_status(task_id, status, message).await
    }

    pub(super) async fn notify_task_progress(
        &self,
        task_id: &str,
        progress: f64,
        total: Option<f64>,
        message: Option<String>,
    ) -> Result<(), String> {
        self.handler
            .notify_task_progress(task_id, progress, total, message)
            .await
    }
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

#[derive(Debug, Clone)]
pub(super) struct TaskEntry {
    task: Task,
    result: Option<Result<serde_json::Value, McpError>>,
    progress_token: Option<ProgressToken>,
}

impl TaskEntry {
    fn new(task: Task, progress_token: Option<ProgressToken>) -> Self {
        Self {
            task,
            result: None,
            progress_token,
        }
    }
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
    client_sinks: Arc<Mutex<Vec<ClientSink>>>,
    server_info: Arc<RwLock<McpServerInfo>>,
    subscriptions: Arc<Mutex<HashMap<String, Vec<ClientSink>>>>,
    tasks: Arc<Mutex<HashMap<String, TaskEntry>>>,
) -> Result<McpServerState, Box<dyn Error>> {
    let (shutdown_tx, shutdown_rx) = oneshot::channel();

    let handler = Arc::new(McpBridgeHandler {
        connection,
        response_map,
        request_counter,
        registry,
        response_timeout,
        client_sinks,
        server_info,
        subscriptions,
        tasks,
    });

    let service = StreamableHttpService::new(
        {
            let handler = handler.clone();
            move || Ok(handler.clone())
        },
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig {
            stateful_mode: true,
            json_response: true,
            sse_keep_alive: None,
            sse_retry: None,
            cancellation_token: CancellationToken::new(),
        },
    );

    let service = AllowListLayer { allow_list }.layer(service);

    let app = Router::new()
        .route("/", get(|| async { "MCP server" }))
        .route_service("/mcp", service)
        .layer(middleware::from_fn(intercept_orphan_initialized));

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
        handler,
    })
}

#[cfg(test)]
fn start_mcp_server_with_listener(
    runtime: Arc<tokio::runtime::Runtime>,
    listener: std::net::TcpListener,
    connection: Option<&'static addin1c::Connection>,
    allow_list: Arc<RwLock<AllowList>>,
    response_map: Arc<Mutex<HashMap<String, oneshot::Sender<McpResponse>>>>,
    request_counter: Arc<AtomicU64>,
    registry: Arc<RwLock<Registry>>,
    response_timeout: Duration,
    client_sinks: Arc<Mutex<Vec<ClientSink>>>,
    server_info: Arc<RwLock<McpServerInfo>>,
    subscriptions: Arc<Mutex<HashMap<String, Vec<ClientSink>>>>,
    tasks: Arc<Mutex<HashMap<String, TaskEntry>>>,
) -> Result<McpServerState, Box<dyn Error>> {
    let (shutdown_tx, shutdown_rx) = oneshot::channel();

    let handler = Arc::new(McpBridgeHandler {
        connection,
        response_map,
        request_counter,
        registry,
        response_timeout,
        client_sinks,
        server_info,
        subscriptions,
        tasks,
    });

    let service = StreamableHttpService::new(
        {
            let handler = handler.clone();
            move || Ok(handler.clone())
        },
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig {
            stateful_mode: true,
            json_response: true,
            sse_keep_alive: None,
            sse_retry: None,
            cancellation_token: CancellationToken::new(),
        },
    );

    let service = AllowListLayer { allow_list }.layer(service);

    let app = Router::new()
        .route("/", get(|| async { "MCP server" }))
        .route_service("/mcp", service)
        .layer(middleware::from_fn(intercept_orphan_initialized));

    listener.set_nonblocking(true)?;

    // Convert and serve entirely within the server's own runtime to avoid
    // calling block_on from a thread that already has a runtime context.
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();
    let join = runtime.spawn(async move {
        let tokio_listener = match tokio::net::TcpListener::from_std(listener) {
            Ok(l) => { let _ = ready_tx.send(Ok(())); l }
            Err(e) => { let _ = ready_tx.send(Err(e.to_string())); return; }
        };
        let server = axum::serve(tokio_listener, app).with_graceful_shutdown(async {
            let _ = shutdown_rx.await;
        });
        let _ = server.await;
    });

    ready_rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .map_err(|e| format!("Server did not start: {e}"))?
        .map_err(|e| format!("Listener conversion failed: {e}"))?;

    Ok(McpServerState {
        shutdown: shutdown_tx,
        _join: join,
        handler,
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
            response = maybe_convert_sse_to_json(response).await;
            add_cors_headers(response.headers_mut(), origin.as_deref(), allow_any);
            Ok(response)
        })
    }
}

async fn maybe_convert_sse_to_json(response: BoxResponse) -> BoxResponse {
    let is_sse = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.contains("text/event-stream"))
        .unwrap_or(false);
    let has_session_id = response.headers().contains_key("mcp-session-id");

    if !is_sse || !has_session_id {
        return response;
    }

    let session_id = response
        .headers()
        .get("mcp-session-id")
        .cloned();

    let (parts, body) = response.into_parts();
    let bytes = match body.collect().await {
        Ok(b) => b.to_bytes(),
        Err(_) => return Response::from_parts(parts, Full::new(Bytes::new()).boxed()),
    };

    // Extract JSON from SSE "data: {...}" line
    let text = match std::str::from_utf8(&bytes) {
        Ok(s) => s,
        Err(_) => return Response::from_parts(parts, Full::new(bytes).boxed()),
    };
    let json_data = text
        .lines()
        .find(|line| line.starts_with("data:"))
        .map(|line| line["data:".len()..].trim());

    let Some(json) = json_data else {
        return Response::from_parts(parts, Full::new(bytes).boxed());
    };

    let mut builder = Response::builder()
        .status(parts.status)
        .header("content-type", "application/json");
    if let Some(sid) = session_id {
        builder = builder.header("mcp-session-id", sid);
    }
    builder
        .body(Full::new(Bytes::copy_from_slice(json.as_bytes())).boxed())
        .unwrap_or_else(|_| Response::from_parts(parts, Full::new(bytes).boxed()))
}

async fn intercept_orphan_initialized(req: AxumRequest, next: Next) -> Response<Body> {
    let has_session_id = req.headers().contains_key("mcp-session-id");
    if !has_session_id && req.method() == Method::POST {
        let (parts, body) = req.into_parts();
        let bytes = match axum::body::to_bytes(body, 4096).await {
            Ok(b) => b,
            Err(_) => {
                return Response::builder()
                    .status(StatusCode::BAD_REQUEST)
                    .body(Body::empty())
                    .unwrap();
            }
        };
        let is_initialized = serde_json::from_slice::<serde_json::Value>(&bytes)
            .ok()
            .and_then(|v| v.get("method").and_then(|m| m.as_str()).map(|s| s.to_owned()))
            .map(|m| m == "notifications/initialized")
            .unwrap_or(false);
        if is_initialized {
            return Response::builder()
                .status(StatusCode::ACCEPTED)
                .body(Body::empty())
                .unwrap();
        }
        let req = AxumRequest::from_parts(parts, Body::from(bytes));
        return next.run(req).await;
    }
    next.run(req).await
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
    headers.insert("Access-Control-Max-Age", HeaderValue::from_static("86400"));
    headers.insert(
        "Access-Control-Expose-Headers",
        HeaderValue::from_static("mcp-session-id"),
    );
}

fn normalize_protocol_version<B>(req: &mut Request<B>) {
    const SUPPORTED_VERSION: &str = "2025-06-18";
    const KNOWN_VERSIONS: [&str; 3] = ["2024-11-05", "2025-03-26", "2025-06-18"];
    if let Some(value) = req.headers().get("mcp-protocol-version") {
        let value = value.to_str().ok().unwrap_or_default();
        if value == "2025-11-25" || value.contains("2025-11-25") {
            let _ = req.headers_mut().insert(
                "mcp-protocol-version",
                HeaderValue::from_static(SUPPORTED_VERSION),
            );
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
    client_sinks: Arc<Mutex<Vec<ClientSink>>>,
    server_info: Arc<RwLock<McpServerInfo>>,
    subscriptions: Arc<Mutex<HashMap<String, Vec<ClientSink>>>>,
    tasks: Arc<Mutex<HashMap<String, TaskEntry>>>,
}

impl McpBridgeHandler {
    async fn dispatch_request<T>(
        &self,
        event: &str,
        payload: serde_json::Value,
    ) -> Result<T, McpError>
    where
        T: serde::de::DeserializeOwned,
    {
        let request_id = self
            .request_counter
            .fetch_add(1, Ordering::Relaxed)
            .to_string();
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

    async fn dispatch_notification(&self, method: &str, params: Option<serde_json::Value>) {
        let payload = serde_json::json!({
            "method": method,
            "params": params.unwrap_or(serde_json::Value::Null),
        });

        let _ = self.emit_event("MCP_NOTIFICATION", payload);
    }

    async fn create_task_entry(
        &self,
        request: &CallToolRequestParams,
    ) -> Result<Task, McpError> {
        let task_id = self
            .request_counter
            .fetch_add(1, Ordering::Relaxed)
            .to_string();
        let timestamp = chrono::Utc::now().to_rfc3339();
        let task = Task::new(
            task_id.clone(),
            TaskStatus::Working,
            timestamp.clone(),
            timestamp,
        )
        .with_status_message(format!("Tool {} is running", request.name))
        .with_poll_interval(1_000);
        let progress_token = request.meta.as_ref().and_then(|meta| meta.get_progress_token());

        let mut tasks = self.tasks.lock().await;
        tasks.insert(task_id, TaskEntry::new(task.clone(), progress_token));
        Ok(task)
    }

    async fn update_task_status(
        &self,
        task_id: &str,
        status: TaskStatus,
        message: Option<String>,
    ) -> Result<(), String> {
        let mut tasks = self.tasks.lock().await;
        let entry = tasks
            .get_mut(task_id)
            .ok_or_else(|| "Не найдена MCP задача".to_owned())?;
        entry.task.status = status;
        entry.task.status_message = message;
        entry.task.last_updated_at = chrono::Utc::now().to_rfc3339();
        Ok(())
    }

    async fn complete_task(&self, task_id: &str, response: McpResponse) -> Result<(), String> {
        let parsed = parse_mcp_response::<serde_json::Value>(response);
        let mut tasks = self.tasks.lock().await;
        let entry = tasks
            .get_mut(task_id)
            .ok_or_else(|| "Не найдена MCP задача".to_owned())?;
        entry.task.last_updated_at = chrono::Utc::now().to_rfc3339();
        match parsed {
            Ok(result) => {
                entry.task.status = TaskStatus::Completed;
                entry.task.status_message = Some("Task completed".to_owned());
                entry.result = Some(Ok(result));
            }
            Err(error) => {
                entry.task.status = TaskStatus::Failed;
                entry.task.status_message = Some(error.message.to_string());
                entry.result = Some(Err(error));
            }
        }
        Ok(())
    }

    async fn notify_task_progress(
        &self,
        task_id: &str,
        progress: f64,
        total: Option<f64>,
        message: Option<String>,
    ) -> Result<(), String> {
        let progress_token = {
            let mut tasks = self.tasks.lock().await;
            let entry = tasks
                .get_mut(task_id)
                .ok_or_else(|| "Не найдена MCP задача".to_owned())?;
            entry.task.last_updated_at = chrono::Utc::now().to_rfc3339();
            if let Some(message) = message.as_ref() {
                entry.task.status_message = Some(message.clone());
            }
            entry.progress_token.clone()
        }
        .ok_or_else(|| "Для MCP задачи не задан progressToken".to_owned())?;

        let mut params = ProgressNotificationParam::new(progress_token, progress);
        if let Some(total) = total {
            params = params.with_total(total);
        }
        if let Some(message) = message {
            params = params.with_message(message);
        }
        let notification =
            rmcp::model::ServerNotification::ProgressNotification(ProgressNotification::new(
                params,
            ));
        self.broadcast_notification(notification).await;
        Ok(())
    }
}

async fn send_to_sinks(sinks: &[ClientSink], notification: rmcp::model::ServerNotification) {
    for sink in sinks {
        let _ = sink.send_notification(notification.clone()).await;
    }
}

impl McpBridgeHandler {
    pub(super) async fn broadcast_notification(
        &self,
        notification: rmcp::model::ServerNotification,
    ) {
        let sinks = {
            let mut sinks = self.client_sinks.lock().await;
            sinks.retain(|s| !s.is_transport_closed());
            sinks.clone()
        };
        send_to_sinks(&sinks, notification).await;
    }

    pub(super) async fn notify_resource_updated(&self, uri: String) {
        let sinks = {
            let mut subs = self.subscriptions.lock().await;
            let Some(entry) = subs.get_mut(&uri) else {
                return;
            };
            entry.retain(|s| !s.is_transport_closed());
            if entry.is_empty() {
                subs.remove(&uri);
                return;
            }
            entry.clone()
        };
        let notification = rmcp::model::ServerNotification::ResourceUpdatedNotification(
            rmcp::model::ResourceUpdatedNotification::new(ResourceUpdatedNotificationParam::new(
                uri,
            )),
        );
        send_to_sinks(&sinks, notification).await;
    }
}

fn peer_id(peer: &ClientSink) -> Option<usize> {
    peer.peer_info()
        .map(|info| info as *const _ as usize)
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
                .enable_tool_list_changed()
                .enable_tasks_with(TasksCapability {
                    requests: Some(TaskRequestsCapability {
                        tools: Some(ToolsTaskCapability {
                            call: Some(serde_json::Map::new()),
                        }),
                        ..Default::default()
                    }),
                    list: Some(serde_json::Map::new()),
                    cancel: Some(serde_json::Map::new()),
                })
                .enable_resources()
                .enable_resources_list_changed()
                .enable_resources_subscribe()
                .enable_prompts()
                .enable_prompts_list_changed()
                .build();
            let mut result = InitializeResult::new(capabilities);
            if let Ok(info) = self.server_info.read() {
                if let (Some(name), Some(version)) = (info.name.as_ref(), info.version.as_ref()) {
                    let mut impl_info = Implementation::new(name.clone(), version.clone());
                    if let Some(title) = info.title.as_ref() {
                        impl_info = impl_info.with_title(title.clone());
                    }
                    if let Some(description) = info.description.as_ref() {
                        impl_info = impl_info.with_description(description.clone());
                    }
                    result = result.with_server_info(impl_info);
                }
                if let Some(instructions) = info.instructions.as_ref() {
                    result = result.with_instructions(instructions.clone());
                }
            }
            Ok(result)
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

    fn get_tool(&self, name: &str) -> Option<rmcp::model::Tool> {
        self.registry
            .read()
            .ok()
            .and_then(|guard| guard.get_tool(name).map(|entry| entry.tool))
    }

    fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<CallToolResult, McpError>> + Send + '_ {
        async move {
            let _entry = {
                let guard = self
                    .registry
                    .read()
                    .map_err(|_| McpError::internal_error("Registry lock poisoned", None))?;
                guard.get_tool(request.name.as_ref())
            }
            .ok_or_else(|| McpError::invalid_params("tool not found", None))?;

            #[cfg(feature = "validate-schema")]
            {
                let validation_value = match request.arguments.clone() {
                    Some(map) => serde_json::Value::Object(map),
                    None => serde_json::Value::Object(serde_json::Map::new()),
                };
                let errors = _entry
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
            }

            let args_payload = match request.arguments {
                Some(map) => serde_json::Value::Object(map),
                None => serde_json::Value::Null,
            };
            let payload = serde_json::json!({
                "executionMode": "sync",
                "name": request.name,
                "arguments": args_payload,
                "progressToken": request.meta.as_ref().and_then(|meta| meta.get_progress_token()),
            });

            self.dispatch_request("MCP_TOOL_CALL", payload).await
        }
    }

    fn enqueue_task(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<rmcp::model::CreateTaskResult, McpError>> + Send + '_
    {
        async move {
            let _entry = {
                let guard = self
                    .registry
                    .read()
                    .map_err(|_| McpError::internal_error("Registry lock poisoned", None))?;
                guard.get_tool(request.name.as_ref())
            }
            .ok_or_else(|| McpError::invalid_params("tool not found", None))?;

            #[cfg(feature = "validate-schema")]
            {
                let validation_value = match request.arguments.clone() {
                    Some(map) => serde_json::Value::Object(map),
                    None => serde_json::Value::Object(serde_json::Map::new()),
                };
                let errors = _entry
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
            }

            let task = self.create_task_entry(&request).await?;
            let args_payload = match request.arguments {
                Some(map) => serde_json::Value::Object(map),
                None => serde_json::Value::Null,
            };
            let payload = serde_json::json!({
                "executionMode": "task",
                "taskId": task.task_id,
                "name": request.name,
                "arguments": args_payload,
                "progressToken": request.meta.as_ref().and_then(|meta| meta.get_progress_token()),
            });
            self.emit_event("MCP_TOOL_CALL", payload)?;
            Ok(rmcp::model::CreateTaskResult::new(task))
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
        async move {
            let resource_templates = {
                let guard = self
                    .registry
                    .read()
                    .map_err(|_| McpError::internal_error("Registry lock poisoned", None))?;
                guard.list_resource_templates()
            };
            Ok(ListResourceTemplatesResult::with_all_items(
                resource_templates,
            ))
        }
    }

    fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<ReadResourceResult, McpError>> + Send + '_ {
        async move {
            let uri = request.uri.clone();
            let resolved = {
                let guard = self
                    .registry
                    .read()
                    .map_err(|_| McpError::internal_error("Registry lock poisoned", None))?;
                guard.resolve_resource(uri.as_str())
            };

            let payload = match resolved {
                Ok(Some(ResolvedResource::Resource(_resource))) => {
                    serde_json::json!({ "uri": uri })
                }
                Ok(Some(ResolvedResource::Template(template))) => serde_json::json!({
                    "uri": uri,
                    "uriTemplate": template.template.uri_template,
                    "arguments": template.arguments,
                }),
                Ok(None) => {
                    return Err(McpError::resource_not_found("resource not found", None));
                }
                Err(ResolveResourceError::AmbiguousTemplates { uri, templates }) => {
                    return Err(McpError::invalid_params(
                        "resource URI matches multiple templates",
                        Some(serde_json::json!({
                            "uri": uri,
                            "uriTemplates": templates,
                        })),
                    ));
                }
            };

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

    fn subscribe(
        &self,
        request: SubscribeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<(), McpError>> + Send + '_ {
        async move {
            let uri = request.uri.clone();
            {
                let mut subs = self.subscriptions.lock().await;
                let entry = subs.entry(uri.clone()).or_default();
                entry.retain(|s| !s.is_transport_closed());
                entry.push(context.peer.clone());
            }
            let _ = self.emit_event(
                "MCP_RESOURCE_SUBSCRIBE",
                serde_json::json!({ "uri": uri }),
            );
            Ok(())
        }
    }

    fn unsubscribe(
        &self,
        request: UnsubscribeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<(), McpError>> + Send + '_ {
        async move {
            let uri = request.uri.clone();
            let requester_id = peer_id(&context.peer);
            {
                let mut subs = self.subscriptions.lock().await;
                if let Some(entry) = subs.get_mut(&uri) {
                    entry.retain(|s| {
                        !s.is_transport_closed()
                            && requester_id.map_or(true, |id| peer_id(s) != Some(id))
                    });
                    if entry.is_empty() {
                        subs.remove(&uri);
                    }
                }
            }
            let _ = self.emit_event(
                "MCP_RESOURCE_UNSUBSCRIBE",
                serde_json::json!({ "uri": uri }),
            );
            Ok(())
        }
    }

    fn on_custom_request(
        &self,
        request: CustomRequest,
        _context: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<CustomResult, McpError>> + Send + '_ {
        async move {
            Err(McpError::new(
                ErrorCode::METHOD_NOT_FOUND,
                request.method,
                None,
            ))
        }
    }

    fn on_initialized(
        &self,
        context: NotificationContext<RoleServer>,
    ) -> impl std::future::Future<Output = ()> + Send + '_ {
        async move {
            {
                let mut sinks = self.client_sinks.lock().await;
                sinks.retain(|s| !s.is_transport_closed());
                sinks.push(context.peer.clone());
            }
            self.dispatch_notification("notifications/initialized", None)
                .await
        }
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
        async move {
            self.dispatch_notification("notifications/roots/list_changed", None)
                .await
        }
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

    fn list_tasks(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<ListTasksResult, McpError>> + Send + '_ {
        async move {
            let tasks = {
                let tasks = self.tasks.lock().await;
                tasks.values().map(|entry| entry.task.clone()).collect::<Vec<_>>()
            };
            Ok(ListTasksResult::new(tasks))
        }
    }

    fn get_task_info(
        &self,
        request: GetTaskInfoParams,
        _context: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<GetTaskResult, McpError>> + Send + '_ {
        async move {
            let task = {
                let tasks = self.tasks.lock().await;
                tasks.get(request.task_id.as_str()).map(|entry| entry.task.clone())
            }
            .ok_or_else(|| McpError::invalid_params("task not found", None))?;
            Ok(GetTaskResult { meta: None, task })
        }
    }

    fn get_task_result(
        &self,
        request: GetTaskResultParams,
        _context: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<GetTaskPayloadResult, McpError>> + Send + '_ {
        async move {
            let result = {
                let tasks = self.tasks.lock().await;
                tasks.get(request.task_id.as_str()).and_then(|entry| entry.result.clone())
            }
            .ok_or_else(|| McpError::invalid_params("task result is not ready", None))?;
            match result {
                Ok(value) => Ok(GetTaskPayloadResult::new(value)),
                Err(error) => Err(error),
            }
        }
    }

    fn cancel_task(
        &self,
        request: CancelTaskParams,
        _context: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<CancelTaskResult, McpError>> + Send + '_ {
        async move {
            let task = {
                let mut tasks = self.tasks.lock().await;
                let entry = tasks
                    .get_mut(request.task_id.as_str())
                    .ok_or_else(|| McpError::invalid_params("task not found", None))?;
                entry.task.status = TaskStatus::Cancelled;
                entry.task.status_message = Some("Task was cancelled".to_owned());
                entry.task.last_updated_at = chrono::Utc::now().to_rfc3339();
                entry.result = Some(Err(McpError::invalid_request(
                    "Task was cancelled",
                    None,
                )));
                entry.task.clone()
            };
            let _ = self.emit_event(
                "MCP_TASK_CANCELLED",
                serde_json::json!({ "taskId": request.task_id }),
            );
            Ok(CancelTaskResult { meta: None, task })
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
        _ => Err(McpError::internal_error("Invalid request payload", None)),
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, RwLock};
    use tokio::sync::Mutex;

    // ── helpers ──────────────────────────────────────────────────────────────

    /// Spin up a real MCP server on a random port and return (base_url, state).
    async fn start_test_server(registry: Registry) -> (String, McpServerState) {
        start_test_server_with_allow_list(registry, AllowList::Any).await
    }

    async fn start_test_server_with_allow_list(
        registry: Registry,
        allow_list: AllowList,
    ) -> (String, McpServerState) {
        // Bind with port 0 to get a free port, keep the listener open to avoid races.
        let std_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = std_listener.local_addr().unwrap().port();

        let allow_list = Arc::new(RwLock::new(allow_list));
        let response_map = Arc::new(Mutex::new(HashMap::new()));
        let request_counter = Arc::new(AtomicU64::new(0));
        let registry = Arc::new(RwLock::new(registry));
        let client_sinks = Arc::new(Mutex::new(Vec::new()));
        let server_info = Arc::new(RwLock::new(McpServerInfo::default()));
        let tasks = Arc::new(Mutex::new(HashMap::new()));

        let (state_tx, state_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let runtime = Arc::new(
                tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                    .unwrap(),
            );
            let runtime_keep = runtime.clone(); // keep alive while thread is parked
            let state = start_mcp_server_with_listener(
                runtime,
                std_listener,
                None,
                allow_list,
                response_map,
                request_counter,
                registry,
                Duration::from_millis(200),
                client_sinks,
                server_info,
                Arc::new(Mutex::new(HashMap::new())),
                tasks,
            )
            .unwrap();
            let _ = state_tx.send(state);
            // Park thread to keep runtime_keep (and thus the server) alive.
            let _keep = runtime_keep;
            std::thread::park();
        });
        let state = state_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("server state");

        let base = format!("http://127.0.0.1:{port}");

        // Wait until the server is actually serving HTTP.
        let client = reqwest::Client::new();
        for _ in 0..50 {
            if client.get(format!("{base}/")).send().await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        (base, state)
    }

    fn make_registry_with_tool() -> Registry {
        use rmcp::model::Tool;
        let mut registry = Registry::default();
        let tool: Tool = serde_json::from_value(serde_json::json!({
            "name": "greet",
            "description": "Say hello",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": { "type": "string" }
                },
                "required": ["name"]
            }
        }))
        .unwrap();
        #[cfg(feature = "validate-schema")]
        {
            let schema_value = serde_json::Value::Object(tool.input_schema.as_ref().clone());
            let schema = jsonschema::validator_for(&schema_value).unwrap();
            registry.register_tool(tool, schema);
        }
        #[cfg(not(feature = "validate-schema"))]
        {
            registry.register_tool(tool);
        }
        registry
    }

    fn make_registry_with_required_task_tool() -> Registry {
        use rmcp::model::Tool;
        let mut registry = Registry::default();
        let tool: Tool = serde_json::from_value(serde_json::json!({
            "name": "long_job",
            "description": "Long-running job",
            "inputSchema": {
                "type": "object"
            },
            "execution": {
                "taskSupport": "required"
            }
        }))
        .unwrap();
        #[cfg(feature = "validate-schema")]
        {
            let schema_value = serde_json::Value::Object(tool.input_schema.as_ref().clone());
            let schema = jsonschema::validator_for(&schema_value).unwrap();
            registry.register_tool(tool, schema);
        }
        #[cfg(not(feature = "validate-schema"))]
        {
            registry.register_tool(tool);
        }
        registry
    }

    fn make_test_handler(registry: Registry) -> Arc<McpBridgeHandler> {
        Arc::new(McpBridgeHandler {
            connection: None,
            response_map: Arc::new(Mutex::new(HashMap::new())),
            request_counter: Arc::new(AtomicU64::new(1)),
            registry: Arc::new(RwLock::new(registry)),
            response_timeout: Duration::from_millis(200),
            client_sinks: Arc::new(Mutex::new(Vec::new())),
            server_info: Arc::new(RwLock::new(McpServerInfo::default())),
            subscriptions: Arc::new(Mutex::new(HashMap::new())),
            tasks: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    fn make_registry_with_resource() -> Registry {
        use rmcp::model::Resource;
        let mut registry = Registry::default();
        let resource: Resource = serde_json::from_value(serde_json::json!({
            "uri": "str://hello",
            "name": "hello"
        }))
        .unwrap();
        registry.register_resource(resource);
        registry
    }

    fn make_registry_with_prompt() -> Registry {
        use rmcp::model::Prompt;
        let mut registry = Registry::default();
        let prompt: Prompt = serde_json::from_value(serde_json::json!({
            "name": "summarize",
            "description": "Summarize text"
        }))
        .unwrap();
        registry.register_prompt(prompt);
        registry
    }

    // ── parse_allow_list ─────────────────────────────────────────────────────

    #[test]
    fn parse_allow_list_empty_returns_default_local() {
        let al = parse_allow_list("").unwrap();
        assert!(matches!(al, AllowList::List(_)));
        assert!(al.allows("http://localhost"));
        assert!(al.allows("http://127.0.0.1"));
        assert!(!al.allows("http://evil.com"));
    }

    #[test]
    fn parse_allow_list_star_returns_any() {
        let al = parse_allow_list("*").unwrap();
        assert!(matches!(al, AllowList::Any));
        assert!(al.allows("http://anything.com"));
    }

    #[test]
    fn parse_allow_list_json_array() {
        let al = parse_allow_list(r#"["http://example.com","http://other.com"]"#).unwrap();
        assert!(al.allows("http://example.com"));
        assert!(al.allows("http://other.com"));
        assert!(!al.allows("http://evil.com"));
    }

    #[test]
    fn parse_allow_list_json_array_with_star_returns_any() {
        let al = parse_allow_list(r#"["http://example.com","*"]"#).unwrap();
        assert!(matches!(al, AllowList::Any));
    }

    // ── parse_mcp_response ───────────────────────────────────────────────────

    #[test]
    fn parse_mcp_response_extracts_result_field() {
        let resp = McpResponse {
            status: 200,
            headers: HashMap::new(),
            body: r#"{"result":{"tools":[]}}"#.to_owned(),
        };
        let val: serde_json::Value = parse_mcp_response(resp).unwrap();
        assert_eq!(val, serde_json::json!({"tools": []}));
    }

    #[test]
    fn parse_mcp_response_returns_error_on_error_field() {
        let resp = McpResponse {
            status: 200,
            headers: HashMap::new(),
            body: r#"{"error":{"code":-32600,"message":"bad request"}}"#.to_owned(),
        };
        let result: Result<serde_json::Value, _> = parse_mcp_response(resp);
        assert!(result.is_err());
        assert!(result.unwrap_err().message.contains("bad request"));
    }

    #[test]
    fn parse_mcp_response_error_on_empty_body() {
        let resp = McpResponse {
            status: 200,
            headers: HashMap::new(),
            body: "   ".to_owned(),
        };
        let result: Result<serde_json::Value, _> = parse_mcp_response(resp);
        assert!(result.is_err());
    }

    #[test]
    fn parse_mcp_response_http_error_status() {
        let resp = McpResponse {
            status: 500,
            headers: HashMap::new(),
            body: r#"{"something":"value"}"#.to_owned(),
        };
        let result: Result<serde_json::Value, _> = parse_mcp_response(resp);
        assert!(result.is_err());
        assert!(result.unwrap_err().message.contains("500"));
    }

    #[tokio::test]
    async fn complete_task_stores_terminal_result() {
        let handler = make_test_handler(Registry::default());
        let task = Task::new(
            "task-1".to_owned(),
            TaskStatus::Working,
            "2026-01-01T00:00:00Z".to_owned(),
            "2026-01-01T00:00:00Z".to_owned(),
        );
        handler
            .tasks
            .lock()
            .await
            .insert("task-1".to_owned(), TaskEntry::new(task, None));

        handler
            .complete_task(
                "task-1",
                McpResponse {
                    status: 200,
                    headers: HashMap::new(),
                    body: r#"{"result":{"content":[{"type":"text","text":"done"}]}}"#.to_owned(),
                },
            )
            .await
            .unwrap();

        let tasks = handler.tasks.lock().await;
        let entry = tasks.get("task-1").unwrap();
        assert_eq!(entry.task.status, TaskStatus::Completed);
        assert_eq!(
            entry.result,
            Some(Ok(serde_json::json!({
                "content": [{"type":"text","text":"done"}]
            })))
        );
    }

    #[tokio::test]
    async fn update_task_status_changes_state() {
        let handler = make_test_handler(Registry::default());
        let task = Task::new(
            "task-2".to_owned(),
            TaskStatus::Working,
            "2026-01-01T00:00:00Z".to_owned(),
            "2026-01-01T00:00:00Z".to_owned(),
        );
        handler
            .tasks
            .lock()
            .await
            .insert("task-2".to_owned(), TaskEntry::new(task, None));

        handler
            .update_task_status(
                "task-2",
                TaskStatus::InputRequired,
                Some("Need input".to_owned()),
            )
            .await
            .unwrap();

        let tasks = handler.tasks.lock().await;
        let entry = tasks.get("task-2").unwrap();
        assert_eq!(entry.task.status, TaskStatus::InputRequired);
        assert_eq!(entry.task.status_message.as_deref(), Some("Need input"));
    }

    // ── normalize_protocol_version ───────────────────────────────────────────

    #[test]
    fn normalize_replaces_future_version() {
        let mut req = Request::builder()
            .header("mcp-protocol-version", "2025-11-25")
            .body(())
            .unwrap();
        normalize_protocol_version(&mut req);
        assert_eq!(
            req.headers().get("mcp-protocol-version").unwrap(),
            "2025-06-18"
        );
    }

    #[test]
    fn normalize_removes_unknown_version() {
        let mut req = Request::builder()
            .header("mcp-protocol-version", "1999-01-01")
            .body(())
            .unwrap();
        normalize_protocol_version(&mut req);
        assert!(req.headers().get("mcp-protocol-version").is_none());
    }

    #[test]
    fn normalize_keeps_known_version() {
        for version in ["2024-11-05", "2025-03-26", "2025-06-18"] {
            let mut req = Request::builder()
                .header("mcp-protocol-version", version)
                .body(())
                .unwrap();
            normalize_protocol_version(&mut req);
            assert_eq!(
                req.headers().get("mcp-protocol-version").unwrap(),
                version,
                "version {version} should be kept"
            );
        }
    }

    // ── HTTP integration tests ───────────────────────────────────────────────

    #[tokio::test]
    async fn get_root_returns_ok() {
        let (base, _state) = start_test_server(Registry::default()).await;
        let resp = reqwest::get(format!("{base}/")).await.unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.text().await.unwrap(), "MCP server");
    }

    #[tokio::test]
    async fn cors_forbidden_for_disallowed_origin() {
        let (base, _state) =
            start_test_server_with_allow_list(Registry::default(), AllowList::default_local())
                .await;

        let client = reqwest::Client::new();
        let resp = client
            .post(format!("{base}/mcp"))
            .header("origin", "http://evil.com")
            .header("content-type", "application/json")
            .body("{}")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 403);
    }

    #[tokio::test]
    async fn cors_options_preflight_returns_204() {
        let (base, _state) = start_test_server(Registry::default()).await;
        let client = reqwest::Client::new();
        let resp = client
            .request(reqwest::Method::OPTIONS, format!("{base}/mcp"))
            .header("origin", "http://example.com")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 204);
        assert!(resp.headers().contains_key("access-control-allow-origin"));
    }

    #[tokio::test]
    async fn orphan_initialized_notification_returns_202() {
        let (base, _state) = start_test_server(Registry::default()).await;
        let client = reqwest::Client::new();
        let url = format!("{base}/mcp");

        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        });
        // No mcp-session-id header → intercepted
        let resp = client
            .post(&url)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 202);
    }

    // ── rmcp client integration tests ───────────────────────────────────────

    mod rmcp_client {
        use super::*;
        use rmcp::service::ServiceExt;
        use rmcp::transport::StreamableHttpClientTransport;

        async fn connect(base: &str) -> rmcp::service::RunningService<rmcp::service::RoleClient, ()> {
            let url = format!("{base}/mcp");
            let transport = StreamableHttpClientTransport::from_uri(url);
            ().serve(transport).await.expect("rmcp client should connect")
        }

        #[tokio::test]
        async fn rmcp_client_initialize_and_get_server_info() {
            let (base, _state) = start_test_server(Registry::default()).await;
            let client = connect(&base).await;
            let info = client.peer().peer_info();
            assert!(info.is_some(), "server info should be available after init");
            client.cancel().await.unwrap();
        }

        #[tokio::test]
        async fn rmcp_client_list_tools_empty() {
            let (base, _state) = start_test_server(Registry::default()).await;
            let client = connect(&base).await;
            let result = client.list_tools(None).await.unwrap();
            assert!(result.tools.is_empty());
            client.cancel().await.unwrap();
        }

        #[tokio::test]
        async fn rmcp_client_list_tools_with_registered_tool() {
            let registry = make_registry_with_tool();
            let (base, _state) = start_test_server(registry).await;
            let client = connect(&base).await;
            let result = client.list_tools(None).await.unwrap();
            assert_eq!(result.tools.len(), 1);
            assert_eq!(result.tools[0].name.as_ref(), "greet");
            client.cancel().await.unwrap();
        }

        #[tokio::test]
        async fn rmcp_client_list_tools_preserves_task_support_metadata() {
            let registry = make_registry_with_required_task_tool();
            let (base, _state) = start_test_server(registry).await;
            let client = connect(&base).await;
            let result = client.list_tools(None).await.unwrap();
            assert_eq!(result.tools.len(), 1);
            assert_eq!(
                result.tools[0]
                    .execution
                    .as_ref()
                    .and_then(|execution| execution.task_support),
                Some(rmcp::model::TaskSupport::Required)
            );
            client.cancel().await.unwrap();
        }

        #[tokio::test]
        async fn rmcp_client_call_tool_unknown() {
            let (base, _state) = start_test_server(Registry::default()).await;
            let client = connect(&base).await;
            let params = serde_json::from_value(serde_json::json!({
                "name": "nonexistent"
            })).unwrap();
            let err = client.call_tool(params).await;
            assert!(err.is_err(), "calling unknown tool should fail");
            client.cancel().await.unwrap();
        }

        #[tokio::test]
        async fn rmcp_client_call_tool_invalid_args() {
            let registry = make_registry_with_tool();
            let (base, _state) = start_test_server(registry).await;
            let client = connect(&base).await;
            let params = serde_json::from_value(serde_json::json!({
                "name": "greet",
                "arguments": {}
            })).unwrap();
            let err = client.call_tool(params).await;
            assert!(err.is_err(), "calling tool with invalid args should fail");
            client.cancel().await.unwrap();
        }

        #[tokio::test]
        async fn rmcp_client_call_tool_no_connection() {
            let registry = make_registry_with_tool();
            let (base, _state) = start_test_server(registry).await;
            let client = connect(&base).await;
            let params = serde_json::from_value(serde_json::json!({
                "name": "greet",
                "arguments": { "name": "world" }
            })).unwrap();
            let err = client.call_tool(params).await;
            assert!(err.is_err(), "calling tool without 1C connection should fail");
            client.cancel().await.unwrap();
        }

        #[tokio::test]
        async fn rmcp_client_list_resources() {
            let registry = make_registry_with_resource();
            let (base, _state) = start_test_server(registry).await;
            let client = connect(&base).await;
            let result = client.list_resources(None).await.unwrap();
            assert_eq!(result.resources.len(), 1);
            assert_eq!(result.resources[0].uri.as_str(), "str://hello");
            client.cancel().await.unwrap();
        }

        #[tokio::test]
        async fn rmcp_client_read_resource_not_found() {
            let (base, _state) = start_test_server(Registry::default()).await;
            let client = connect(&base).await;
            let params = serde_json::from_value(serde_json::json!({
                "uri": "str://missing"
            })).unwrap();
            let err = client.read_resource(params).await;
            assert!(err.is_err(), "reading missing resource should fail");
            client.cancel().await.unwrap();
        }

        #[tokio::test]
        async fn rmcp_client_list_resource_templates() {
            use rmcp::model::ResourceTemplate;
            let mut registry = Registry::default();
            let tmpl: ResourceTemplate = serde_json::from_value(serde_json::json!({
                "uriTemplate": "str://users/{id}",
                "name": "user"
            }))
            .unwrap();
            registry.register_resource_template(tmpl).unwrap();

            let (base, _state) = start_test_server(registry).await;
            let client = connect(&base).await;
            let result = client.list_resource_templates(None).await.unwrap();
            assert_eq!(result.resource_templates.len(), 1);
            assert_eq!(result.resource_templates[0].uri_template.as_str(), "str://users/{id}");
            client.cancel().await.unwrap();
        }

        #[tokio::test]
        async fn rmcp_client_list_prompts() {
            let registry = make_registry_with_prompt();
            let (base, _state) = start_test_server(registry).await;
            let client = connect(&base).await;
            let result = client.list_prompts(None).await.unwrap();
            assert_eq!(result.prompts.len(), 1);
            assert_eq!(result.prompts[0].name.as_str(), "summarize");
            client.cancel().await.unwrap();
        }

        #[tokio::test]
        async fn rmcp_client_get_prompt_not_found() {
            let (base, _state) = start_test_server(Registry::default()).await;
            let client = connect(&base).await;
            let params = serde_json::from_value(serde_json::json!({
                "name": "nonexistent"
            })).unwrap();
            let err = client.get_prompt(params).await;
            assert!(err.is_err(), "getting missing prompt should fail");
            client.cancel().await.unwrap();
        }

        #[tokio::test]
        async fn rmcp_client_multiple_sessions_independent() {
            let registry = make_registry_with_tool();
            let (base, _state) = start_test_server(registry).await;

            let client1 = connect(&base).await;
            let client2 = connect(&base).await;

            let r1 = client1.list_tools(None).await.unwrap();
            let r2 = client2.list_tools(None).await.unwrap();
            assert_eq!(r1.tools.len(), 1);
            assert_eq!(r2.tools.len(), 1);

            client1.cancel().await.unwrap();
            // client2 should still work after client1 disconnects
            let r3 = client2.list_tools(None).await.unwrap();
            assert_eq!(r3.tools.len(), 1);
            client2.cancel().await.unwrap();
        }
    }
}
