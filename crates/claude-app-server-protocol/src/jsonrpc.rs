//! JSON-RPC 2.0 message envelope. Matches codex app-server semantics:
//! `jsonrpc` field omitted on the wire; messages discriminated structurally.

use serde::{Deserialize, Serialize};
use std::fmt;

pub const JSONRPC_VERSION: &str = "2.0";

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(untagged)]
pub enum RequestId {
    String(String),
    Integer(i64),
}

impl fmt::Display for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::String(v) => f.write_str(v),
            Self::Integer(v) => write!(f, "{v}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(untagged)]
pub enum JsonRpcMessage {
    Request(JsonRpcRequest),
    Response(JsonRpcResponse),
    Error(JsonRpcErrorMessage),
    Notification(JsonRpcNotification),
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct JsonRpcRequest {
    pub id: RequestId,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct JsonRpcNotification {
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct JsonRpcResponse {
    pub id: RequestId,
    pub result: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct JsonRpcErrorMessage {
    pub id: RequestId,
    pub error: JsonRpcErrorBody,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct JsonRpcErrorBody {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

impl JsonRpcErrorBody {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self { code, message: message.into(), data: None }
    }
}
