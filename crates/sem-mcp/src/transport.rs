use std::{future::Future, sync::Arc};

use rmcp::model::{ClientJsonRpcMessage, ClientRequest, JsonRpcMessage, ServerJsonRpcMessage};
use rmcp::transport::Transport;
use rmcp::RoleServer;
use serde::Serialize;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;

const PARSE_ERROR_RESPONSE: &[u8] =
    br#"{"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"Parse error"}}"#;

pub(crate) struct ResilientStdioTransport<R, W> {
    read: BufReader<R>,
    write: Arc<Mutex<Option<W>>>,
}

impl<R, W> ResilientStdioTransport<R, W>
where
    R: AsyncRead + Send + Unpin + 'static,
    W: AsyncWrite + Send + Unpin + 'static,
{
    pub(crate) fn new(read: R, write: W) -> Self {
        Self {
            read: BufReader::new(read),
            write: Arc::new(Mutex::new(Some(write))),
        }
    }
}

impl<R, W> Transport<RoleServer> for ResilientStdioTransport<R, W>
where
    R: AsyncRead + Send + Unpin + 'static,
    W: AsyncWrite + Send + Unpin + 'static,
{
    type Error = std::io::Error;

    fn send(
        &mut self,
        item: ServerJsonRpcMessage,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        let write = Arc::clone(&self.write);
        async move { write_json_line(&write, &item).await }
    }

    async fn receive(&mut self) -> Option<ClientJsonRpcMessage> {
        let mut line = Vec::new();

        loop {
            line.clear();

            match self.read.read_until(b'\n', &mut line).await {
                Ok(0) => return None,
                Ok(_) => {}
                Err(error) => {
                    tracing::error!("Error reading from stream: {}", error);
                    return None;
                }
            }

            let line = without_line_ending(&line);
            match parse_client_message(line) {
                IncomingLine::Message(message) => return Some(*message),
                IncomingLine::Ignore => {}
                IncomingLine::MethodNotFound { id } => {
                    if let Err(error) = write_json_line(
                        &self.write,
                        &serde_json::json!({
                            "jsonrpc": "2.0", "id": id,
                            "error": {"code": -32601, "message": "Method not found"},
                        }),
                    )
                    .await
                    {
                        tracing::error!("Error writing method-not-found response: {}", error);
                        return None;
                    }
                }
                IncomingLine::ParseError => {
                    tracing::debug!("Malformed JSON-RPC frame received");
                    let write = Arc::clone(&self.write);
                    if let Err(error) = write_raw_line(&write, PARSE_ERROR_RESPONSE).await {
                        tracing::error!("Error writing parse error response: {}", error);
                        return None;
                    }
                }
                IncomingLine::InvalidRequest { id } => {
                    tracing::debug!("Invalid JSON-RPC request received");
                    let write = Arc::clone(&self.write);
                    if let Err(error) = write_invalid_request(&write, id).await {
                        tracing::error!("Error writing invalid request response: {}", error);
                        return None;
                    }
                }
            }
        }
    }

    async fn close(&mut self) -> Result<(), Self::Error> {
        let mut write = self.write.lock().await;
        let Some(writer) = write.as_mut() else {
            return Ok(());
        };

        let result = writer.shutdown().await;
        let closed_writer = write.take();
        drop(closed_writer);

        result
    }
}

enum IncomingLine {
    Message(Box<ClientJsonRpcMessage>),
    Ignore,
    ParseError,
    InvalidRequest { id: serde_json::Value },
    MethodNotFound { id: serde_json::Value },
}

