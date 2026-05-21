//! JSON-RPC dispatcher. Mirrors codex `MessageProcessor`. All model-loop work
//! is delegated to the TypeScript sidecar via `SidecarClient`.

use crate::outgoing::OutgoingSender;
use crate::sidecar::{SidecarClient, SidecarEvent, SidecarSessionOptions, TurnInput};
use crate::thread_store::ThreadStore;
use claude_app_server_protocol as proto;
use proto::{
    ItemAgentMessageDeltaEvent, ItemCompletedEvent, ItemStartedEvent, JsonRpcMessage,
    JsonRpcRequest, ModelDescriptor, ModelListResult, ThreadListResult, ThreadReadResult,
    ThreadStartResult, ThreadStartedEvent, ThreadStatus, ThreadStatusChangedEvent,
    TurnCompletedEvent, TurnStartResult, TurnStartedEvent, TurnStatus,
};
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::warn;
use uuid::Uuid;

pub struct MessageProcessor {
    out: OutgoingSender,
    store: ThreadStore,
    sidecar: SidecarClient,
    initialized: Arc<Mutex<bool>>,
    default_model: String,
    /// Sidecar sessions we've created. We lazily create one the first time a
    /// thread runs a turn, and reuse it for the thread's lifetime.
    created_sessions: Arc<Mutex<HashSet<String>>>,
}

