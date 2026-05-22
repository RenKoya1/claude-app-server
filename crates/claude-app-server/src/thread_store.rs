//! In-memory thread + goal + archive store. Codex persists to sqlite + JSONL
//! rollouts; this MVP keeps everything in RAM and is reset per process.
//! Persistence is a future task.

use chrono::Utc;
use claude_app_server_protocol::{
    InputChunk, Item, Thread, ThreadGoal, ThreadStatus, Turn, TurnStatus,
};
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
    pub name: Option<String>,
    pub goal: Option<ThreadGoal>,
    pub subscribed: bool,
}

#[derive(Clone, Default)]
pub struct ThreadStore {
    inner: Arc<Mutex<Inner>>,
}

#[derive(Default)]
struct Inner {
    threads: HashMap<String, StoredThread>,
    archived: HashMap<String, StoredThread>,
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
        guard.threads.insert(id.clone(), StoredThread {
            thread: thread.clone(),
            turns: vec![],
            model,
            system_prompt,
            cwd,
            name: None,
            goal: None,
            subscribed: true,
        });
        thread
    }

    pub async fn get(&self, id: &str) -> Option<StoredThread> {
        let guard = self.inner.lock().await;
        guard.threads.get(id).cloned()
    }

    pub async fn list(&self) -> Vec<Thread> {
        let guard = self.inner.lock().await;
        let mut out: Vec<Thread> = guard.threads.values().map(|s| s.thread.clone()).collect();
        out.sort_by_key(|t| -t.created_at);
        out
    }

    pub async fn list_loaded_ids(&self) -> Vec<String> {
        let guard = self.inner.lock().await;
        guard.threads.keys().cloned().collect()
    }

    pub async fn fork(&self, source_id: &str, ephemeral: bool) -> Option<Thread> {
        let mut guard = self.inner.lock().await;
        let Some(src) = guard.threads.get(source_id).cloned() else { return None; };
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
            name: None,
            goal: None,
            subscribed: true,
        };
        guard.threads.insert(new_id, stored);
        Some(forked)
    }

    pub async fn archive(&self, id: &str) -> Option<Thread> {
        let mut guard = self.inner.lock().await;
        let Some(stored) = guard.threads.remove(id) else { return None; };
        let thread = stored.thread.clone();
        guard.archived.insert(id.to_string(), stored);
        Some(thread)
    }

    pub async fn unarchive(&self, id: &str) -> Option<Thread> {
        let mut guard = self.inner.lock().await;
        let Some(stored) = guard.archived.remove(id) else { return None; };
        let thread = stored.thread.clone();
        guard.threads.insert(id.to_string(), stored);
        Some(thread)
    }

    pub async fn set_name(&self, id: &str, name: String) -> Option<String> {
        let mut guard = self.inner.lock().await;
        let stored = guard.threads.get_mut(id)?;
        stored.name = Some(name.clone());
        stored.thread.updated_at = Some(Utc::now().timestamp());
        Some(name)
    }

    pub async fn unsubscribe(&self, id: &str) -> UnsubscribeStatus {
        let mut guard = self.inner.lock().await;
        let Some(stored) = guard.threads.get_mut(id) else { return UnsubscribeStatus::NotLoaded; };
        if !stored.subscribed {
            return UnsubscribeStatus::NotSubscribed;
        }
        stored.subscribed = false;
        UnsubscribeStatus::Unsubscribed
    }

    pub async fn set_goal(
        &self,
        thread_id: &str,
        objective: Option<String>,
        status: Option<String>,
        token_budget: Option<u64>,
    ) -> Option<ThreadGoal> {
        let mut guard = self.inner.lock().await;
        let stored = guard.threads.get_mut(thread_id)?;
        let now = Utc::now().timestamp();
        let goal = stored.goal.get_or_insert(ThreadGoal {
            thread_id: thread_id.to_string(),
            objective: String::new(),
            status: "active".into(),
            token_budget: None,
            tokens_used: 0,
            time_used_seconds: 0,
            created_at: now,
            updated_at: now,
        });
        if let Some(o) = objective { goal.objective = o; }
        if let Some(s) = status { goal.status = s; }
        if token_budget.is_some() { goal.token_budget = token_budget; }
        goal.updated_at = now;
        Some(goal.clone())
    }

    pub async fn get_goal(&self, thread_id: &str) -> Option<ThreadGoal> {
        let guard = self.inner.lock().await;
        guard.threads.get(thread_id)?.goal.clone()
    }

    pub async fn clear_goal(&self, thread_id: &str) -> bool {
        let mut guard = self.inner.lock().await;
        let Some(stored) = guard.threads.get_mut(thread_id) else { return false; };
        if stored.goal.take().is_some() {
            stored.thread.updated_at = Some(Utc::now().timestamp());
            true
        } else {
            false
        }
    }

    pub async fn start_turn(&self, thread_id: &str, input: Vec<InputChunk>) -> Option<Turn> {
        let mut guard = self.inner.lock().await;
        let stored = guard.threads.get_mut(thread_id)?;
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
        let stored = guard.threads.get_mut(thread_id)?;
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

#[derive(Debug, Clone, Copy)]
pub enum UnsubscribeStatus {
    Unsubscribed,
    NotSubscribed,
    NotLoaded,
}

impl UnsubscribeStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Unsubscribed => "unsubscribed",
            Self::NotSubscribed => "notSubscribed",
            Self::NotLoaded => "notLoaded",
        }
    }
}
