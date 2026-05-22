//! JSON-RPC dispatcher. Mirrors codex `MessageProcessor`. All model-loop work
//! is delegated to the TypeScript sidecar via `SidecarClient`.

use crate::exec::ExecRegistry;
use crate::fs::{self as fs_ops, FsWatchRegistry};
use crate::outgoing::OutgoingSender;
use crate::sidecar::{
    build_session_options, SidecarClient, SidecarEvent, SidecarSessionOptions, TurnInput,
};
use crate::thread_store::{ThreadStore, UnsubscribeStatus};
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
    fs_watches: FsWatchRegistry,
    exec_registry: ExecRegistry,
}

impl MessageProcessor {
    pub fn new(out: OutgoingSender, sidecar: SidecarClient, default_model: String) -> Self {
        Self::with_store(out, sidecar, default_model, ThreadStore::new())
    }

    pub fn with_store(
        out: OutgoingSender,
        sidecar: SidecarClient,
        default_model: String,
        store: ThreadStore,
    ) -> Self {
        Self {
            out,
            store,
            sidecar,
            initialized: Arc::new(Mutex::new(false)),
            default_model,
            created_sessions: Arc::new(Mutex::new(HashSet::new())),
            fs_watches: FsWatchRegistry::new(),
            exec_registry: ExecRegistry::new(),
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
            // Lifecycle
            proto::request::INITIALIZE => self.handle_initialize(id, params).await,

            // Threads
            proto::request::THREAD_START => self.handle_thread_start(id, params).await,
            proto::request::THREAD_RESUME => self.handle_thread_resume(id, params).await,
            proto::request::THREAD_FORK => self.handle_thread_fork(id, params).await,
            proto::request::THREAD_LIST => self.handle_thread_list(id, params).await,
            proto::request::THREAD_LOADED_LIST => self.handle_thread_loaded_list(id).await,
            proto::request::THREAD_READ => self.handle_thread_read(id, params).await,
            proto::request::THREAD_ARCHIVE => self.handle_thread_archive(id, params).await,
            proto::request::THREAD_UNARCHIVE => self.handle_thread_unarchive(id, params).await,
            proto::request::THREAD_UNSUBSCRIBE => self.handle_thread_unsubscribe(id, params).await,
            proto::request::THREAD_NAME_SET => self.handle_thread_name_set(id, params).await,
            proto::request::THREAD_INJECT_ITEMS => self.handle_thread_inject_items(id, params).await,
            proto::request::THREAD_COMPACT_START => self.handle_thread_compact(id, params).await,
            proto::request::THREAD_GOAL_SET => self.handle_goal_set(id, params).await,
            proto::request::THREAD_GOAL_GET => self.handle_goal_get(id, params).await,
            proto::request::THREAD_GOAL_CLEAR => self.handle_goal_clear(id, params).await,

            // Turns
            proto::request::TURN_START => self.handle_turn_start(id, params).await,
            proto::request::TURN_INTERRUPT => self.handle_turn_interrupt(id, params).await,
            proto::request::TURN_STEER => self.handle_turn_steer(id, params).await,

            // Models / config
            proto::request::MODEL_LIST => self.handle_model_list(id).await,
            proto::request::CONFIG_READ => self.handle_config_read(id).await,

            // Skills / hooks
            proto::request::SKILLS_LIST => self.handle_skills_list(id, params).await,
            proto::request::HOOKS_LIST => self.handle_hooks_list(id, params).await,

            // MCP
            proto::request::MCP_SERVER_STATUS_LIST => self.handle_mcp_status_list(id).await,
            proto::request::MCP_SERVER_TOOL_CALL => self.handle_mcp_tool_call(id, params).await,

            // Filesystem
            proto::request::FS_READ_FILE => self.handle_fs_read_file(id, params).await,
            proto::request::FS_WRITE_FILE => self.handle_fs_write_file(id, params).await,
            proto::request::FS_CREATE_DIRECTORY => self.handle_fs_create_dir(id, params).await,
            proto::request::FS_GET_METADATA => self.handle_fs_metadata(id, params).await,
            proto::request::FS_READ_DIRECTORY => self.handle_fs_read_dir(id, params).await,
            proto::request::FS_REMOVE => self.handle_fs_remove(id, params).await,
            proto::request::FS_COPY => self.handle_fs_copy(id, params).await,
            proto::request::FS_WATCH => self.handle_fs_watch(id, params).await,
            proto::request::FS_UNWATCH => self.handle_fs_unwatch(id, params).await,

            // Command exec
            proto::request::COMMAND_EXEC => self.handle_command_exec(id, params).await,
            proto::request::COMMAND_EXEC_WRITE => self.handle_command_exec_write(id, params).await,
            proto::request::COMMAND_EXEC_TERMINATE => {
                self.handle_command_exec_terminate(id, params).await
            }

            // v0.4.0 additions
            proto::request::THREAD_TURNS_LIST => self.handle_thread_turns_list(id, params).await,
            proto::request::THREAD_TURNS_ITEMS_LIST => {
                self.out
                    .error(id, proto::METHOD_NOT_FOUND, "thread/turns/items/list is not supported yet")
                    .await
            }
            proto::request::THREAD_METADATA_UPDATE => self.handle_thread_metadata_update(id, params).await,
            proto::request::THREAD_SETTINGS_UPDATE => self.handle_thread_settings_update(id, params).await,
            proto::request::THREAD_ROLLBACK => self.handle_thread_rollback(id, params).await,
            proto::request::THREAD_SHELL_COMMAND => self.handle_thread_shell_command(id, params).await,
            proto::request::THREAD_BACKGROUND_TERMINALS_CLEAN => {
                self.handle_thread_bg_terminals_clean(id, params).await
            }
            proto::request::THREAD_MEMORY_MODE_SET => self.handle_thread_memory_mode_set(id, params).await,
            proto::request::MEMORY_RESET => self.handle_memory_reset(id).await,
            proto::request::PERMISSION_PROFILE_LIST => self.handle_permission_profile_list(id).await,
            proto::request::EXPERIMENTAL_FEATURE_LIST => self.handle_experimental_feature_list(id).await,
            proto::request::COLLABORATION_MODE_LIST => self.handle_collaboration_mode_list(id).await,
            proto::request::MODEL_PROVIDER_CAPABILITIES_READ => {
                self.handle_model_provider_capabilities(id).await
            }
            proto::request::REVIEW_START => self.handle_review_start(id, params).await,

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
        let customization: crate::thread_store::ThreadCustomization = p.sdk_options.into_iter().collect();
        let thread = self.store
            .create_with_customization(
                model,
                p.cwd,
                p.system_prompt,
                p.ephemeral.unwrap_or(false),
                customization,
            )
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
        let archived = self.store.archive(&p.thread_id).await;
        self.out.respond_empty(id).await;
        if archived.is_some() {
            self.handle_thread_archive_emit(&p.thread_id).await;
        }
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
            let opts = build_session_options_for(
                &p.model.clone().or(Some(stored.model.clone())),
                &p.cwd.clone().or(stored.cwd.clone()),
                &p.system_prompt.clone().or(stored.system_prompt.clone()),
                &stored.customization,
                &p.sdk_options,
            );
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

    // --- New endpoints (v0.2.0) ----------------------------------------

    async fn handle_thread_loaded_list(&self, id: proto::RequestId) {
        let data = self.store.list_loaded_ids().await;
        self.out
            .respond(id, &proto::ThreadLoadedListResult { data })
            .await;
    }

    async fn handle_thread_unsubscribe(&self, id: proto::RequestId, params: serde_json::Value) {
        let p: proto::ThreadUnsubscribeParams = match serde_json::from_value(params) {
            Ok(v) => v,
            Err(e) => { self.out.error(id, proto::INVALID_PARAMS, format!("{e}")).await; return; }
        };
        let status = self.store.unsubscribe(&p.thread_id).await;
        self.out
            .respond(
                id,
                &proto::ThreadUnsubscribeResult {
                    status: status.as_str().to_string(),
                },
            )
            .await;
        if matches!(status, UnsubscribeStatus::Unsubscribed) {
            // Best-effort: tell the sidecar to drop the session.
            if let Err(e) = self.sidecar.close_session(&p.thread_id).await {
                warn!("sidecar close_session failed: {e}");
            }
            self.created_sessions.lock().await.remove(&p.thread_id);
            self.out
                .notify(
                    proto::notification::THREAD_CLOSED,
                    &serde_json::json!({ "threadId": p.thread_id }),
                )
                .await;
        }
    }

    async fn handle_thread_unarchive(&self, id: proto::RequestId, params: serde_json::Value) {
        let p: proto::ThreadArchiveParams = match serde_json::from_value(params) {
            Ok(v) => v,
            Err(e) => { self.out.error(id, proto::INVALID_PARAMS, format!("{e}")).await; return; }
        };
        let Some(thread) = self.store.unarchive(&p.thread_id).await else {
            self.out.error(id, proto::INTERNAL_ERROR, "Archived thread not found").await;
            return;
        };
        self.out
            .respond(id, &serde_json::json!({ "thread": thread }))
            .await;
        self.out
            .notify(
                proto::notification::THREAD_UNARCHIVED,
                &serde_json::json!({ "threadId": p.thread_id }),
            )
            .await;
    }

    async fn handle_thread_name_set(&self, id: proto::RequestId, params: serde_json::Value) {
        let p: proto::ThreadNameSetParams = match serde_json::from_value(params) {
            Ok(v) => v,
            Err(e) => { self.out.error(id, proto::INVALID_PARAMS, format!("{e}")).await; return; }
        };
        if self.store.set_name(&p.thread_id, p.name.clone()).await.is_none() {
            self.out.error(id, proto::INTERNAL_ERROR, "Thread not found").await;
            return;
        }
        self.out.respond_empty(id).await;
        self.out
            .notify(
                proto::notification::THREAD_NAME_UPDATED,
                &serde_json::json!({ "threadId": p.thread_id, "name": p.name }),
            )
            .await;
    }

    async fn handle_thread_inject_items(
        self: Arc<Self>,
        id: proto::RequestId,
        params: serde_json::Value,
    ) {
        let p: proto::ThreadInjectItemsParams = match serde_json::from_value(params) {
            Ok(v) => v,
            Err(e) => { self.out.error(id, proto::INVALID_PARAMS, format!("{e}")).await; return; }
        };
        if !self.created_sessions.lock().await.contains(&p.thread_id) {
            self.out
                .error(id, proto::INTERNAL_ERROR, "thread has no active sidecar session")
                .await;
            return;
        }
        if let Err(e) = self.sidecar.inject_items(&p.thread_id, p.items).await {
            self.out.error(id, proto::INTERNAL_ERROR, format!("{e}")).await;
            return;
        }
        self.out.respond_empty(id).await;
    }

    async fn handle_thread_compact(
        self: Arc<Self>,
        id: proto::RequestId,
        params: serde_json::Value,
    ) {
        let p: proto::ThreadCompactStartParams = match serde_json::from_value(params) {
            Ok(v) => v,
            Err(e) => { self.out.error(id, proto::INVALID_PARAMS, format!("{e}")).await; return; }
        };
        if !self.created_sessions.lock().await.contains(&p.thread_id) {
            self.out
                .error(id, proto::INTERNAL_ERROR, "thread has no active sidecar session")
                .await;
            return;
        }
        if let Err(e) = self.sidecar.compact(&p.thread_id).await {
            self.out.error(id, proto::INTERNAL_ERROR, format!("{e}")).await;
            return;
        }
        self.out.respond_empty(id).await;
    }

    async fn handle_goal_set(&self, id: proto::RequestId, params: serde_json::Value) {
        let p: proto::ThreadGoalSetParams = match serde_json::from_value(params) {
            Ok(v) => v,
            Err(e) => { self.out.error(id, proto::INVALID_PARAMS, format!("{e}")).await; return; }
        };
        let goal = self
            .store
            .set_goal(&p.thread_id, p.objective, p.status, p.token_budget)
            .await;
        let Some(goal) = goal else {
            self.out.error(id, proto::INTERNAL_ERROR, "Thread not found").await;
            return;
        };
        self.out
            .respond(id, &proto::ThreadGoalResult { goal: Some(goal.clone()) })
            .await;
        self.out
            .notify(
                proto::notification::THREAD_GOAL_UPDATED,
                &serde_json::json!({ "threadId": p.thread_id, "goal": goal }),
            )
            .await;
    }

    async fn handle_goal_get(&self, id: proto::RequestId, params: serde_json::Value) {
        let p: proto::ThreadGoalGetParams = match serde_json::from_value(params) {
            Ok(v) => v,
            Err(e) => { self.out.error(id, proto::INVALID_PARAMS, format!("{e}")).await; return; }
        };
        let goal = self.store.get_goal(&p.thread_id).await;
        self.out.respond(id, &proto::ThreadGoalResult { goal }).await;
    }

    async fn handle_goal_clear(&self, id: proto::RequestId, params: serde_json::Value) {
        let p: proto::ThreadGoalClearParams = match serde_json::from_value(params) {
            Ok(v) => v,
            Err(e) => { self.out.error(id, proto::INVALID_PARAMS, format!("{e}")).await; return; }
        };
        let cleared = self.store.clear_goal(&p.thread_id).await;
        self.out
            .respond(id, &proto::ThreadGoalClearResult { cleared })
            .await;
        if cleared {
            self.out
                .notify(
                    proto::notification::THREAD_GOAL_CLEARED,
                    &serde_json::json!({ "threadId": p.thread_id }),
                )
                .await;
        }
    }

    async fn handle_turn_steer(
        self: Arc<Self>,
        id: proto::RequestId,
        params: serde_json::Value,
    ) {
        let p: proto::TurnSteerParams = match serde_json::from_value(params) {
            Ok(v) => v,
            Err(e) => { self.out.error(id, proto::INVALID_PARAMS, format!("{e}")).await; return; }
        };
        let inputs: Vec<TurnInput> = p.input.into_iter().map(Into::into).collect();
        if let Err(e) = self.sidecar.steer_turn(&p.thread_id, &p.expected_turn_id, inputs).await {
            self.out.error(id, proto::INTERNAL_ERROR, format!("{e}")).await;
            return;
        }
        self.out
            .respond(
                id,
                &proto::TurnSteerResult { turn_id: p.expected_turn_id },
            )
            .await;
    }

    async fn handle_config_read(&self, id: proto::RequestId) {
        let claude_home = std::env::var("CLAUDE_HOME")
            .or_else(|_| std::env::var("HOME").map(|h| format!("{h}/.claude")))
            .unwrap_or_else(|_| ".claude".into());
        let cwd = std::env::current_dir().ok().map(|p| p.to_string_lossy().into_owned());
        let result = proto::ConfigReadResult {
            model: self.default_model.clone(),
            claude_home,
            platform_family: std::env::consts::FAMILY.into(),
            platform_os: std::env::consts::OS.into(),
            cwd,
        };
        self.out.respond(id, &result).await;
    }

    async fn handle_skills_list(self: Arc<Self>, id: proto::RequestId, params: serde_json::Value) {
        let p: proto::SkillsListParams = serde_json::from_value(params).unwrap_or_default();
        match self.sidecar.list_skills(p.cwds).await {
            Ok(value) => self.out.raw_response(id, value).await,
            Err(e) => self.out.error(id, proto::INTERNAL_ERROR, format!("{e}")).await,
        }
    }

    async fn handle_hooks_list(self: Arc<Self>, id: proto::RequestId, params: serde_json::Value) {
        let p: proto::HooksListParams = serde_json::from_value(params).unwrap_or_default();
        match self.sidecar.list_hooks(p.cwds).await {
            Ok(value) => self.out.raw_response(id, value).await,
            Err(e) => self.out.error(id, proto::INTERNAL_ERROR, format!("{e}")).await,
        }
    }

    async fn handle_mcp_status_list(self: Arc<Self>, id: proto::RequestId) {
        match self.sidecar.list_mcp_servers(None).await {
            Ok(value) => self.out.raw_response(id, value).await,
            Err(e) => self.out.error(id, proto::INTERNAL_ERROR, format!("{e}")).await,
        }
    }

    async fn handle_mcp_tool_call(
        self: Arc<Self>,
        id: proto::RequestId,
        params: serde_json::Value,
    ) {
        let p: proto::McpServerToolCallParams = match serde_json::from_value(params) {
            Ok(v) => v,
            Err(e) => { self.out.error(id, proto::INVALID_PARAMS, format!("{e}")).await; return; }
        };
        match self
            .sidecar
            .call_mcp_tool(p.thread_id, p.server, p.tool, p.arguments)
            .await
        {
            Ok(value) => self.out.raw_response(id, value).await,
            Err(e) => self.out.error(id, proto::INTERNAL_ERROR, format!("{e}")).await,
        }
    }

    // --- Filesystem ----------------------------------------------------

    async fn handle_fs_read_file(&self, id: proto::RequestId, params: serde_json::Value) {
        let p: proto::FsReadFileParams = match serde_json::from_value(params) {
            Ok(v) => v,
            Err(e) => { self.out.error(id, proto::INVALID_PARAMS, format!("{e}")).await; return; }
        };
        match fs_ops::read_file(p) {
            Ok(r) => self.out.respond(id, &r).await,
            Err(e) => self.out.error(id, e.error_code(), format!("{e}")).await,
        }
    }

    async fn handle_fs_write_file(&self, id: proto::RequestId, params: serde_json::Value) {
        let p: proto::FsWriteFileParams = match serde_json::from_value(params) {
            Ok(v) => v,
            Err(e) => { self.out.error(id, proto::INVALID_PARAMS, format!("{e}")).await; return; }
        };
        match fs_ops::write_file(p) {
            Ok(()) => self.out.respond_empty(id).await,
            Err(e) => self.out.error(id, e.error_code(), format!("{e}")).await,
        }
    }

    async fn handle_fs_create_dir(&self, id: proto::RequestId, params: serde_json::Value) {
        let p: proto::FsCreateDirectoryParams = match serde_json::from_value(params) {
            Ok(v) => v,
            Err(e) => { self.out.error(id, proto::INVALID_PARAMS, format!("{e}")).await; return; }
        };
        match fs_ops::create_directory(p) {
            Ok(()) => self.out.respond_empty(id).await,
            Err(e) => self.out.error(id, e.error_code(), format!("{e}")).await,
        }
    }

    async fn handle_fs_metadata(&self, id: proto::RequestId, params: serde_json::Value) {
        let p: proto::FsMetadataParams = match serde_json::from_value(params) {
            Ok(v) => v,
            Err(e) => { self.out.error(id, proto::INVALID_PARAMS, format!("{e}")).await; return; }
        };
        match fs_ops::get_metadata(p) {
            Ok(r) => self.out.respond(id, &r).await,
            Err(e) => self.out.error(id, e.error_code(), format!("{e}")).await,
        }
    }

    async fn handle_fs_read_dir(&self, id: proto::RequestId, params: serde_json::Value) {
        let p: proto::FsReadDirectoryParams = match serde_json::from_value(params) {
            Ok(v) => v,
            Err(e) => { self.out.error(id, proto::INVALID_PARAMS, format!("{e}")).await; return; }
        };
        match fs_ops::read_directory(p) {
            Ok(r) => self.out.respond(id, &r).await,
            Err(e) => self.out.error(id, e.error_code(), format!("{e}")).await,
        }
    }

    async fn handle_fs_remove(&self, id: proto::RequestId, params: serde_json::Value) {
        let p: proto::FsRemoveParams = match serde_json::from_value(params) {
            Ok(v) => v,
            Err(e) => { self.out.error(id, proto::INVALID_PARAMS, format!("{e}")).await; return; }
        };
        match fs_ops::remove(p) {
            Ok(()) => self.out.respond_empty(id).await,
            Err(e) => self.out.error(id, e.error_code(), format!("{e}")).await,
        }
    }

    async fn handle_fs_copy(&self, id: proto::RequestId, params: serde_json::Value) {
        let p: proto::FsCopyParams = match serde_json::from_value(params) {
            Ok(v) => v,
            Err(e) => { self.out.error(id, proto::INVALID_PARAMS, format!("{e}")).await; return; }
        };
        match fs_ops::copy(p) {
            Ok(()) => self.out.respond_empty(id).await,
            Err(e) => self.out.error(id, e.error_code(), format!("{e}")).await,
        }
    }

    async fn handle_fs_watch(&self, id: proto::RequestId, params: serde_json::Value) {
        let p: proto::FsWatchParams = match serde_json::from_value(params) {
            Ok(v) => v,
            Err(e) => { self.out.error(id, proto::INVALID_PARAMS, format!("{e}")).await; return; }
        };
        match self.fs_watches.watch(p, self.out.clone()).await {
            Ok(r) => self.out.respond(id, &r).await,
            Err(e) => self.out.error(id, e.error_code(), format!("{e}")).await,
        }
    }

    async fn handle_fs_unwatch(&self, id: proto::RequestId, params: serde_json::Value) {
        let p: proto::FsUnwatchParams = match serde_json::from_value(params) {
            Ok(v) => v,
            Err(e) => { self.out.error(id, proto::INVALID_PARAMS, format!("{e}")).await; return; }
        };
        match self.fs_watches.unwatch(p).await {
            Ok(()) => self.out.respond_empty(id).await,
            Err(e) => self.out.error(id, e.error_code(), format!("{e}")).await,
        }
    }

    // --- Command exec --------------------------------------------------

    async fn handle_command_exec(&self, id: proto::RequestId, params: serde_json::Value) {
        let p: proto::CommandExecParams = match serde_json::from_value(params) {
            Ok(v) => v,
            Err(e) => { self.out.error(id, proto::INVALID_PARAMS, format!("{e}")).await; return; }
        };
        match self.exec_registry.run(p, self.out.clone()).await {
            Ok(r) => self.out.respond(id, &r).await,
            Err(e) => self.out.error(id, e.error_code(), format!("{e}")).await,
        }
    }

    async fn handle_command_exec_write(&self, id: proto::RequestId, params: serde_json::Value) {
        let p: proto::CommandExecWriteParams = match serde_json::from_value(params) {
            Ok(v) => v,
            Err(e) => { self.out.error(id, proto::INVALID_PARAMS, format!("{e}")).await; return; }
        };
        match self.exec_registry.write(p).await {
            Ok(()) => self.out.respond_empty(id).await,
            Err(e) => self.out.error(id, e.error_code(), format!("{e}")).await,
        }
    }

    async fn handle_command_exec_terminate(&self, id: proto::RequestId, params: serde_json::Value) {
        let p: proto::CommandExecTerminateParams = match serde_json::from_value(params) {
            Ok(v) => v,
            Err(e) => { self.out.error(id, proto::INVALID_PARAMS, format!("{e}")).await; return; }
        };
        match self.exec_registry.terminate(p).await {
            Ok(()) => self.out.respond_empty(id).await,
            Err(e) => self.out.error(id, e.error_code(), format!("{e}")).await,
        }
    }

    async fn handle_thread_archive_emit(&self, thread_id: &str) {
        // Emit `thread/archived` (codex parity) after a successful archive.
        self.out
            .notify(
                proto::notification::THREAD_ARCHIVED,
                &serde_json::json!({ "threadId": thread_id }),
            )
            .await;
    }

    // --- v0.4.0 handlers -----------------------------------------------

    async fn handle_thread_turns_list(&self, id: proto::RequestId, params: serde_json::Value) {
        let p: proto::ThreadTurnsListParams = match serde_json::from_value(params) {
            Ok(v) => v,
            Err(e) => { self.out.error(id, proto::INVALID_PARAMS, format!("{e}")).await; return; }
        };
        let Some(stored) = self.store.get(&p.thread_id).await else {
            self.out.error(id, proto::INTERNAL_ERROR, "Thread not found").await;
            return;
        };
        let descending = p.sort_direction.as_deref() != Some("asc");
        let items_view = p.items_view.as_deref().unwrap_or("summary").to_string();
        let limit = p.limit.unwrap_or(50).max(1) as usize;
        let offset = p.cursor.as_deref().and_then(|c| c.parse::<usize>().ok()).unwrap_or(0);
        let total = stored.turns.len();
        let mut all: Vec<_> = stored.turns.clone();
        if descending { all.reverse(); }
        let end = (offset + limit).min(total);
        let slice = &all[offset.min(total)..end];
        let data: Vec<proto::TurnPage> = slice.iter().map(|t| {
            let items = if items_view == "full" {
                t.items.clone()
            } else if items_view == "notLoaded" {
                vec![]
            } else {
                // summary: keep only top-level assistant text + tool use names
                t.items.iter().filter(|i| matches!(
                    i,
                    proto::Item::AgentMessage { .. }
                        | proto::Item::UserMessage { .. }
                        | proto::Item::ToolUse { .. }
                )).cloned().collect()
            };
            proto::TurnPage {
                id: t.id.clone(),
                status: t.status,
                items,
                items_view: items_view.clone(),
            }
        }).collect();
        let next_cursor = if end < total { Some(end.to_string()) } else { None };
        let backwards_cursor = if offset > 0 { Some(offset.to_string()) } else { None };
        self.out
            .respond(id, &proto::ThreadTurnsListResult { data, next_cursor, backwards_cursor })
            .await;
    }

    async fn handle_thread_metadata_update(&self, id: proto::RequestId, params: serde_json::Value) {
        let p: proto::ThreadMetadataUpdateParams = match serde_json::from_value(params) {
            Ok(v) => v,
            Err(e) => { self.out.error(id, proto::INVALID_PARAMS, format!("{e}")).await; return; }
        };
        let Some(thread) = self.store.update_metadata(&p.thread_id, p.git_info).await else {
            self.out.error(id, proto::INTERNAL_ERROR, "Thread not found").await;
            return;
        };
        self.out.respond(id, &serde_json::json!({ "thread": thread })).await;
        self.out
            .notify(
                proto::notification::THREAD_METADATA_UPDATED,
                &serde_json::json!({ "threadId": p.thread_id }),
            )
            .await;
    }

    async fn handle_thread_settings_update(&self, id: proto::RequestId, params: serde_json::Value) {
        let p: proto::ThreadSettingsUpdateParams = match serde_json::from_value(params) {
            Ok(v) => v,
            Err(e) => { self.out.error(id, proto::INVALID_PARAMS, format!("{e}")).await; return; }
        };
        let Some(settings) = self.store.update_settings(&p.thread_id, p.settings).await else {
            self.out.error(id, proto::INTERNAL_ERROR, "Thread not found").await;
            return;
        };
        self.out.respond_empty(id).await;
        self.out
            .notify(
                proto::notification::THREAD_SETTINGS_UPDATED,
                &serde_json::json!({ "threadId": p.thread_id, "threadSettings": settings }),
            )
            .await;
    }

    async fn handle_thread_rollback(&self, id: proto::RequestId, params: serde_json::Value) {
        let p: proto::ThreadRollbackParams = match serde_json::from_value(params) {
            Ok(v) => v,
            Err(e) => { self.out.error(id, proto::INVALID_PARAMS, format!("{e}")).await; return; }
        };
        let Some(thread) = self.store.rollback(&p.thread_id, p.turns).await else {
            self.out.error(id, proto::INTERNAL_ERROR, "Thread not found").await;
            return;
        };
        self.out.respond(id, &serde_json::json!({ "thread": thread })).await;
    }

    async fn handle_thread_shell_command(self: Arc<Self>, id: proto::RequestId, params: serde_json::Value) {
        let p: proto::ThreadShellCommandParams = match serde_json::from_value(params) {
            Ok(v) => v,
            Err(e) => { self.out.error(id, proto::INVALID_PARAMS, format!("{e}")).await; return; }
        };
        let stored = self.store.get(&p.thread_id).await;
        let cwd = stored.as_ref().and_then(|s| s.cwd.clone());
        self.out.respond_empty(id).await;
        let store = self.store.clone();
        let out = self.out.clone();
        let exec = self.exec_registry.clone();
        let thread_id = p.thread_id.clone();
        let command = p.command.clone();
        tokio::spawn(async move {
            // Emit synthetic item flow for the ! command: started → output → completed.
            let item_id = format!("cmd_{}", Uuid::now_v7());
            let user_item = proto::Item::UserMessage {
                id: item_id.clone(),
                content: vec![proto::InputChunk::Text { text: format!("! {command}") }],
            };
            out.item_started(&proto::ItemStartedEvent {
                thread_id: thread_id.clone(),
                turn_id: item_id.clone(),
                item: user_item.clone(),
            }).await;
            let params = proto::CommandExecParams {
                command: vec!["bash".into(), "-lc".into(), command],
                process_id: Some(item_id.clone()),
                cwd,
                env: Default::default(),
                timeout_ms: None,
                disable_timeout: false,
                output_bytes_cap: None,
                disable_output_cap: false,
                stream_stdout_stderr: false,
            };
            let result = exec.run(params, out.clone()).await;
            let completed_item = match &result {
                Ok(r) => proto::Item::AgentMessage {
                    id: format!("msg_{}", Uuid::now_v7()),
                    text: format!("$ exit {}\n{}{}", r.exit_code, r.stdout, r.stderr),
                },
                Err(e) => proto::Item::AgentMessage {
                    id: format!("msg_{}", Uuid::now_v7()),
                    text: format!("$ exec failed: {e}"),
                },
            };
            out.item_completed(&proto::ItemCompletedEvent {
                thread_id: thread_id.clone(),
                turn_id: item_id.clone(),
                item: completed_item.clone(),
            }).await;
            // Persist as a turn if rollouts enabled.
            let _ = store.start_turn(&thread_id, vec![proto::InputChunk::Text { text: "! (shell)".into() }]).await;
        });
    }

    async fn handle_thread_bg_terminals_clean(&self, id: proto::RequestId, params: serde_json::Value) {
        let _p: proto::ThreadBackgroundTerminalsCleanParams = match serde_json::from_value(params) {
            Ok(v) => v,
            Err(e) => { self.out.error(id, proto::INVALID_PARAMS, format!("{e}")).await; return; }
        };
        // The Claude Agent SDK manages its own background tool processes;
        // we cannot reach into them from outside the sidecar today.
        // Acknowledge the request so clients can call it without erroring.
        self.out.respond_empty(id).await;
    }

    async fn handle_thread_memory_mode_set(&self, id: proto::RequestId, params: serde_json::Value) {
        let p: proto::ThreadMemoryModeSetParams = match serde_json::from_value(params) {
            Ok(v) => v,
            Err(e) => { self.out.error(id, proto::INVALID_PARAMS, format!("{e}")).await; return; }
        };
        if !self.store.set_memory_mode(&p.thread_id, p.mode.clone()).await {
            self.out.error(id, proto::INTERNAL_ERROR, "Thread not found").await;
            return;
        }
        self.out.respond_empty(id).await;
        self.out
            .notify(
                proto::notification::THREAD_MEMORY_MODE_CHANGED,
                &serde_json::json!({ "threadId": p.thread_id, "mode": p.mode }),
            )
            .await;
    }

    async fn handle_memory_reset(&self, id: proto::RequestId) {
        let home = std::env::var("CLAUDE_HOME")
            .or_else(|_| std::env::var("HOME").map(|h| format!("{h}/.claude")))
            .unwrap_or_else(|_| ".claude".into());
        let memories_dir = std::path::PathBuf::from(home).join("memories");
        if memories_dir.exists() {
            if let Err(e) = std::fs::remove_dir_all(&memories_dir) {
                self.out
                    .error(id, proto::INTERNAL_ERROR, format!("could not remove memories: {e}"))
                    .await;
                return;
            }
        }
        let _ = std::fs::create_dir_all(&memories_dir);
        self.out.respond_empty(id).await;
    }

    async fn handle_permission_profile_list(&self, id: proto::RequestId) {
        let data = vec![
            proto::PermissionProfileDescriptor {
                id: "default".into(),
                description: Some("Prompt before destructive tool use.".into()),
            },
            proto::PermissionProfileDescriptor {
                id: "acceptEdits".into(),
                description: Some("Auto-approve file edits, prompt for other tools.".into()),
            },
            proto::PermissionProfileDescriptor {
                id: "bypassPermissions".into(),
                description: Some("Skip permission prompts (used by app-server by default).".into()),
            },
            proto::PermissionProfileDescriptor {
                id: "plan".into(),
                description: Some("Plan-only mode; no tool execution.".into()),
            },
            proto::PermissionProfileDescriptor {
                id: "delegate".into(),
                description: Some("Delegate to a subagent's permission decision.".into()),
            },
            proto::PermissionProfileDescriptor {
                id: "dontAsk".into(),
                description: Some("Never ask the user; deny anything not pre-approved.".into()),
            },
        ];
        self.out.respond(id, &proto::PermissionProfileListResult { data }).await;
    }

    async fn handle_experimental_feature_list(&self, id: proto::RequestId) {
        let data = vec![
            proto::ExperimentalFeatureDescriptor {
                id: "websocket_transport".into(),
                stage: "stable".into(),
                enabled: true,
                description: Some("Listen on ws://HOST:PORT for multi-client connections.".into()),
            },
            proto::ExperimentalFeatureDescriptor {
                id: "jsonl_rollouts".into(),
                stage: "stable".into(),
                enabled: true,
                description: Some("Persist threads as JSONL under $CLAUDE_HOME/sessions/.".into()),
            },
            proto::ExperimentalFeatureDescriptor {
                id: "schema_export".into(),
                stage: "beta".into(),
                enabled: true,
                description: Some("Run `generate-ts` / `generate-json-schema` subcommands.".into()),
            },
        ];
        self.out.respond(id, &proto::ExperimentalFeatureListResult { data }).await;
    }

    async fn handle_collaboration_mode_list(&self, id: proto::RequestId) {
        let data = vec![
            proto::CollaborationModeDescriptor {
                id: "code".into(),
                display_name: "Code".into(),
                description: Some("Default Claude Code collaboration mode.".into()),
            },
            proto::CollaborationModeDescriptor {
                id: "plan".into(),
                display_name: "Plan".into(),
                description: Some("Plan-only; no tool execution.".into()),
            },
            proto::CollaborationModeDescriptor {
                id: "review".into(),
                display_name: "Review".into(),
                description: Some("Focused code review with structured findings.".into()),
            },
        ];
        self.out.respond(id, &proto::CollaborationModeListResult { data }).await;
    }

    async fn handle_model_provider_capabilities(&self, id: proto::RequestId) {
        let result = proto::ModelProviderCapabilities {
            name: "anthropic".into(),
            streaming: true,
            tool_use: true,
            vision: true,
            extended_thinking: true,
            prompt_caching: true,
            mcp: true,
        };
        self.out.respond(id, &result).await;
    }

    async fn handle_review_start(self: Arc<Self>, id: proto::RequestId, params: serde_json::Value) {
        let p: proto::ReviewStartParams = match serde_json::from_value(params) {
            Ok(v) => v,
            Err(e) => { self.out.error(id, proto::INVALID_PARAMS, format!("{e}")).await; return; }
        };
        let Some(stored) = self.store.get(&p.thread_id).await else {
            self.out.error(id, proto::INTERNAL_ERROR, "Thread not found").await;
            return;
        };
        let prompt = match &p.target {
            proto::ReviewTarget::UncommittedChanges => {
                "You are reviewing the uncommitted changes in this workspace. Use git status / git diff to read them. Produce a focused review.".to_string()
            }
            proto::ReviewTarget::BaseBranch { branch } => {
                format!("Review the difference between HEAD and the base branch `{branch}`. Use `git diff $(git merge-base HEAD {branch})...HEAD` to see it.")
            }
            proto::ReviewTarget::Commit { sha, title } => match title {
                Some(t) => format!("Review commit {sha} ({t}). Use `git show {sha}`."),
                None => format!("Review commit {sha}. Use `git show {sha}`."),
            },
            proto::ReviewTarget::Custom { instructions } => instructions.clone(),
        };

        let turn = match self.store.start_turn(
            &p.thread_id,
            vec![proto::InputChunk::Text { text: prompt.clone() }],
        ).await {
            Some(t) => t,
            None => {
                self.out.error(id, proto::INTERNAL_ERROR, "Could not start review turn").await;
                return;
            }
        };

        self.out
            .respond(
                id,
                &proto::ReviewStartResult {
                    turn: turn.clone(),
                    review_thread_id: p.thread_id.clone(),
                },
            )
            .await;

        // Emit the entered-review-mode item so clients can render UI state.
        // We piggyback on the existing turn streaming loop by issuing a
        // turn through the sidecar with the review prompt.
        let needs_create = {
            let mut set = self.created_sessions.lock().await;
            if set.contains(&p.thread_id) { false } else {
                set.insert(p.thread_id.clone());
                true
            }
        };
        if needs_create {
            let opts = build_session_options_for(
                &Some(stored.model.clone()),
                &stored.cwd.clone(),
                &stored.system_prompt.clone(),
                &stored.customization,
                &std::collections::HashMap::new(),
            );
            if let Err(e) = self.sidecar.create_session(&p.thread_id, Some(opts)).await {
                tracing::warn!("review create_session failed: {e}");
                return;
            }
        }
        let _ = self
            .sidecar
            .turn(
                &p.thread_id,
                &turn.id,
                vec![crate::sidecar::TurnInput::Text { text: prompt }],
            )
            .await;
    }
}

/// Compose a `SidecarSessionOptions` blob.
///
/// Layering (last write wins):
///   1. ergonomic typed defaults (model, cwd, systemPrompt, permissionMode,
///      includePartialMessages)
///   2. the thread's stored customization (from `thread/start sdk_options`)
///   3. the per-turn `sdk_options` map (from `turn/start sdk_options`)
fn build_session_options_for(
    model: &Option<String>,
    cwd: &Option<String>,
    system_prompt: &Option<String>,
    thread_customization: &serde_json::Map<String, serde_json::Value>,
    per_turn_overrides: &std::collections::HashMap<String, serde_json::Value>,
) -> SidecarSessionOptions {
    let mut typed: Vec<(&'static str, serde_json::Value)> = vec![
        ("permissionMode", serde_json::Value::String("bypassPermissions".into())),
        ("includePartialMessages", serde_json::Value::Bool(true)),
    ];
    if let Some(m) = model {
        typed.push(("model", serde_json::Value::String(m.clone())));
    }
    if let Some(c) = cwd {
        typed.push(("cwd", serde_json::Value::String(c.clone())));
    }
    if let Some(sp) = system_prompt {
        typed.push(("systemPrompt", serde_json::Value::String(sp.clone())));
    } else {
        // `null` → sidecar uses the claude_code preset.
        typed.push(("systemPrompt", serde_json::Value::Null));
    }
    let mut merged = build_session_options(typed, thread_customization.clone());
    for (k, v) in per_turn_overrides {
        merged.insert(k.clone(), v.clone());
    }
    merged
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
