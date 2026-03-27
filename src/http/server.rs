use super::{mcp_handler, HttpAddIn};
use crate::parse_headers;
use addin1c::{name, AddinResult, CString1C, Variant};
use axum::body::{to_bytes, Body};
use axum::extract::State;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, Request, Response, StatusCode};
use axum::routing::{get, post};
use axum::Router;
use bytes::Bytes;
use futures_util::stream;
use std::collections::HashMap;
use std::error::Error;
use std::net::SocketAddr;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, Mutex};

const HTTP_RESPONSE_TIMEOUT_SECS: u64 = 30;

pub(super) struct HttpServerState {
    pub(super) shutdown: oneshot::Sender<()>,
    pub(super) _join: tokio::task::JoinHandle<()>,
    response_map: Arc<Mutex<HashMap<String, oneshot::Sender<HttpResponse>>>>,
}

#[derive(Clone)]
struct HttpAppState {
    response_map: Arc<Mutex<HashMap<String, oneshot::Sender<HttpResponse>>>>,
    counter: Arc<AtomicU64>,
    connection: Option<&'static addin1c::Connection>,
    sse_sessions: Arc<Mutex<HashMap<String, mpsc::UnboundedSender<String>>>>,
    sse_session_counter: Arc<AtomicU64>,
}

#[derive(Debug)]
struct HttpIncomingRequest {
    id: String,
    method: String,
    path: String,
    query: String,
    headers: HashMap<String, String>,
    body: String,
}

impl HttpIncomingRequest {
    fn to_json(&self) -> String {
        serde_json::json!({
            "id": self.id,
            "method": self.method,
            "path": self.path,
            "query": self.query,
            "headers": self.headers,
            "body": self.body,
        })
        .to_string()
    }
}

#[derive(Debug)]
struct HttpResponse {
    status: u16,
    headers: HashMap<String, String>,
    body: String,
}

impl HttpAddIn {
    pub(super) fn http_start(
        &mut self,
        address: &mut Variant,
        return_value: &mut Variant,
    ) -> AddinResult {
        if self.http_server.is_some() {
            return Err("HTTP сервер уже запущен".to_owned().into());
        }

        let address = address.get_string()?;
        let addr: SocketAddr = address
            .parse()
            .map_err(|err| format!("Некорректный адрес: {err}"))?;
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let response_map = Arc::new(Mutex::new(HashMap::new()));
        let state = HttpAppState {
            response_map: response_map.clone(),
            counter: self.http_request_counter.clone(),
            connection: self.connection,
            sse_sessions: self.sse_sessions.clone(),
            sse_session_counter: self.sse_session_counter.clone(),
        };

        let listener = self
            .runtime
            .block_on(async { tokio::net::TcpListener::bind(addr).await })
            .map_err(|err| format!("Не удалось запустить HTTP сервер: {err}"))?;

        let join = self.runtime.spawn(async move {
            let app = Router::new()
                .route("/sse", get(handle_sse_request))
                .route("/message", post(handle_mcp_route))
                .route("/", get(handle_root))
                .fallback(handle_http_request)
                .with_state(state);

            let server = axum::serve(listener, app).with_graceful_shutdown(async {
                let _ = shutdown_rx.await;
            });

            let _ = server.await;
        });

        self.http_server = Some(HttpServerState {
            shutdown: shutdown_tx,
            _join: join,
            response_map,
        });

        return_value.set_bool(true);
        Ok(())
    }

    pub(super) fn http_stop(&mut self, return_value: &mut Variant) -> AddinResult {
        let Some(server) = self.http_server.take() else {
            return Err("HTTP сервер не запущен".to_owned().into());
        };

        let response_map = server.response_map.clone();
        let sse_sessions = self.sse_sessions.clone();
        self.runtime.clone().block_on(async {
            let mut map = response_map.lock().await;
            map.clear();
            let mut sse_map = sse_sessions.lock().await;
            sse_map.clear();
        });

        let _ = server.shutdown.send(());
        return_value.set_bool(true);
        Ok(())
    }

