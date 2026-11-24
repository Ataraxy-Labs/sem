use std::{future::Future, sync::Arc};

use rmcp::model::{
    ClientJsonRpcMessage, ClientRequest, ErrorCode, ErrorData, ServerJsonRpcMessage,
};
use rmcp::transport::Transport;
use rmcp::RoleServer;
use serde::Serialize;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;

const PARSE_ERROR_RESPONSE: &[u8] =
    br#"{"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"Parse error"}}"#;

/// The largest request line stdio accepts (issue #553), the same 1 MiB the HTTP transport's body limit allows.
const MAX_REQUEST_BYTES: usize = 1024 * 1024;

const TOO_LARGE_RESPONSE: &[u8] =
    br#"{"jsonrpc":"2.0","id":null,"error":{"code":-32600,"message":"Invalid Request: a request line is at most 1 MiB"}}"#;

/// What one bounded read of a line found.
#[derive(Debug, PartialEq)]
enum Bounded {
    /// a line (possibly the last, unterminated one) in the buffer
    Line,
    /// a line past the bound: read through its end and discarded, never held in memory
    TooLong,
    /// end of input, nothing read
    Eof,
}

/// Read one '\n'-terminated line into `out`, holding at most `max` bytes of it (plus the newline). A longer line is
/// consumed through its newline in the reader's own chunks and reported as TooLong, so the next read starts at the
/// next request.
async fn read_bounded_line<R: AsyncRead + Unpin>(
    r: &mut BufReader<R>,
    out: &mut Vec<u8>,
    max: usize,
) -> std::io::Result<Bounded> {
    let mut too_long = false;
    loop {
        let buf = r.fill_buf().await?;
        if buf.is_empty() {
            return Ok(if too_long {
                Bounded::TooLong
            } else if out.is_empty() {
                Bounded::Eof
            } else {
                Bounded::Line
            });
        }
        let (n, done) = match buf.iter().position(|&b| b == b'\n') {
            Some(i) => (i + 1, true),
            None => (buf.len(), false),
        };
        if !too_long {
            if out.len() + n > max + 1 {
                too_long = true;
                out.clear();
            } else {
                out.extend_from_slice(&buf[..n]);
            }
        }
        r.consume(n);
        if done {
            return Ok(if too_long { Bounded::TooLong } else { Bounded::Line });
        }
    }
}

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

            match read_bounded_line(&mut self.read, &mut line, MAX_REQUEST_BYTES).await {
                Ok(Bounded::Eof) => return None,
                Ok(Bounded::Line) => {}
                Ok(Bounded::TooLong) => {
                    tracing::debug!("Request line over {} bytes discarded", MAX_REQUEST_BYTES);
                    let write = Arc::clone(&self.write);
                    if let Err(error) = write_raw_line(&write, TOO_LARGE_RESPONSE).await {
                        tracing::error!("Error writing request too large response: {}", error);
                        return None;
                    }
                    continue;
                }
                Err(error) => {
                    tracing::error!("Error reading from stream: {}", error);
                    return None;
                }
            }

            let line = without_line_ending(&line);
            match parse_client_message(line) {
                IncomingLine::Message(message) => {
                    // Reject unsupported extension methods here, before RMCP's
                    // initialize gate. Clients such as Copilot probe server/discover
                    // first and fall back to initialize on Method not found.
                    // Passing the probe to RMCP 1.x instead terminates the session.
                    if let ClientJsonRpcMessage::Request(request) = message.as_ref() {
                        if matches!(request.request, ClientRequest::CustomRequest(_)) {
                            if let Err(error) = self
                                .send(ServerJsonRpcMessage::error(
                                    ErrorData::new(
                                        ErrorCode::METHOD_NOT_FOUND,
                                        "Method not found",
                                        None,
                                    ),
                                    request.id.clone(),
                                ))
                                .await
                            {
                                tracing::error!(
                                    "Error writing method not found response: {}",
                                    error
                                );
                                return None;
                            }
                            continue;
                        }
                    }
                    return Some(*message);
                }
                IncomingLine::Ignore => {}
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
}

fn parse_client_message(line: &[u8]) -> IncomingLine {
    match serde_json::from_slice::<ClientJsonRpcMessage>(line) {
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
    async fn unsupported_probes_preserve_ids_and_keep_transport_open() {
        let (mut client_input, server_input) = tokio::io::duplex(4096);
        let (server_output, client_output) = tokio::io::duplex(4096);
        let mut transport = ResilientStdioTransport::new(server_input, server_output);
        client_input
            .write_all(
                br#"{"jsonrpc":"2.0","id":0,"method":"server/discover","params":{}}
{"jsonrpc":"2.0","id":"probe","method":"future/extension","params":{}}
{"jsonrpc":"2.0","id":2,"method":"ping"}
"#,
            )
            .await
            .unwrap();
        let message = tokio::time::timeout(std::time::Duration::from_secs(2), transport.receive())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(message, ClientJsonRpcMessage::Request(request)
            if matches!(request.request, ClientRequest::PingRequest(_))));
        let mut output = BufReader::new(client_output);
        for id in [serde_json::json!(0), serde_json::json!("probe")] {
            let mut line = String::new();
            output.read_line(&mut line).await.unwrap();
            let response: serde_json::Value = serde_json::from_str(&line).unwrap();
            assert_eq!(response["id"], id);
            assert_eq!(response["error"]["code"], -32601);
        }
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


    /// Issue #553: a request line past the bound is drained and reported, never held whole, and the next line
    /// reads normally.
    #[tokio::test]
    async fn an_oversized_line_is_drained_and_the_next_one_reads() {
        let mut input = vec![b'x'; 300];
        input.push(b'\n');
        input.extend_from_slice(b"short\n");
        input.extend_from_slice(b"last");
        let mut r = BufReader::with_capacity(16, &input[..]);
        let mut line = Vec::new();
        assert_eq!(read_bounded_line(&mut r, &mut line, 100).await.unwrap(), Bounded::TooLong);
        assert!(line.is_empty());
        line.clear();
        assert_eq!(read_bounded_line(&mut r, &mut line, 100).await.unwrap(), Bounded::Line);
        assert_eq!(line, b"short\n");
        line.clear();
        assert_eq!(read_bounded_line(&mut r, &mut line, 100).await.unwrap(), Bounded::Line);
        assert_eq!(line, b"last");
        line.clear();
        assert_eq!(read_bounded_line(&mut r, &mut line, 100).await.unwrap(), Bounded::Eof);
    }
}
