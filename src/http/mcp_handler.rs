use addin1c::{name, CString1C};
use hyper::body::to_bytes;
use hyper::{Body, Request, Response, StatusCode};
use std::collections::HashMap;
use std::convert::Infallible;

pub(super) async fn handle_mcp_message(
    req: Request<Body>,
    connection: Option<&'static addin1c::Connection>,
) -> Result<Response<Body>, Infallible> {
    let (parts, body) = req.into_parts();
    let body_bytes = match to_bytes(body).await {
        Ok(bytes) => bytes,
        Err(_) => {
            return Ok(Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(Body::from("Failed to read request body"))
                .unwrap())
        }
    };

    let headers = parts
        .headers
        .iter()
        .map(|(key, value)| {
            let value = value.to_str().unwrap_or_default().to_owned();
            (key.to_string(), value)
        })
        .collect::<HashMap<_, _>>();

    let request_json = serde_json::json!({
        "id": "mcp",
        "method": parts.method.as_str(),
        "path": parts.uri.path(),
        "query": parts.uri.query().unwrap_or(""),
        "headers": headers,
        "body": String::from_utf8_lossy(&body_bytes).to_string(),
    })
    .to_string();

    let Some(connection) = connection else {
        return Ok(Response::builder()
            .status(StatusCode::SERVICE_UNAVAILABLE)
            .body(Body::from("Event connection is unavailable"))
            .unwrap());
    };

    let data = CString1C::from(request_json.as_str());
    if !connection.external_event(name!("WebTransport"), name!("MCP_MESSAGE"), data) {
        return Ok(Response::builder()
            .status(StatusCode::SERVICE_UNAVAILABLE)
            .body(Body::from("Event queue is full"))
            .unwrap());
    }

    Ok(Response::builder()
        .status(StatusCode::ACCEPTED)
        .body(Body::empty())
        .unwrap())
}
