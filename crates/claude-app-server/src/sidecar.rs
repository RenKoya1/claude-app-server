//! Driver for the long-lived TypeScript sidecar that wraps
//! `@anthropic-ai/claude-agent-sdk`.
//!
//! Rust manages JSON-RPC + thread state + multi-client fan-out. All
//! interaction with the model loop (turns, tool use, MCP, hooks, skills,
//! subagents, sessions) goes through the sidecar over newline-delimited JSON
//! on stdin/stdout.
//!
//! This module:
//!   - locates and spawns the sidecar (`node dist/index.js` or an explicit
//!     `CLAUDE_APP_SERVER_SIDECAR` binary),
//!   - tracks pending command acks via a `oneshot` registry keyed on the
//!     auto-generated command id,
//!   - re-publishes streaming events on a broadcast channel so the
//!     `MessageProcessor` can pick them up per-turn,
//!   - exposes a typed `SidecarClient` with `create_session` / `turn` /
//!     `interrupt` / `close_session` / `shutdown` helpers.

use anyhow::{anyhow, bail, Context};
use claude_app_server_protocol::InputChunk;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{broadcast, oneshot, Mutex};
use tracing::{debug, error, info, warn};

const ACK_TIMEOUT_SECS: u64 = 10;
const BROADCAST_CAPACITY: usize = 1024;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase", tag = "type")]
enum OutboundCommand {
    #[serde(rename_all = "camelCase")]
    CreateSession {
        id: String,
        session_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        options: Option<SidecarSessionOptions>,
    },
    #[serde(rename_all = "camelCase")]
    Turn {
        id: String,
        session_id: String,
        turn_id: String,
        input: Vec<TurnInput>,
    },
    #[serde(rename_all = "camelCase")]
    SteerTurn {
        id: String,
        session_id: String,
        turn_id: String,
        input: Vec<TurnInput>,
    },
    #[serde(rename_all = "camelCase")]
    InjectItems {
        id: String,
        session_id: String,
        items: Vec<Value>,
    },
    #[serde(rename_all = "camelCase")]
    Interrupt { id: String, session_id: String, turn_id: String },
    #[serde(rename_all = "camelCase")]
    Compact { id: String, session_id: String },
    #[serde(rename_all = "camelCase")]
    CloseSession { id: String, session_id: String },
    // --- Query control (runtime steering of a live session) ---
    #[serde(rename_all = "camelCase")]
    SetModel {
        id: String,
        session_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        model: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    SetPermissionMode {
        id: String,
        session_id: String,
        mode: String,
    },
    #[serde(rename_all = "camelCase")]
    SetMaxThinkingTokens {
        id: String,
        session_id: String,
        max_thinking_tokens: Option<i64>,
    },
    #[serde(rename_all = "camelCase")]
    SetMcpServers {
        id: String,
        session_id: String,
        servers: Value,
    },
    #[serde(rename_all = "camelCase")]
    RewindFiles {
        id: String,
        session_id: String,
        user_message_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        dry_run: Option<bool>,
    },
    #[serde(rename_all = "camelCase")]
    SupportedCommands { id: String, session_id: String },
    #[serde(rename_all = "camelCase")]
    SupportedModels { id: String, session_id: String },
    #[serde(rename_all = "camelCase")]
    McpServerStatus { id: String, session_id: String },
    #[serde(rename_all = "camelCase")]
    AccountInfo { id: String, session_id: String },
    // --- Bridge responses (Rust → sidecar reply to a bridge event) ---
    #[serde(rename_all = "camelCase")]
    PermissionResponse {
        id: String,
        request_id: String,
        result: Value,
    },
    #[serde(rename_all = "camelCase")]
    HookResponse {
        id: String,
        request_id: String,
        output: Value,
    },
    // --- Legacy enumeration helpers ---
    #[serde(rename_all = "camelCase")]
    ListSkills { id: String, #[serde(skip_serializing_if = "Vec::is_empty")] cwds: Vec<String> },
    #[serde(rename_all = "camelCase")]
    ListHooks { id: String, #[serde(skip_serializing_if = "Vec::is_empty")] cwds: Vec<String> },
    #[serde(rename_all = "camelCase")]
    ListMcpServers {
        id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    CallMcpTool {
        id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        server: String,
        tool: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        arguments: Option<Value>,
    },
    Shutdown { id: String },
}

/// Opaque session-options blob. Whatever JSON we put here is forwarded
/// verbatim into the sidecar's `query({ options: ... })` call. Use any
/// key the `@anthropic-ai/claude-agent-sdk` `Options` type accepts —
/// `mcpServers`, `agents`, `hooks` (callbacks not supported via JSON),
/// `additionalDirectories`, `env`, `allowedTools`, `disallowedTools`,
/// `permissionMode`, `maxTurns`, etc. The frontend does not validate the
/// shape; if the SDK rejects it the sidecar emits a `turnCompleted`
/// error event with the upstream message.
pub type SidecarSessionOptions = serde_json::Map<String, Value>;

/// Helper: build a session-options blob, merging known keys with an
/// arbitrary passthrough map. The passthrough wins when keys collide so
/// callers can override defaults from the typed ergonomic surface.
pub fn build_session_options(
    typed_keys: impl IntoIterator<Item = (&'static str, Value)>,
    passthrough: serde_json::Map<String, Value>,
) -> SidecarSessionOptions {
    let mut map = serde_json::Map::new();
    for (k, v) in typed_keys {
        if !v.is_null() {
            map.insert(k.to_string(), v);
        }
    }
    for (k, v) in passthrough {
        map.insert(k, v);
    }
    map
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum TurnInput {
    #[serde(rename_all = "camelCase")]
    Text { text: String },
    #[serde(rename_all = "camelCase")]
    Image { url: String },
    #[serde(rename_all = "camelCase")]
    LocalImage { path: String },
}

impl From<InputChunk> for TurnInput {
    fn from(chunk: InputChunk) -> Self {
        match chunk {
            InputChunk::Text { text } => TurnInput::Text { text },
            InputChunk::Image { url } => TurnInput::Image { url },
            InputChunk::LocalImage { path } => TurnInput::LocalImage { path },
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum SidecarEvent {
    #[serde(rename_all = "camelCase")]
    Ack { id: String, ok: bool, #[serde(default)] error: Option<String> },
    /// Like `Ack { ok: true }` but carries a JSON payload for query-style commands.
    #[serde(rename_all = "camelCase")]
    Result { id: String, payload: Value },
    Ready,
    #[serde(rename_all = "camelCase")]
    Log { level: String, msg: String },
    #[serde(rename_all = "camelCase")]
    SessionReady { session_id: String },
    /// First message after the SDK loop initializes. Carries the SDK session
    /// id plus the discovered tool/mcp/skill/agent inventory so clients can
    /// render UI state without an extra round-trip.
    #[serde(rename_all = "camelCase")]
    SessionInit {
        session_id: String,
        sdk_session_id: String,
        #[serde(default)] model: Option<String>,
        #[serde(default)] tools: Option<Vec<String>>,
        #[serde(default)] mcp_servers: Option<Vec<Value>>,
        #[serde(default)] slash_commands: Option<Vec<String>>,
        #[serde(default)] skills: Option<Vec<String>>,
        #[serde(default)] agents: Option<Vec<String>>,
        #[serde(default)] permission_mode: Option<String>,
        #[serde(default)] cwd: Option<String>,
        #[serde(default)] claude_code_version: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    SessionClosed { session_id: String, #[serde(default)] reason: Option<String> },
    #[serde(rename_all = "camelCase")]
    TurnStarted { session_id: String, turn_id: String },
    #[serde(rename_all = "camelCase")]
    AssistantDelta { session_id: String, turn_id: String, item_id: String, delta: String },
    #[serde(rename_all = "camelCase")]
    AssistantMessage { session_id: String, turn_id: String, item_id: String, text: String },
    #[serde(rename_all = "camelCase")]
    ToolUse {
        session_id: String,
        turn_id: String,
        tool_use_id: String,
        name: String,
        input: Value,
    },
    #[serde(rename_all = "camelCase")]
    ToolResult {
        session_id: String,
        turn_id: String,
        tool_use_id: String,
        content: Value,
        is_error: bool,
    },
    #[serde(rename_all = "camelCase")]
    Reasoning { session_id: String, turn_id: String, item_id: String, text: String },
    #[serde(rename_all = "camelCase")]
    ReasoningDelta { session_id: String, turn_id: String, item_id: String, delta: String },
    #[serde(rename_all = "camelCase")]
    TokenUsageUpdated {
        session_id: String,
        turn_id: String,
        usage: Value,
        #[serde(default)] model_usage: Option<Value>,
    },
    #[serde(rename_all = "camelCase")]
    CompactBoundary {
        session_id: String,
        trigger: String,
        pre_tokens: u64,
    },
    #[serde(rename_all = "camelCase")]
    ModelRerouted { session_id: String, reason: String },
    /// Bridge: a hook callback fired in the SDK. If `expect_response` is
    /// true the Rust side must reply with `HookResponse { request_id }` or
    /// the SDK will hang on this hook.
    #[serde(rename_all = "camelCase")]
    HookEvent {
        session_id: String,
        request_id: String,
        event: String,
        #[serde(default)] tool_use_id: Option<String>,
        payload: Value,
        expect_response: bool,
    },
    /// Bridge: a canUseTool callback fired. The SDK is blocked until the
    /// Rust side replies with `PermissionResponse { request_id, result }`.
    #[serde(rename_all = "camelCase")]
    PermissionRequest {
        session_id: String,
        request_id: String,
        tool_name: String,
        tool_use_id: String,
        input: Value,
        #[serde(default)] suggestions: Option<Value>,
        #[serde(default)] blocked_path: Option<String>,
        #[serde(default)] decision_reason: Option<String>,
        #[serde(default)] agent_id: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    TurnCompleted {
        session_id: String,
        turn_id: String,
        is_error: bool,
        #[serde(default)] result: Option<String>,
        #[serde(default)] errors: Option<Vec<String>>,
        #[serde(default)] usage: Option<Value>,
        #[serde(default)] model_usage: Option<Value>,
        #[serde(default)] total_cost_usd: Option<f64>,
        #[serde(default)] duration_ms: Option<u64>,
        #[serde(default)] num_turns: Option<u32>,
        #[serde(default)] permission_denials: Option<Value>,
        #[serde(default)] structured_output: Option<Value>,
    },
}

#[derive(Clone)]
pub struct SidecarClient {
    inner: Arc<Inner>,
}

struct Inner {
    stdin: Mutex<ChildStdin>,
    next_id: Mutex<u64>,
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<AckResult>>>>,
    events_tx: broadcast::Sender<SidecarEvent>,
    _child: Mutex<Child>,
}

#[derive(Debug)]
struct AckResult {
    ok: bool,
    error: Option<String>,
    payload: Option<Value>,
}

impl SidecarClient {
    /// Spawn the sidecar. Resolution order for the executable:
    ///   1. `CLAUDE_APP_SERVER_SIDECAR` env var (path to a node-runnable file
    ///      or a single shell command).
    ///   2. `claude/sidecar/dist/index.js` relative to the binary or cwd.
    pub async fn spawn() -> anyhow::Result<Self> {
        let (cmd, args) = resolve_sidecar()?;
        debug!(?cmd, ?args, "spawning sidecar");
        let mut child = Command::new(&cmd)
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("could not spawn sidecar {cmd:?}"))?;

        let stdin = child.stdin.take().context("sidecar has no stdin")?;
        let stdout = child.stdout.take().context("sidecar has no stdout")?;
        let stderr = child.stderr.take().context("sidecar has no stderr")?;

        let (events_tx, _) = broadcast::channel(BROADCAST_CAPACITY);
        let pending: Arc<Mutex<HashMap<String, oneshot::Sender<AckResult>>>> =
            Arc::new(Mutex::new(HashMap::new()));

        // Reader task — uses the same `pending` Arc that Inner will hold so
        // that `dispatch` and the reader see one shared registry.
        {
            let events_tx = events_tx.clone();
            let pending = pending.clone();
            tokio::spawn(async move {
                let mut reader = BufReader::new(stdout);
                let mut line = String::new();
                loop {
                    line.clear();
                    match reader.read_line(&mut line).await {
                        Ok(0) => {
                            warn!("sidecar stdout closed");
                            break;
                        }
                        Ok(_) => {
                            let trimmed = line.trim();
                            if trimmed.is_empty() { continue; }
                            match serde_json::from_str::<SidecarEvent>(trimmed) {
                                Ok(SidecarEvent::Ack { id, ok, error }) => {
                                    let mut guard = pending.lock().await;
                                    if let Some(tx) = guard.remove(&id) {
                                        let _ = tx.send(AckResult { ok, error, payload: None });
                                    }
                                }
                                Ok(SidecarEvent::Result { id, payload }) => {
                                    let mut guard = pending.lock().await;
                                    if let Some(tx) = guard.remove(&id) {
                                        let _ = tx.send(AckResult {
                                            ok: true,
                                            error: None,
                                            payload: Some(payload),
                                        });
                                    }
                                }
                                Ok(SidecarEvent::Log { level, msg }) => {
                                    match level.as_str() {
                                        "error" => error!(target: "sidecar", "{msg}"),
                                        "warn" => warn!(target: "sidecar", "{msg}"),
                                        _ => info!(target: "sidecar", "{msg}"),
                                    }
                                }
                                Ok(event) => {
                                    let _ = events_tx.send(event);
                                }
                                Err(e) => {
                                    warn!("could not decode sidecar event '{}': {e}", trimmed);
                                }
                            }
                        }
                        Err(e) => {
                            error!("sidecar stdout read failed: {e}");
                            break;
                        }
                    }
                }
            });
        }

        // Stderr: forward sidecar messages to our tracing layer.
        tokio::spawn(async move {
            let mut reader = BufReader::new(stderr);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) => break,
                    Ok(_) => warn!(target: "sidecar", "{}", line.trim_end()),
                    Err(_) => break,
                }
            }
        });

        let inner = Arc::new(Inner {
            stdin: Mutex::new(stdin),
            next_id: Mutex::new(1),
            pending,
            events_tx: events_tx.clone(),
            _child: Mutex::new(child),
        });

        let client = SidecarClient { inner };

        // Wait for the `ready` frame before returning. The sidecar emits it
        // as soon as its readline loop is up.
        let mut rx = events_tx.subscribe();
        let ready_timeout = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            async {
                loop {
                    match rx.recv().await {
                        Ok(SidecarEvent::Ready) => return Ok(()),
                        Ok(_) => continue,
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(e) => return Err(anyhow!("sidecar closed before ready: {e}")),
                    }
                }
            },
        )
        .await;
        match ready_timeout {
            Ok(Ok(())) => {}
            Ok(Err(e)) => bail!(e),
            Err(_) => bail!("sidecar did not emit ready within 10s"),
        }

        Ok(client)
    }

    /// Subscribe to the streaming event firehose. Each subscriber gets every
    /// event observed after the subscription; old events are not replayed.
    pub fn subscribe(&self) -> broadcast::Receiver<SidecarEvent> {
        self.inner.events_tx.subscribe()
    }

    async fn next_id(&self) -> String {
        let mut guard = self.inner.next_id.lock().await;
        let id = *guard;
        *guard += 1;
        format!("c{id}")
    }

    async fn send_command(&self, command: OutboundCommand, id: &str) -> anyhow::Result<()> {
        let line = serde_json::to_string(&command)? + "\n";
        let mut stdin = self.inner.stdin.lock().await;
        stdin
            .write_all(line.as_bytes())
            .await
            .with_context(|| format!("could not write sidecar command (id={id})"))?;
        stdin.flush().await?;
        Ok(())
    }

    async fn dispatch(&self, command: OutboundCommand) -> anyhow::Result<()> {
        let _ = self.dispatch_with_payload(command).await?;
        Ok(())
    }

    async fn dispatch_with_payload(
        &self,
        command: OutboundCommand,
    ) -> anyhow::Result<Option<Value>> {
        let id = command_id(&command);
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.inner.pending.lock().await;
            pending.insert(id.clone(), tx);
        }
        self.send_command(command, &id).await?;

        let ack = tokio::time::timeout(
            std::time::Duration::from_secs(ACK_TIMEOUT_SECS),
            rx,
        )
        .await
        .map_err(|_| anyhow!("sidecar ack timeout for id={id}"))?
        .map_err(|_| anyhow!("sidecar dropped pending ack for id={id}"))?;
        if !ack.ok {
            bail!("sidecar rejected command id={id}: {}", ack.error.unwrap_or_default());
        }
        Ok(ack.payload)
    }

    pub async fn create_session(
        &self,
        session_id: &str,
        options: Option<SidecarSessionOptions>,
    ) -> anyhow::Result<()> {
        let id = self.next_id().await;
        self.dispatch(OutboundCommand::CreateSession {
            id,
            session_id: session_id.to_string(),
            options,
        })
        .await
    }

    pub async fn turn(
        &self,
        session_id: &str,
        turn_id: &str,
        input: Vec<TurnInput>,
    ) -> anyhow::Result<()> {
        let id = self.next_id().await;
        self.dispatch(OutboundCommand::Turn {
            id,
            session_id: session_id.to_string(),
            turn_id: turn_id.to_string(),
            input,
        })
        .await
    }

    pub async fn interrupt(&self, session_id: &str, turn_id: &str) -> anyhow::Result<()> {
        let id = self.next_id().await;
        self.dispatch(OutboundCommand::Interrupt {
            id,
            session_id: session_id.to_string(),
            turn_id: turn_id.to_string(),
        })
        .await
    }

    pub async fn close_session(&self, session_id: &str) -> anyhow::Result<()> {
        let id = self.next_id().await;
        self.dispatch(OutboundCommand::CloseSession {
            id,
            session_id: session_id.to_string(),
        })
        .await
    }

    pub async fn shutdown(&self) -> anyhow::Result<()> {
        let id = self.next_id().await;
        self.dispatch(OutboundCommand::Shutdown { id }).await
    }

    pub async fn steer_turn(
        &self,
        session_id: &str,
        turn_id: &str,
        input: Vec<TurnInput>,
    ) -> anyhow::Result<()> {
        let id = self.next_id().await;
        self.dispatch(OutboundCommand::SteerTurn {
            id,
            session_id: session_id.to_string(),
            turn_id: turn_id.to_string(),
            input,
        })
        .await
    }

    pub async fn inject_items(
        &self,
        session_id: &str,
        items: Vec<Value>,
    ) -> anyhow::Result<()> {
        let id = self.next_id().await;
        self.dispatch(OutboundCommand::InjectItems {
            id,
            session_id: session_id.to_string(),
            items,
        })
        .await
    }

    pub async fn compact(&self, session_id: &str) -> anyhow::Result<()> {
        let id = self.next_id().await;
        self.dispatch(OutboundCommand::Compact {
            id,
            session_id: session_id.to_string(),
        })
        .await
    }

    pub async fn list_skills(&self, cwds: Vec<String>) -> anyhow::Result<Value> {
        let id = self.next_id().await;
        let payload = self
            .dispatch_with_payload(OutboundCommand::ListSkills { id, cwds })
            .await?;
        Ok(payload.unwrap_or(Value::Null))
    }

    pub async fn list_hooks(&self, cwds: Vec<String>) -> anyhow::Result<Value> {
        let id = self.next_id().await;
        let payload = self
            .dispatch_with_payload(OutboundCommand::ListHooks { id, cwds })
            .await?;
        Ok(payload.unwrap_or(Value::Null))
    }

    pub async fn list_mcp_servers(
        &self,
        session_id: Option<String>,
    ) -> anyhow::Result<Value> {
        let id = self.next_id().await;
        let payload = self
            .dispatch_with_payload(OutboundCommand::ListMcpServers { id, session_id })
            .await?;
        Ok(payload.unwrap_or(Value::Null))
    }

    pub async fn call_mcp_tool(
        &self,
        session_id: Option<String>,
        server: String,
        tool: String,
        arguments: Option<Value>,
    ) -> anyhow::Result<Value> {
        let id = self.next_id().await;
        let payload = self
            .dispatch_with_payload(OutboundCommand::CallMcpTool {
                id,
                session_id,
                server,
                tool,
                arguments,
            })
            .await?;
        Ok(payload.unwrap_or(Value::Null))
    }

    // --- Query control ------------------------------------------------

    pub async fn set_model(&self, session_id: &str, model: Option<String>) -> anyhow::Result<()> {
        let id = self.next_id().await;
        self.dispatch(OutboundCommand::SetModel {
            id,
            session_id: session_id.to_string(),
            model,
        })
        .await
    }

    pub async fn set_permission_mode(&self, session_id: &str, mode: &str) -> anyhow::Result<()> {
        let id = self.next_id().await;
        self.dispatch(OutboundCommand::SetPermissionMode {
            id,
            session_id: session_id.to_string(),
            mode: mode.to_string(),
        })
        .await
    }

    pub async fn set_max_thinking_tokens(
        &self,
        session_id: &str,
        max_thinking_tokens: Option<i64>,
    ) -> anyhow::Result<()> {
        let id = self.next_id().await;
        self.dispatch(OutboundCommand::SetMaxThinkingTokens {
            id,
            session_id: session_id.to_string(),
            max_thinking_tokens,
        })
        .await
    }

    pub async fn set_mcp_servers(
        &self,
        session_id: &str,
        servers: Value,
    ) -> anyhow::Result<Value> {
        let id = self.next_id().await;
        let payload = self
            .dispatch_with_payload(OutboundCommand::SetMcpServers {
                id,
                session_id: session_id.to_string(),
                servers,
            })
            .await?;
        Ok(payload.unwrap_or(Value::Null))
    }

    pub async fn rewind_files(
        &self,
        session_id: &str,
        user_message_id: &str,
        dry_run: Option<bool>,
    ) -> anyhow::Result<Value> {
        let id = self.next_id().await;
        let payload = self
            .dispatch_with_payload(OutboundCommand::RewindFiles {
                id,
                session_id: session_id.to_string(),
                user_message_id: user_message_id.to_string(),
                dry_run,
            })
            .await?;
        Ok(payload.unwrap_or(Value::Null))
    }

    pub async fn supported_commands(&self, session_id: &str) -> anyhow::Result<Value> {
        let id = self.next_id().await;
        let payload = self
            .dispatch_with_payload(OutboundCommand::SupportedCommands {
                id,
                session_id: session_id.to_string(),
            })
            .await?;
        Ok(payload.unwrap_or(Value::Null))
    }

    pub async fn supported_models(&self, session_id: &str) -> anyhow::Result<Value> {
        let id = self.next_id().await;
        let payload = self
            .dispatch_with_payload(OutboundCommand::SupportedModels {
                id,
                session_id: session_id.to_string(),
            })
            .await?;
        Ok(payload.unwrap_or(Value::Null))
    }

    pub async fn mcp_server_status(&self, session_id: &str) -> anyhow::Result<Value> {
        let id = self.next_id().await;
        let payload = self
            .dispatch_with_payload(OutboundCommand::McpServerStatus {
                id,
                session_id: session_id.to_string(),
            })
            .await?;
        Ok(payload.unwrap_or(Value::Null))
    }

    pub async fn account_info(&self, session_id: &str) -> anyhow::Result<Value> {
        let id = self.next_id().await;
        let payload = self
            .dispatch_with_payload(OutboundCommand::AccountInfo {
                id,
                session_id: session_id.to_string(),
            })
            .await?;
        Ok(payload.unwrap_or(Value::Null))
    }

    pub async fn permission_response(
        &self,
        request_id: &str,
        result: Value,
    ) -> anyhow::Result<()> {
        let id = self.next_id().await;
        self.dispatch(OutboundCommand::PermissionResponse {
            id,
            request_id: request_id.to_string(),
            result,
        })
        .await
    }

    pub async fn hook_response(
        &self,
        request_id: &str,
        output: Value,
    ) -> anyhow::Result<()> {
        let id = self.next_id().await;
        self.dispatch(OutboundCommand::HookResponse {
            id,
            request_id: request_id.to_string(),
            output,
        })
        .await
    }
}

fn command_id(c: &OutboundCommand) -> String {
    match c {
        OutboundCommand::CreateSession { id, .. }
        | OutboundCommand::Turn { id, .. }
        | OutboundCommand::SteerTurn { id, .. }
        | OutboundCommand::InjectItems { id, .. }
        | OutboundCommand::Interrupt { id, .. }
        | OutboundCommand::Compact { id, .. }
        | OutboundCommand::CloseSession { id, .. }
        | OutboundCommand::SetModel { id, .. }
        | OutboundCommand::SetPermissionMode { id, .. }
        | OutboundCommand::SetMaxThinkingTokens { id, .. }
        | OutboundCommand::SetMcpServers { id, .. }
        | OutboundCommand::RewindFiles { id, .. }
        | OutboundCommand::SupportedCommands { id, .. }
        | OutboundCommand::SupportedModels { id, .. }
        | OutboundCommand::McpServerStatus { id, .. }
        | OutboundCommand::AccountInfo { id, .. }
        | OutboundCommand::PermissionResponse { id, .. }
        | OutboundCommand::HookResponse { id, .. }
        | OutboundCommand::ListSkills { id, .. }
        | OutboundCommand::ListHooks { id, .. }
        | OutboundCommand::ListMcpServers { id, .. }
        | OutboundCommand::CallMcpTool { id, .. }
        | OutboundCommand::Shutdown { id } => id.clone(),
    }
}

fn resolve_sidecar() -> anyhow::Result<(String, Vec<String>)> {
    if let Ok(custom) = std::env::var("CLAUDE_APP_SERVER_SIDECAR") {
        // Allow either a path to a .js (we run with node) or any other
        // executable.
        if custom.ends_with(".js") || custom.ends_with(".mjs") {
            return Ok(("node".into(), vec![custom]));
        }
        return Ok((custom, vec![]));
    }

    let mut candidates: Vec<PathBuf> = vec![];
    if let Ok(exe) = std::env::current_exe() {
        // .../claude/target/release/claude-app-server → .../claude/sidecar/dist/index.js
        let mut path = exe.clone();
        for _ in 0..3 {
            if path.pop() {
                let cand = path.join("sidecar/dist/index.js");
                candidates.push(cand);
            }
        }
    }
    candidates.push(PathBuf::from("sidecar/dist/index.js"));
    candidates.push(PathBuf::from("claude/sidecar/dist/index.js"));

    for path in candidates {
        if path.exists() {
            return Ok(("node".into(), vec![path.to_string_lossy().to_string()]));
        }
    }
    bail!(
        "could not locate sidecar; set CLAUDE_APP_SERVER_SIDECAR or build claude/sidecar/dist/index.js"
    )
}
