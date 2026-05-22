//! Stdio JSONL transport for the Claude app-server.
//!
//! Mirrors codex semantics:
//!   - newline-delimited JSON, one `JsonRpcMessage` per line
//!   - bounded mpsc queues between transport ingress / processing / outbound writer
//!   - when ingress queue is saturated, the offending request is rejected with
//!     JSON-RPC error code `-32001` ("Server overloaded; retry later.")
//!
//! `jsonrpc` field is intentionally omitted on the wire.

use claude_app_server_protocol::{
    JsonRpcErrorBody, JsonRpcErrorMessage, JsonRpcMessage, JsonRpcRequest, RequestId,
    SERVER_OVERLOADED,
};
use std::io;
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use tracing::{debug, error, warn};

const DEFAULT_INGRESS_CAPACITY: usize = 256;
const DEFAULT_OUTBOUND_CAPACITY: usize = 1024;

fn ingress_capacity() -> usize {
    std::env::var("CLAUDE_APP_SERVER_INGRESS_CAPACITY")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_INGRESS_CAPACITY)
}

fn outbound_capacity() -> usize {
    std::env::var("CLAUDE_APP_SERVER_OUTBOUND_CAPACITY")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_OUTBOUND_CAPACITY)
}

#[derive(Debug, Error)]
pub enum TransportError {
    #[error("io: {0}")]
    Io(#[from] io::Error),
    #[error("encode: {0}")]
    Encode(#[from] serde_json::Error),
}

/// Handles to a running stdio transport.
pub struct StdioTransport {
    pub incoming: mpsc::Receiver<JsonRpcMessage>,
    pub outgoing: mpsc::Sender<JsonRpcMessage>,
}

impl StdioTransport {
    pub fn spawn() -> Self {
        let (ingress_tx, ingress_rx) = mpsc::channel::<JsonRpcMessage>(ingress_capacity());
        let (outbound_tx, outbound_rx) = mpsc::channel::<JsonRpcMessage>(outbound_capacity());

        // Reader: stdin -> ingress_tx. On backpressure (ingress full and message is a Request),
        // synthesize a -32001 overload error directly to outbound_tx.
        let outbound_for_overload = outbound_tx.clone();
        tokio::spawn(async move {
            let stdin = tokio::io::stdin();
            let mut reader = BufReader::new(stdin);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) => {
                        debug!("stdin closed");
                        break;
                    }
                    Ok(_) => {
                        let trimmed = line.trim();
                        if trimmed.is_empty() {
                            continue;
                        }
                        match serde_json::from_str::<JsonRpcMessage>(trimmed) {
                            Ok(msg) => {
                                if let Err(mpsc::error::TrySendError::Full(rejected)) =
                                    ingress_tx.try_send(msg)
                                {
                                    if let JsonRpcMessage::Request(req) = rejected {
                                        let err = JsonRpcMessage::Error(JsonRpcErrorMessage {
                                            id: req.id,
                                            error: JsonRpcErrorBody::new(
                                                SERVER_OVERLOADED,
                                                "Server overloaded; retry later.",
                                            ),
                                        });
                                        let _ = outbound_for_overload.send(err).await;
                                    }
                                    warn!("ingress queue full; rejected request");
                                } else if ingress_tx.is_closed() {
                                    break;
                                }
                            }
                            Err(e) => {
                                warn!("invalid jsonrpc line: {e}");
                            }
                        }
                    }
                    Err(e) => {
                        error!("stdin read failed: {e}");
                        break;
                    }
                }
            }
        });

        // Writer: outbound_rx -> stdout. One JSON document per line.
        tokio::spawn(async move {
            let mut stdout = tokio::io::stdout();
            let mut rx = outbound_rx;
            while let Some(msg) = rx.recv().await {
                match serde_json::to_string(&msg) {
                    Ok(mut s) => {
                        s.push('\n');
                        if let Err(e) = stdout.write_all(s.as_bytes()).await {
                            error!("stdout write failed: {e}");
                            break;
                        }
                        if let Err(e) = stdout.flush().await {
                            error!("stdout flush failed: {e}");
                            break;
                        }
                    }
                    Err(e) => warn!("could not encode outbound message: {e}"),
                }
            }
        });

        Self {
            incoming: ingress_rx,
            outgoing: outbound_tx,
        }
    }
}

/// Helper for synthesizing an error response for a known request id.
pub fn make_error(id: RequestId, code: i64, message: impl Into<String>) -> JsonRpcMessage {
    JsonRpcMessage::Error(JsonRpcErrorMessage {
        id,
        error: JsonRpcErrorBody::new(code, message),
    })
}

/// Helper for synthesizing a successful response.
pub fn make_response(id: RequestId, result: serde_json::Value) -> JsonRpcMessage {
    JsonRpcMessage::Response(claude_app_server_protocol::JsonRpcResponse { id, result })
}

/// Helper for synthesizing a notification.
pub fn make_notification(method: &str, params: serde_json::Value) -> JsonRpcMessage {
    JsonRpcMessage::Notification(claude_app_server_protocol::JsonRpcNotification {
        method: method.to_string(),
        params: Some(params),
    })
}

/// Re-export request shape for convenience.
pub type IncomingRequest = JsonRpcRequest;