    pub(super) fn http_send_response(
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
        let response = HttpResponse {
            status: status_code as u16,
            headers,
            body,
        };
        let Some(server) = self.http_server.as_ref() else {
            return Err("HTTP сервер не запущен".to_owned().into());
        };

        self.runtime.clone().block_on(async {
            let mut map = server.response_map.lock().await;
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

    pub(super) fn sse_send(
        &mut self,
        session_id: &mut Variant,
        data: &mut Variant,
        return_value: &mut Variant,
    ) -> AddinResult {
        let session_id = session_id.get_string()?;
        let data = data.get_string()?;
        let sse_sessions = self.sse_sessions.clone();
        self.runtime.clone().block_on(async {
            let map = sse_sessions.lock().await;
            let sender = map
                .get(session_id.as_str())
                .ok_or_else(|| "SSE сессия не найдена".to_owned())?;
            let payload = sse_format_message_event(data.as_str());
            sender.send(payload).map_err(|_| -> Box<dyn Error> {
                "Не удалось отправить SSE событие".to_owned().into()
            })?;
            return_value.set_bool(true);
            Ok(())
        })
    }

    pub(super) fn sse_close(
        &mut self,
        session_id: &mut Variant,
        return_value: &mut Variant,
    ) -> AddinResult {
        let session_id = session_id.get_string()?;
        let sse_sessions = self.sse_sessions.clone();
        self.runtime.clone().block_on(async {
            let mut map = sse_sessions.lock().await;
            map.remove(session_id.as_str());
            return_value.set_bool(true);
            Ok(())
        })
    }
}

async fn handle_root() -> Response<Body> {
    let mut response = Response::builder()
        .status(StatusCode::OK)
        .body(Body::from("MCP server"))
        .unwrap();
    add_cors_headers(response.headers_mut());
    response
}

async fn handle_mcp_route(State(state): State<HttpAppState>, req: Request<Body>) -> Response<Body> {
    let mut response = match mcp_handler::handle_mcp_message(req, state.connection).await {
        Ok(response) => response,
        Err(err) => match err {},
    };
    add_cors_headers(response.headers_mut());
    response
}

async fn handle_http_request(
    State(state): State<HttpAppState>,
    req: Request<Body>,
) -> Response<Body> {
    if req.method() == Method::OPTIONS {
        return cors_preflight_response();
    }

    let (parts, body) = req.into_parts();
    let body_bytes = match to_bytes(body, 16 * 1024 * 1024).await {
        Ok(bytes) => bytes,
        Err(_) => {
            let mut response = Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(Body::from("Failed to read request body"))
                .unwrap();
            add_cors_headers(response.headers_mut());
            return response;
        }
    };

    let id = state.counter.fetch_add(1, Ordering::Relaxed).to_string();
    let headers = parts
        .headers
        .iter()
        .map(|(key, value)| {
            let value = value.to_str().unwrap_or_default().to_owned();
            (key.to_string(), value)
        })
        .collect::<HashMap<_, _>>();

    let request = HttpIncomingRequest {
        id: id.clone(),
        method: parts.method.as_str().to_owned(),
        path: parts.uri.path().to_owned(),
        query: parts.uri.query().unwrap_or("").to_owned(),
        headers,
        body: String::from_utf8_lossy(&body_bytes).to_string(),
    };

    let (response_tx, response_rx) = oneshot::channel();
    {
        let mut map = state.response_map.lock().await;
        map.insert(id.clone(), response_tx);
    }

    let Some(connection) = state.connection else {
        let mut map = state.response_map.lock().await;
        map.remove(&id);
        let mut response = Response::builder()
            .status(StatusCode::SERVICE_UNAVAILABLE)
            .body(Body::from("Event connection is unavailable"))
            .unwrap();
        add_cors_headers(response.headers_mut());
        return response;
    };

    let data = CString1C::from(request.to_json().as_str());
    if !connection.external_event(name!("WebTransport"), name!("HTTP"), data) {
        let mut map = state.response_map.lock().await;
        map.remove(&id);
        let mut response = Response::builder()
            .status(StatusCode::SERVICE_UNAVAILABLE)
            .body(Body::from("Event queue is full"))
            .unwrap();
        add_cors_headers(response.headers_mut());
        return response;
    }

    match tokio::time::timeout(Duration::from_secs(HTTP_RESPONSE_TIMEOUT_SECS), response_rx).await {
        Ok(Ok(response)) => {
            let mut builder = Response::builder()
                .status(StatusCode::from_u16(response.status).unwrap_or(StatusCode::OK));
            if response
                .headers
                .keys()
                .all(|key| key.to_ascii_lowercase() != "content-type")
            {
                builder = builder.header("Content-Type", "application/json; charset=utf-8");
            }
            for (key, value) in response.headers {
                let name = HeaderName::from_bytes(key.as_bytes());
                let value = HeaderValue::from_str(value.as_str());
                if let (Ok(name), Ok(value)) = (name, value) {
                    builder = builder.header(name, value);
                }
            }
            let mut resp = builder.body(Body::from(response.body)).unwrap();
            add_cors_headers(resp.headers_mut());
            resp
        }
        Ok(Err(_)) => {
            let mut response = Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(Body::from("Response channel closed"))
                .unwrap();
            add_cors_headers(response.headers_mut());
            response
        }
        Err(_) => {
            let mut map = state.response_map.lock().await;
            map.remove(&id);
            let mut response = Response::builder()
                .status(StatusCode::GATEWAY_TIMEOUT)
                .body(Body::from("Handler timeout"))
                .unwrap();
            add_cors_headers(response.headers_mut());
            response
        }
    }
}

async fn handle_sse_request(
    State(state): State<HttpAppState>,
    req: Request<Body>,
) -> Response<Body> {
    let session_id = match get_query_param(req.uri().query().unwrap_or(""), "sessionId") {
        Some(value) => value,
        None => state
            .sse_session_counter
            .fetch_add(1, Ordering::Relaxed)
            .to_string(),
    };
    let (tx, rx) = mpsc::unbounded_channel::<String>();

    {
        let mut map = state.sse_sessions.lock().await;
        map.insert(session_id.clone(), tx.clone());
    }

    let host = req
        .headers()
        .get("host")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("127.0.0.1");
    let endpoint = format!("http://{host}/message?sessionId={session_id}");
    let initial = sse_format_event("endpoint", endpoint.as_str());
    let _ = tx.send(initial);

    if let Some(connection) = state.connection {
        let data = CString1C::from(
            serde_json::json!({
                "id": session_id,
                "path": "/sse",
                "headers": {},
            })
            .to_string()
            .as_str(),
        );
        let _ = connection.external_event(name!("WebTransport"), name!("SSE_OPEN"), data);
    }

    let stream = stream::unfold(rx, |mut rx| async {
        match rx.recv().await {
            Some(item) => Some((Ok::<Bytes, std::io::Error>(Bytes::from(item)), rx)),
            None => None,
        }
    });

    let mut response = Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "text/event-stream")
        .header("Cache-Control", "no-cache")
        .header("Connection", "keep-alive")
        .header("X-Accel-Buffering", "no")
        .body(Body::from_stream(stream))
        .unwrap();
    add_cors_headers(response.headers_mut());
    response
}

fn sse_format_event(event: &str, data: &str) -> String {
    let mut out = String::new();
    out.push_str("event: ");
    out.push_str(event);
    out.push('\n');
    if data.is_empty() {
        out.push_str("data:\n");
    } else {
        for line in data.lines() {
            out.push_str("data: ");
            out.push_str(line);
            out.push('\n');
        }
    }
    out.push('\n');
    out
}

fn sse_format_message_event(data: &str) -> String {
    sse_format_event("message", data)
}

fn cors_preflight_response() -> Response<Body> {
    let mut response = Response::builder()
        .status(StatusCode::NO_CONTENT)
        .body(Body::empty())
        .unwrap();
    add_cors_headers(response.headers_mut());
    response
}

fn add_cors_headers(headers: &mut HeaderMap) {
    headers.insert("Access-Control-Allow-Origin", HeaderValue::from_static("*"));
    headers.insert(
        "Access-Control-Allow-Methods",
        HeaderValue::from_static("GET, POST, OPTIONS"),
    );
    headers.insert(
        "Access-Control-Allow-Headers",
        HeaderValue::from_static("content-type, authorization"),
    );
    headers.insert("Access-Control-Max-Age", HeaderValue::from_static("86400"));
}

fn get_query_param(query: &str, key: &str) -> Option<String> {
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let mut it = pair.splitn(2, '=');
        let k = it.next().unwrap_or("");
        if k == key {
            return Some(it.next().unwrap_or("").to_owned());
        }
    }
    None
}
