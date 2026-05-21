//! Domain models: Thread, Turn, Item. Mirrors codex shapes but slimmed to MVP fields.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientInfo {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub version: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientCapabilities {
    #[serde(default)]
    pub experimental_api: bool,
    #[serde(default)]
    pub opt_out_notification_methods: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerInfo {
    pub user_agent: String,
    pub codex_home: String,
    pub platform_family: String,
    pub platform_os: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Thread {
    pub id: String,
    #[serde(default)]
    pub preview: String,
    pub model_provider: String,
    pub created_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ephemeral: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forked_from_id: Option<String>,
    pub status: ThreadStatus,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub turns: Vec<Turn>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ThreadStatus {
    NotLoaded,
    Idle,
    SystemError,
    Active {
        #[serde(default, rename = "activeFlags")]
        active_flags: Vec<String>,
    },
}

impl Default for ThreadStatus {
    fn default() -> Self { Self::Idle }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Turn {
    pub id: String,
    pub status: TurnStatus,
    #[serde(default)]
    pub items: Vec<Item>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TurnStatus {
    InProgress,
    Completed,
    Interrupted,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Item {
    #[serde(rename_all = "camelCase")]
    UserMessage {
        id: String,
        content: Vec<InputChunk>,
    },
    #[serde(rename_all = "camelCase")]
    AgentMessage { id: String, text: String },
    #[serde(rename_all = "camelCase")]
    Reasoning { id: String, text: String },
    #[serde(rename_all = "camelCase")]
    ToolUse {
        id: String,
        tool_use_id: String,
        name: String,
        input: serde_json::Value,
    },
    #[serde(rename_all = "camelCase")]
    ToolResult {
        id: String,
        tool_use_id: String,
        content: serde_json::Value,
        is_error: bool,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum InputChunk {
    #[serde(rename_all = "camelCase")]
    Text { text: String },
    #[serde(rename_all = "camelCase")]
    Image { url: String },
    #[serde(rename_all = "camelCase")]
    LocalImage { path: String },
}