impl MessageProcessor {
    pub fn new(out: OutgoingSender, sidecar: SidecarClient, default_model: String) -> Self {
        Self {
            out,
            store: ThreadStore::new(),
            sidecar,
            initialized: Arc::new(Mutex::new(false)),
            default_model,
            created_sessions: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    pub async fn handle(self: Arc<Self>, msg: JsonRpcMessage) {
        match msg {
            JsonRpcMessage::Request(req) => self.clone().dispatch_request(req).await,
            JsonRpcMessage::Notification(n) => {
                if n.method == proto::notification::INITIALIZED {
                    *self.initialized.lock().await = true;
                }
            }
            JsonRpcMessage::Response(_) | JsonRpcMessage::Error(_) => {
                warn!("server received response message; ignoring");
            }
        }
    }

    async fn dispatch_request(self: Arc<Self>, req: JsonRpcRequest) {
        let id = req.id.clone();
        let method = req.method.as_str();

        if method != proto::request::INITIALIZE {
            if !*self.initialized.lock().await {
                self.out.error(id, proto::INTERNAL_ERROR, "Not initialized").await;
                return;
            }
        }

        let params = req.params.clone().unwrap_or(serde_json::Value::Null);

        match method {
            proto::request::INITIALIZE => self.handle_initialize(id, params).await,
            proto::request::THREAD_START => self.handle_thread_start(id, params).await,
            proto::request::THREAD_RESUME => self.handle_thread_resume(id, params).await,
            proto::request::THREAD_FORK => self.handle_thread_fork(id, params).await,
            proto::request::THREAD_LIST => self.handle_thread_list(id, params).await,
            proto::request::THREAD_READ => self.handle_thread_read(id, params).await,
            proto::request::THREAD_ARCHIVE => self.handle_thread_archive(id, params).await,
            proto::request::TURN_START => self.handle_turn_start(id, params).await,
            proto::request::TURN_INTERRUPT => self.handle_turn_interrupt(id, params).await,
            proto::request::MODEL_LIST => self.handle_model_list(id).await,
            other => {
                self.out
                    .error(
                        id,
                        proto::METHOD_NOT_FOUND,
                        format!("Method not supported: {other}"),
                    )
                    .await;
            }
        }
    }

    async fn handle_initialize(&self, id: proto::RequestId, params: serde_json::Value) {
        let mut init = self.initialized.lock().await;
        if *init {
            self.out.error(id, proto::ALREADY_INITIALIZED, "Already initialized").await;
            return;
        }
        let _parsed: proto::InitializeParams = match serde_json::from_value(params) {
            Ok(p) => p,
            Err(e) => {
                self.out.error(id, proto::INVALID_PARAMS, format!("{e}")).await;
                return;
            }
        };
        let claude_home = std::env::var("CLAUDE_HOME")
            .or_else(|_| std::env::var("HOME").map(|h| format!("{h}/.claude")))
            .unwrap_or_else(|_| ".claude".into());
        let result = proto::InitializeResult {
            user_agent: "claude-app-server/0.1".into(),
            codex_home: claude_home,
            platform_family: std::env::consts::FAMILY.into(),
            platform_os: std::env::consts::OS.into(),
        };
        *init = true;
        drop(init);
        self.out.respond(id, &result).await;
    }

    async fn handle_thread_start(&self, id: proto::RequestId, params: serde_json::Value) {
        let p: proto::ThreadStartParams = match serde_json::from_value(params) {
            Ok(p) => p,
            Err(e) => {
                self.out.error(id, proto::INVALID_PARAMS, format!("{e}")).await;
                return;
            }
        };
        let model = p.model.unwrap_or_else(|| self.default_model.clone());
        let thread = self.store
            .create(model, p.cwd, p.system_prompt, p.ephemeral.unwrap_or(false))
            .await;
        let result = ThreadStartResult { thread: thread.clone() };
        self.out.respond(id, &result).await;
        self.out
            .thread_started(&ThreadStartedEvent { thread })
            .await;
    }

    async fn handle_thread_resume(&self, id: proto::RequestId, params: serde_json::Value) {
        let p: proto::ThreadResumeParams = match serde_json::from_value(params) {
            Ok(p) => p,
            Err(e) => {
                self.out.error(id, proto::INVALID_PARAMS, format!("{e}")).await;
                return;
            }
        };
        let Some(mut stored) = self.store.get(&p.thread_id).await else {
            self.out.error(id, proto::INTERNAL_ERROR, "Thread not found").await;
            return;
        };
        if p.exclude_turns.unwrap_or(false) {
            stored.thread.turns = vec![];
        } else {
            stored.thread.turns = stored.turns.clone();
        }
        let result = ThreadStartResult { thread: stored.thread.clone() };
        self.out.respond(id, &result).await;
        self.out
            .thread_started(&ThreadStartedEvent { thread: stored.thread })
            .await;
    }

    async fn handle_thread_fork(&self, id: proto::RequestId, params: serde_json::Value) {
        let p: proto::ThreadForkParams = match serde_json::from_value(params) {
            Ok(p) => p,
            Err(e) => {
                self.out.error(id, proto::INVALID_PARAMS, format!("{e}")).await;
                return;
            }
        };
        let Some(thread) = self.store.fork(&p.thread_id, p.ephemeral.unwrap_or(false)).await else {
            self.out.error(id, proto::INTERNAL_ERROR, "Source thread not found").await;
            return;
        };
        let result = ThreadStartResult { thread: thread.clone() };
        self.out.respond(id, &result).await;
        self.out.thread_started(&ThreadStartedEvent { thread }).await;
    }

    async fn handle_thread_list(&self, id: proto::RequestId, _params: serde_json::Value) {
        let data = self.store.list().await;
        let result = ThreadListResult { data, next_cursor: None, backwards_cursor: None };
        self.out.respond(id, &result).await;
    }

    async fn handle_thread_read(&self, id: proto::RequestId, params: serde_json::Value) {
        let p: proto::ThreadReadParams = match serde_json::from_value(params) {
            Ok(p) => p,
            Err(e) => {
                self.out.error(id, proto::INVALID_PARAMS, format!("{e}")).await;
                return;
            }
        };
        let Some(stored) = self.store.get(&p.thread_id).await else {
            self.out.error(id, proto::INTERNAL_ERROR, "Thread not found").await;
            return;
        };
        let mut thread = stored.thread.clone();
        if !p.include_turns {
            thread.turns = vec![];
        }
        let result = ThreadReadResult { thread };
        self.out.respond(id, &result).await;
    }

    async fn handle_thread_archive(&self, id: proto::RequestId, params: serde_json::Value) {
        let p: proto::ThreadArchiveParams = match serde_json::from_value(params) {
            Ok(p) => p,
            Err(e) => {
                self.out.error(id, proto::INVALID_PARAMS, format!("{e}")).await;
                return;
            }
        };
        // Best-effort: ask the sidecar to drop its session too.
        if self.created_sessions.lock().await.remove(&p.thread_id) {
            if let Err(e) = self.sidecar.close_session(&p.thread_id).await {
                warn!("could not close sidecar session for {}: {e}", p.thread_id);
            }
        }
        let _ = self.store.archive(&p.thread_id).await;
        self.out.respond_empty(id).await;
    }

    async fn handle_model_list(&self, id: proto::RequestId) {
        let data = vec![
            ModelDescriptor {
                id: "claude-opus-4-7".into(),
                display_name: "Claude Opus 4.7".into(),
                hidden: false,
            },
            ModelDescriptor {
                id: "claude-sonnet-4-6".into(),
                display_name: "Claude Sonnet 4.6".into(),
                hidden: false,
            },
            ModelDescriptor {
                id: "claude-haiku-4-5-20251001".into(),
                display_name: "Claude Haiku 4.5".into(),
                hidden: false,
            },
        ];
        self.out.respond(id, &ModelListResult { data }).await;
    }

    async fn handle_turn_interrupt(&self, id: proto::RequestId, params: serde_json::Value) {
        let p: proto::TurnInterruptParams = match serde_json::from_value(params) {
            Ok(p) => p,
            Err(e) => {
                self.out.error(id, proto::INVALID_PARAMS, format!("{e}")).await;
                return;
            }
        };
        match self.sidecar.interrupt(&p.thread_id, &p.turn_id).await {
            Ok(()) => self.out.respond_empty(id).await,
            Err(e) => self.out.error(id, proto::INTERNAL_ERROR, format!("interrupt failed: {e}")).await,
        }
    }

    async fn handle_turn_start(self: Arc<Self>, id: proto::RequestId, params: serde_json::Value) {
        let p: proto::TurnStartParams = match serde_json::from_value(params) {
            Ok(p) => p,
            Err(e) => {
                self.out.error(id, proto::INVALID_PARAMS, format!("{e}")).await;
                return;
            }
        };
        let Some(stored) = self.store.get(&p.thread_id).await else {
            self.out.error(id, proto::INTERNAL_ERROR, "Thread not found").await;
            return;
        };

        // Lazily create the sidecar session.
        let needs_create = {
            let mut set = self.created_sessions.lock().await;
            if set.contains(&p.thread_id) { false } else {
                set.insert(p.thread_id.clone());
                true
            }
        };
        if needs_create {
            let model = p.model.clone().unwrap_or(stored.model.clone());
            let system_prompt = p.system_prompt.clone().or(stored.system_prompt.clone());
            let opts = SidecarSessionOptions {
                cwd: p.cwd.clone().or(stored.cwd.clone()),
                model: Some(model),
                system_prompt: Some(system_prompt),
                permission_mode: Some("bypassPermissions".into()),
                allowed_tools: None,
                disallowed_tools: None,
                max_turns: None,
                include_partial_messages: Some(true),
                resume: None,
            };
            if let Err(e) = self.sidecar.create_session(&p.thread_id, Some(opts)).await {
                self.created_sessions.lock().await.remove(&p.thread_id);
                self.out
                    .error(id, proto::INTERNAL_ERROR, format!("sidecar create_session failed: {e}"))
                    .await;
                return;
            }
        }

        let Some(turn) = self.store.start_turn(&p.thread_id, p.input.clone()).await else {
            self.out.error(id, proto::INTERNAL_ERROR, "Could not start turn").await;
            return;
        };

        let result = TurnStartResult { turn: turn.clone() };
        self.out.respond(id, &result).await;
        self.out
            .turn_started(&TurnStartedEvent {
                thread_id: p.thread_id.clone(),
                turn: turn.clone(),
            })
            .await;
        self.out
            .thread_status_changed(&ThreadStatusChangedEvent {
                thread_id: p.thread_id.clone(),
                status: ThreadStatus::Active { active_flags: vec![] },
            })
            .await;

        // Subscribe to sidecar events BEFORE issuing the turn so we don't
        // miss the first delta.
        let mut events = self.sidecar.subscribe();
        let inputs: Vec<TurnInput> = p.input.clone().into_iter().map(Into::into).collect();
        if let Err(e) = self.sidecar.turn(&p.thread_id, &turn.id, inputs).await {
            self.out
                .error(
                    proto::RequestId::String(format!("turn-fail-{}", turn.id)),
                    proto::INTERNAL_ERROR,
                    format!("sidecar turn failed: {e}"),
                )
                .await;
            // Still close out the turn so the client doesn't hang.
            self.complete_turn(&p.thread_id, &turn.id, String::new(), TurnStatus::Failed, None)
                .await;
            return;
        }

        let store = self.store.clone();
        let out = self.out.clone();
        let thread_id = p.thread_id.clone();
        let turn_id = turn.id.clone();

        tokio::spawn(async move {
            let mut last_text = String::new();
            loop {
                let event = match events.recv().await {
                    Ok(e) => e,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        warn!("sidecar event subscriber lagged by {n}");
                        continue;
                    }
                    Err(_) => break,
                };
                match event {
                    SidecarEvent::AssistantDelta {
                        session_id, turn_id: t, item_id, delta,
                    } if session_id == thread_id && t == turn_id => {
                        out.item_agent_message_delta(&ItemAgentMessageDeltaEvent {
                            thread_id: thread_id.clone(),
                            turn_id: turn_id.clone(),
                            item_id,
                            delta,
                        })
                        .await;
                    }
                    SidecarEvent::AssistantMessage {
                        session_id, turn_id: t, item_id, text,
                    } if session_id == thread_id && t == turn_id => {
                        last_text = text.clone();
                        out.item_completed(&ItemCompletedEvent {
                            thread_id: thread_id.clone(),
                            turn_id: turn_id.clone(),
                            item: proto::Item::AgentMessage { id: item_id, text },
                        })
                        .await;
                    }
                    SidecarEvent::ToolUse {
                        session_id, turn_id: t, tool_use_id, name, input,
                    } if session_id == thread_id && t == turn_id => {
                        let item = proto::Item::ToolUse {
                            id: format!("tu_{}", Uuid::now_v7()),
                            tool_use_id,
                            name,
                            input,
                        };
                        out.item_started(&ItemStartedEvent {
                            thread_id: thread_id.clone(),
                            turn_id: turn_id.clone(),
                            item: item.clone(),
                        })
                        .await;
                        out.item_completed(&ItemCompletedEvent {
                            thread_id: thread_id.clone(),
                            turn_id: turn_id.clone(),
                            item,
                        })
                        .await;
                    }
                    SidecarEvent::ToolResult {
                        session_id, turn_id: t, tool_use_id, content, is_error,
                    } if session_id == thread_id && t == turn_id => {
                        let item = proto::Item::ToolResult {
                            id: format!("tr_{}", Uuid::now_v7()),
                            tool_use_id,
                            content,
                            is_error,
                        };
                        out.item_started(&ItemStartedEvent {
                            thread_id: thread_id.clone(),
                            turn_id: turn_id.clone(),
                            item: item.clone(),
                        })
                        .await;
                        out.item_completed(&ItemCompletedEvent {
                            thread_id: thread_id.clone(),
                            turn_id: turn_id.clone(),
                            item,
                        })
                        .await;
                    }
                    SidecarEvent::Reasoning {
                        session_id, turn_id: t, item_id, text,
                    } if session_id == thread_id && t == turn_id => {
                        let item = proto::Item::Reasoning { id: item_id, text };
                        out.item_completed(&ItemCompletedEvent {
                            thread_id: thread_id.clone(),
                            turn_id: turn_id.clone(),
                            item,
                        })
                        .await;
                    }
                    SidecarEvent::TurnCompleted {
                        session_id, turn_id: t, is_error, result, errors, usage, ..
                    } if session_id == thread_id && t == turn_id => {
                        let status = if is_error { TurnStatus::Failed } else { TurnStatus::Completed };
                        let final_text = result
                            .or_else(|| if !last_text.is_empty() { Some(last_text.clone()) } else { None })
                            .unwrap_or_default();
                        let token_usage = usage.and_then(parse_usage);
                        let completed_turn = store
                            .complete_turn(&thread_id, &turn_id, final_text.clone(), status)
                            .await
                            .unwrap_or_else(|| proto::Turn {
                                id: turn_id.clone(),
                                status,
                                items: vec![],
                                error: errors.as_ref().and_then(|v| v.first().cloned()),
                            });
                        out.turn_completed(&TurnCompletedEvent {
                            thread_id: thread_id.clone(),
                            turn: completed_turn,
                            token_usage,
                        })
                        .await;
                        out.thread_status_changed(&ThreadStatusChangedEvent {
                            thread_id: thread_id.clone(),
                            status: ThreadStatus::Idle,
                        })
                        .await;
                        break;
                    }
                    _ => {}
                }
            }
        });
    }

    async fn complete_turn(
        &self,
        thread_id: &str,
        turn_id: &str,
        text: String,
        status: TurnStatus,
        token_usage: Option<proto::TokenUsage>,
    ) {
        let completed = self
            .store
            .complete_turn(thread_id, turn_id, text, status)
            .await
            .unwrap_or_else(|| proto::Turn {
                id: turn_id.to_string(),
                status,
                items: vec![],
                error: None,
            });
        self.out
            .turn_completed(&TurnCompletedEvent {
                thread_id: thread_id.to_string(),
                turn: completed,
                token_usage,
            })
            .await;
        self.out
            .thread_status_changed(&ThreadStatusChangedEvent {
                thread_id: thread_id.to_string(),
                status: ThreadStatus::Idle,
            })
            .await;
    }
}

fn parse_usage(value: serde_json::Value) -> Option<proto::TokenUsage> {
    let obj = value.as_object()?;
    let input = obj.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
    let output = obj.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
    Some(proto::TokenUsage {
        input_tokens: input,
        output_tokens: output,
        cache_read_input_tokens: obj.get("cache_read_input_tokens").and_then(|v| v.as_u64()),
        cache_creation_input_tokens: obj.get("cache_creation_input_tokens").and_then(|v| v.as_u64()),
    })
}