fn parse_client_message(line: &[u8]) -> IncomingLine {
    match serde_json::from_slice::<ClientJsonRpcMessage>(line) {
        Ok(JsonRpcMessage::Request(request))
            if matches!(request.request, ClientRequest::CustomRequest(_)) =>
        {
            // Newer clients probe server/discover before legacy initialize.
            // Passing unknown requests to rmcp's handshake closes the session;
            // reject them at the transport so the client can negotiate/fallback.
            IncomingLine::MethodNotFound {
                id: serde_json::to_value(request.id).expect("request IDs are JSON values"),
            }
        }
        Ok(message) => IncomingLine::Message(Box::new(message)),
        Err(error) => {
            let Ok(value) = serde_json::from_slice::<serde_json::Value>(line) else {
                return IncomingLine::ParseError;
            };

            if should_ignore_notification(&value) {
                return IncomingLine::Ignore;
            }

            tracing::debug!("Failed to decode JSON-RPC message: {}", error);
            IncomingLine::InvalidRequest {
                id: request_id_or_null(&value),
            }
        }
    }
}

fn request_id_or_null(value: &serde_json::Value) -> serde_json::Value {
    match value.get("id") {
        Some(serde_json::Value::Number(number)) => serde_json::Value::Number(number.clone()),
        Some(serde_json::Value::String(string)) => serde_json::Value::String(string.clone()),
        _ => serde_json::Value::Null,
    }
}

fn should_ignore_notification(value: &serde_json::Value) -> bool {
    let Some(method) = value.get("method").and_then(serde_json::Value::as_str) else {
        return false;
    };
    let is_notification = value.get("id").is_none();

    // Preserve rmcp's stdio compatibility behavior for noisy non-MCP notifications.
    is_notification && !is_standard_method(method)
}

fn is_standard_method(method: &str) -> bool {
    matches!(
        method,
        "initialize"
            | "ping"
            | "prompts/get"
            | "prompts/list"
            | "resources/list"
            | "resources/read"
            | "resources/subscribe"
            | "resources/unsubscribe"
            | "resources/templates/list"
            | "tools/call"
            | "tools/list"
            | "completion/complete"
            | "logging/setLevel"
            | "roots/list"
            | "sampling/createMessage"
    ) || is_standard_notification(method)
}

fn is_standard_notification(method: &str) -> bool {
    matches!(
        method,
        "notifications/cancelled"
            | "notifications/initialized"
            | "notifications/message"
            | "notifications/progress"
            | "notifications/prompts/list_changed"
            | "notifications/resources/list_changed"
            | "notifications/resources/updated"
            | "notifications/roots/list_changed"
            | "notifications/tools/list_changed"
    )
}

fn without_line_ending(line: &[u8]) -> &[u8] {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    line.strip_suffix(b"\r").unwrap_or(line)
}

async fn write_json_line<W, T>(
    write: &Arc<Mutex<Option<W>>>,
    item: &T,
) -> Result<(), std::io::Error>
where
    W: AsyncWrite + Unpin,
    T: Serialize + ?Sized,
{
    let bytes = serde_json::to_vec(item)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    write_raw_line(write, &bytes).await
}

async fn write_invalid_request<W>(
    write: &Arc<Mutex<Option<W>>>,
    id: serde_json::Value,
) -> Result<(), std::io::Error>
where
    W: AsyncWrite + Unpin,
{
    write_json_line(
        write,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {
                "code": -32600,
                "message": "Invalid Request",
            },
        }),
    )
    .await
}

