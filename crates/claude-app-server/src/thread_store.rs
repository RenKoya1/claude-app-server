//! In-memory thread store. Codex persists to sqlite + JSONL rollouts; this MVP
//! keeps everything in RAM and is reset per process. Persistence is a future task.

use chrono::Utc;
use claude_app_server_protocol::{InputChunk, Item, Thread, ThreadStatus, Turn, TurnStatus};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct StoredThread {
    pub thread: Thread,
    pub turns: Vec<Turn>,
    pub model: String,
    pub system_prompt: Option<String>,
    pub cwd: Option<String>,
}

#[derive(Clone, Default)]
pub struct ThreadStore {
    inner: Arc<Mutex<HashMap<String, StoredThread>>>,
}

impl ThreadStore {
    pub fn new() -> Self { Self::default() }

    pub async fn create(
        &self,
        model: String,
        cwd: Option<String>,
        system_prompt: Option<String>,
        ephemeral: bool,
    ) -> Thread {
        let id = format!("thr_{}", Uuid::now_v7());
        let thread = Thread {
            id: id.clone(),
            preview: String::new(),
            model_provider: "anthropic".into(),
            created_at: Utc::now().timestamp(),
            updated_at: Some(Utc::now().timestamp()),
            ephemeral: Some(ephemeral),
            path: None,
            session_id: Some(id.clone()),
            forked_from_id: None,
            status: ThreadStatus::Idle,
            turns: vec![],
        };
        let mut guard = self.inner.lock().await;
        guard.insert(id.clone(), StoredThread {
            thread: thread.clone(),
            turns: vec![],
            model,
            system_prompt,
            cwd,
        });
        thread
    }

    pub async fn get(&self, id: &str) -> Option<StoredThread> {
        let guard = self.inner.lock().await;
        guard.get(id).cloned()
    }

    pub async fn list(&self) -> Vec<Thread> {
        let guard = self.inner.lock().await;
        let mut out: Vec<Thread> = guard.values().map(|s| s.thread.clone()).collect();
        out.sort_by_key(|t| -t.created_at);
        out
    }

    pub async fn fork(&self, source_id: &str, ephemeral: bool) -> Option<Thread> {
        let mut guard = self.inner.lock().await;
        let Some(src) = guard.get(source_id).cloned() else { return None; };
        let new_id = format!("thr_{}", Uuid::now_v7());
        let mut forked = src.thread.clone();
        forked.id = new_id.clone();
        forked.created_at = Utc::now().timestamp();
        forked.updated_at = Some(Utc::now().timestamp());
        forked.forked_from_id = Some(source_id.to_string());
        forked.session_id = Some(new_id.clone());
        forked.ephemeral = Some(ephemeral);
        forked.turns = src.turns.clone();
        let stored = StoredThread {
            thread: forked.clone(),
            turns: src.turns.clone(),
            model: src.model.clone(),
            system_prompt: src.system_prompt.clone(),
            cwd: src.cwd.clone(),
        };
        guard.insert(new_id, stored);
        Some(forked)
    }

    pub async fn archive(&self, id: &str) -> bool {
        let mut guard = self.inner.lock().await;
        guard.remove(id).is_some()
    }

    pub async fn start_turn(&self, thread_id: &str, input: Vec<InputChunk>) -> Option<Turn> {
        let mut guard = self.inner.lock().await;
        let stored = guard.get_mut(thread_id)?;
        let turn_id = format!("turn_{}", Uuid::now_v7());
        let user_item = Item::UserMessage {
            id: turn_id.clone(),
            content: input,
        };
        let turn = Turn {
            id: turn_id,
            status: TurnStatus::InProgress,
            items: vec![user_item],
            error: None,
        };
        stored.turns.push(turn.clone());
        stored.thread.turns = stored.turns.clone();
        stored.thread.status = ThreadStatus::Active { active_flags: vec![] };
        Some(turn)
    }

    pub async fn complete_turn(
        &self,
        thread_id: &str,
        turn_id: &str,
        agent_text: String,
        status: TurnStatus,
    ) -> Option<Turn> {
        let mut guard = self.inner.lock().await;
        let stored = guard.get_mut(thread_id)?;
        let updated_turn = {
            let turn = stored.turns.iter_mut().find(|t| t.id == turn_id)?;
            turn.status = status;
            if !agent_text.is_empty() {
                turn.items.push(Item::AgentMessage {
                    id: format!("msg_{}", Uuid::now_v7()),
                    text: agent_text.clone(),
                });
            }
            turn.clone()
        };
        stored.thread.status = ThreadStatus::Idle;
        stored.thread.preview = agent_text.chars().take(80).collect();
        stored.thread.updated_at = Some(Utc::now().timestamp());
        stored.thread.turns = stored.turns.clone();
        Some(updated_turn)
    }
}
