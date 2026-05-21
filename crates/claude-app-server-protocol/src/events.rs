//! Notification payloads emitted by the server.

use crate::models::{Item, Thread, ThreadStatus, Turn};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadStartedEvent {
    pub thread: Thread,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadStatusChangedEvent {
    pub thread_id: String,
    pub status: ThreadStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadClosedEvent {
    pub thread_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnStartedEvent {
    pub thread_id: String,
    pub turn: Turn,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnCompletedEvent {
    pub thread_id: String,
    pub turn: Turn,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_usage: Option<TokenUsage>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation_input_tokens: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ItemStartedEvent {
    pub thread_id: String,
    pub turn_id: String,
    pub item: Item,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ItemCompletedEvent {
    pub thread_id: String,
    pub turn_id: String,
    pub item: Item,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ItemAgentMessageDeltaEvent {
    pub thread_id: String,
    pub turn_id: String,
    pub item_id: String,
    pub delta: String,
}