async fn write_raw_line<W>(
    write: &Arc<Mutex<Option<W>>>,
    bytes: &[u8],
) -> Result<(), std::io::Error>
where
    W: AsyncWrite + Unpin,
{
    let mut write = write.lock().await;
    let Some(write) = write.as_mut() else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotConnected,
            "Transport is closed",
        ));
    };

    write.write_all(bytes).await?;
    write.write_all(b"\n").await?;
    write.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::{ClientRequest, JsonRpcMessage, NumberOrString};
    use tokio::io::AsyncReadExt;

    #[tokio::test]
    async fn discovery_probe_replies_method_not_found_then_accepts_initialize() {
        let (mut input, server_input) = tokio::io::duplex(2048);
        let (server_output, output) = tokio::io::duplex(2048);
        let mut transport = ResilientStdioTransport::new(server_input, server_output);
        input.write_all(br#"{"jsonrpc":"2.0","id":"probe","method":"server/discover","params":{}}
{"jsonrpc":"2.0","id":2,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"test","version":"1"}}}
"#).await.unwrap();
        let message = transport.receive().await.unwrap();
        assert!(matches!(message, JsonRpcMessage::Request(request)
            if matches!(request.request, ClientRequest::InitializeRequest(_))));
        let mut line = String::new();
        BufReader::new(output).read_line(&mut line).await.unwrap();
        let response: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(response["id"], "probe");
        assert_eq!(response["error"]["code"], -32601);
    }

    #[tokio::test]
    async fn malformed_json_emits_parse_error_and_keeps_reading() {
        let (mut client_input, server_input) = tokio::io::duplex(1024);
        let (server_output, mut client_output) = tokio::io::duplex(1024);
        let mut transport = ResilientStdioTransport::new(server_input, server_output);

        client_input
            .write_all(
                br#"garbage
{"jsonrpc":"2.0","id":7,"method":"ping"}
"#,
            )
            .await
            .unwrap();

        let message = transport.receive().await.unwrap();

        match message {
            JsonRpcMessage::Request(request) => {
                assert_eq!(request.id, NumberOrString::Number(7));
                assert!(matches!(request.request, ClientRequest::PingRequest(_)));
            }
            other => panic!("expected ping request after malformed frame, got {other:?}"),
        }

        let mut output = vec![0; PARSE_ERROR_RESPONSE.len() + 1];
        client_output.read_exact(&mut output).await.unwrap();
        assert_eq!(output, [PARSE_ERROR_RESPONSE, b"\n"].concat());
    }

    #[test]
    fn invalid_json_rpc_uses_existing_request_id_when_possible() {
        for (line, expected) in [
            (
                br#"{"jsonrpc":"2.0","id":"req-1"}"#.as_slice(),
                serde_json::json!("req-1"),
            ),
            (
                br#"{"jsonrpc":"2.0","id":18446744073709551615}"#.as_slice(),
                serde_json::json!(18446744073709551615_u64),
            ),
            (
                br#"{"jsonrpc":"2.0","id":1.5}"#.as_slice(),
                serde_json::json!(1.5),
            ),
        ] {
            match parse_client_message(line) {
                IncomingLine::InvalidRequest { id } => assert_eq!(id, expected),
                _ => panic!("expected invalid request"),
            }
        }
    }

    #[test]
    fn invalid_json_rpc_discards_invalid_request_id_types() {
        for line in [
            br#"{"jsonrpc":"2.0","id":true}"#.as_slice(),
            br#"{"jsonrpc":"2.0","id":[]}"#.as_slice(),
            br#"{"jsonrpc":"2.0","id":{}}"#.as_slice(),
            br#"{"jsonrpc":"2.0","id":null}"#.as_slice(),
        ] {
            match parse_client_message(line) {
                IncomingLine::InvalidRequest { id } => assert_eq!(id, serde_json::Value::Null),
                _ => panic!("expected invalid request"),
            }
        }
    }

    #[test]
    fn only_notifications_are_ignored_for_compatibility() {
        assert!(should_ignore_notification(&serde_json::json!({
            "method": "notifications/custom",
            "params": {}
        })));

        assert!(!should_ignore_notification(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "notifications/custom",
            "params": {}
        })));
    }

    #[tokio::test]
    async fn close_takes_and_shuts_down_writer() {
        let (_client_input, server_input) = tokio::io::duplex(1024);
        let (server_output, _client_output) = tokio::io::duplex(1024);
        let mut transport = ResilientStdioTransport::new(server_input, server_output);

        transport.close().await.unwrap();

        let err = write_raw_line(&transport.write, b"{}").await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotConnected);
    }
}
