//! Typed wrapper around the outbound JSON-RPC sender so request processors do
//! not stringly-construct method names.

use claude_app_server_protocol::{
    notification, ItemAgentMessageDeltaEvent, ItemCompletedEvent, ItemStartedEvent,
    JsonRpcMessage, JsonRpcNotification, JsonRpcResponse, RequestId, ThreadStartedEvent,
    ThreadStatusChangedEvent, TurnCompletedEvent, TurnStartedEvent,
};
use claude_app_server_transport::{make_error, make_response};
use serde::Serialize;
use tokio::sync::mpsc;

#[derive(Clone)]
pub struct OutgoingSender {
    tx: mpsc::Sender<JsonRpcMessage>,
}

impl OutgoingSender {
    pub fn new(tx: mpsc::Sender<JsonRpcMessage>) -> Self {
        Self { tx }
    }

    pub async fn respond<R: Serialize>(&self, id: RequestId, result: &R) {
        let value = serde_json::to_value(result).unwrap_or(serde_json::json!({}));
        let _ = self.tx.send(make_response(id, value)).await;
    }

    pub async fn respond_empty(&self, id: RequestId) {
        let _ = self.tx.send(make_response(id, serde_json::json!({}))).await;
    }

    pub async fn error(&self, id: RequestId, code: i64, message: impl Into<String>) {
        let _ = self.tx.send(make_error(id, code, message)).await;
    }

    pub async fn notify<P: Serialize>(&self, method: &str, params: &P) {
        let value = serde_json::to_value(params).unwrap_or(serde_json::json!({}));
        let _ = self
            .tx
            .send(JsonRpcMessage::Notification(JsonRpcNotification {
                method: method.to_string(),
                params: Some(value),
            }))
            .await;
    }

    pub async fn raw_response(&self, id: RequestId, result: serde_json::Value) {
        let _ = self.tx.send(JsonRpcMessage::Response(JsonRpcResponse { id, result })).await;
    }

    pub async fn thread_started(&self, e: &ThreadStartedEvent) {
        self.notify(notification::THREAD_STARTED, e).await;
    }

    pub async fn thread_status_changed(&self, e: &ThreadStatusChangedEvent) {
        self.notify(notification::THREAD_STATUS_CHANGED, e).await;
    }

    pub async fn turn_started(&self, e: &TurnStartedEvent) {
        self.notify(notification::TURN_STARTED, e).await;
    }

    pub async fn turn_completed(&self, e: &TurnCompletedEvent) {
        self.notify(notification::TURN_COMPLETED, e).await;
    }

    pub async fn item_started(&self, e: &ItemStartedEvent) {
        self.notify(notification::ITEM_STARTED, e).await;
    }

    pub async fn item_completed(&self, e: &ItemCompletedEvent) {
        self.notify(notification::ITEM_COMPLETED, e).await;
    }

    pub async fn item_agent_message_delta(&self, e: &ItemAgentMessageDeltaEvent) {
        self.notify(notification::ITEM_AGENT_MESSAGE_DELTA, e).await;
    }
}
