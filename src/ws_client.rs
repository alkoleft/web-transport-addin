use addin1c::{AddinResult, Variant};
use futures_util::{
    stream::{SplitSink, SplitStream},
    SinkExt, StreamExt,
};
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::runtime::Runtime;
use tokio::net::TcpStream;
use tokio_tungstenite::{
    connect_async,
    tungstenite::http::{Request as WsRequest, Uri},
    MaybeTlsStream, WebSocketStream,
};

pub(crate) struct WebSocketConnection {
    pub(super) sender: SplitSink<
        WebSocketStream<MaybeTlsStream<TcpStream>>,
        tokio_tungstenite::tungstenite::Message,
    >,
    pub(super) receiver: SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>,
}

#[derive(Default)]
struct RequestData {
    address: String,
    headers: HashMap<String, String>,
}

impl RequestData {
    fn try_new(address: String, json_headers: String) -> Result<Self, String> {
        let headers = if !json_headers.is_empty() {
            serde_json::from_str::<HashMap<String, serde_json::Value>>(&json_headers)
                .map_err(|error| error.to_string())?
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
                .collect()
        } else {
            HashMap::default()
        };

        Ok(RequestData { address, headers })
    }
}

impl TryFrom<RequestData> for WsRequest<()> {
    type Error = String;

    fn try_from(data: RequestData) -> Result<Self, Self::Error> {
        let uri = data
            .address
            .parse::<Uri>()
            .map_err(|err| err.to_string())?;
        let authority = uri
            .authority()
            .ok_or("No host name in the URL".to_string())?
            .as_str();
        let host = authority
            .find('@')
            .map(|idx| authority.split_at(idx + 1).1)
            .unwrap_or_else(|| authority);

        let websocket_key = tokio_tungstenite::tungstenite::handshake::client::generate_key();

        let mut request_builder = WsRequest::get(data.address.as_str())
            .header("Host", host)
            .header("Connection", "Upgrade")
            .header("Upgrade", "websocket")
            .header("Sec-WebSocket-Version", "13")
            .header("Sec-WebSocket-Key", websocket_key);

        for (key, value) in data.headers.iter() {
            request_builder = request_builder.header(key.as_str(), value.as_str());
        }
        request_builder
            .body(())
            .map_err(|error| error.to_string())
    }
}

pub(crate) fn connect(
    runtime: &Arc<Runtime>,
    websocket: &mut Option<WebSocketConnection>,
    address: &mut Variant,
    json_headers: &mut Variant,
    return_value: &mut Variant,
) -> AddinResult {
    runtime.clone().block_on(async {
        let request_data = RequestData::try_new(address.get_string()?, json_headers.get_string()?)?;
        let request = WsRequest::try_from(request_data)?;
        let (stream, _) = connect_async(request)
            .await
            .map_err(|error| format!("{error}"))?;
        let (sender, receiver) = stream.split();
        *websocket = Some(WebSocketConnection { sender, receiver });
        return_value.set_bool(true);
        Ok(())
    })
}

pub(crate) fn send(
    runtime: &Arc<Runtime>,
    websocket: &mut Option<WebSocketConnection>,
    message: &mut Variant,
    return_value: &mut Variant,
) -> AddinResult {
    runtime.clone().block_on(async {
        let message = message.get_string()?;
        match websocket.as_mut() {
            None => Err("Отсутствует установленное соединение!".to_owned().into()),
            Some(websocket) => {
                websocket
                    .sender
                    .send(tokio_tungstenite::tungstenite::Message::Text(message.into()))
                    .await?;
                return_value.set_bool(true);
                Ok(())
            }
        }
    })
}

pub(crate) fn receive(
    runtime: &Arc<Runtime>,
    websocket: &mut Option<WebSocketConnection>,
    timeout: &mut Variant,
    return_value: &mut Variant,
) -> AddinResult {
    runtime.clone().block_on(async {
        match websocket.as_mut() {
            None => Err("Отсутствует установленное соединение!".to_owned().into()),
            Some(websocket) => {
                let timeout = timeout.get_i32()?;
                match tokio::time::timeout(
                    Duration::from_millis(timeout as u64),
                    websocket.receiver.next(),
                )
                .await
                {
                    Err(_) | Ok(None) => {
                        return_value.set_str1c("")?;
                        Ok(())
                    }
                    Ok(Some(result)) => {
                        let message = result?.to_text()?.to_owned();
                        return_value.set_str1c(message)?;
                        Ok(())
                    }
                }
            }
        }
    })
}

pub(crate) fn disconnect(
    websocket: &mut Option<WebSocketConnection>,
    return_value: &mut Variant,
) -> AddinResult {
    *websocket = None;
    return_value.set_bool(true);
    Ok(())
}
