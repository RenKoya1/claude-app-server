//! Thread + goal + archive state. Backed by an in-memory hash map for hot
//! access and (optionally) by `RolloutStore` for JSONL persistence so the
//! server survives restarts the same way `codex app-server` does.

use crate::rollouts::{RolloutEvent, RolloutStore};
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
    pub git_info: Option<claude_app_server_protocol::GitInfo>,
    pub memory_mode: Option<String>,
    pub settings: claude_app_server_protocol::ThreadSettings,
}

#[derive(Clone, Default)]
pub struct ThreadStore {
    inner: Arc<Mutex<Inner>>,
    rollouts: Option<RolloutStore>,
}

#[derive(Default)]
struct Inner {
    threads: HashMap<String, StoredThread>,
    archived: HashMap<String, StoredThread>,
}

impl ThreadStore {
    pub fn new() -> Self { Self::default() }

    pub fn with_rollouts(rollouts: RolloutStore) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner::default())),
            rollouts: Some(rollouts),
        }
    }

    /// Rebuild in-memory state from rollout files. Call once at startup
    /// after constructing the store. Threads come back in `notLoaded`
    /// status; the first `thread/resume` will lift them to `idle`.
    pub async fn replay_rollouts(&self) {
        let Some(rollouts) = &self.rollouts else { return; };
        let rebuilt = match rollouts.replay_all() {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("rollout replay failed: {e}");
                return;
            }
        };
        let mut guard = self.inner.lock().await;
        for r in rebuilt {
            let stored = StoredThread {
                thread: r.thread.clone(),
                turns: r.turns,
                model: r.model,
                system_prompt: r.system_prompt,
                cwd: r.cwd,
                name: r.name,
                goal: None,
                subscribed: false,
                git_info: None,
                memory_mode: None,
                settings: Default::default(),
            };
            if r.archived {
                guard.archived.insert(r.thread.id.clone(), stored);
            } else {
                guard.threads.insert(r.thread.id.clone(), stored);
            }
        }
    }

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
            path: self
                .rollouts
                .as_ref()
                .filter(|_| !ephemeral)
                .map(|r| r.thread_path(&id).to_string_lossy().into_owned()),
            session_id: Some(id.clone()),
            forked_from_id: None,
            status: ThreadStatus::Idle,
            turns: vec![],
        };
        let mut guard = self.inner.lock().await;
        guard.threads.insert(id.clone(), StoredThread {
            thread: thread.clone(),
            turns: vec![],
            model: model.clone(),
            system_prompt: system_prompt.clone(),
            cwd: cwd.clone(),
            name: None,
            goal: None,
            subscribed: true,
            git_info: None,
            memory_mode: None,
            settings: Default::default(),
        });
        if !ephemeral {
            if let Some(rollouts) = &self.rollouts {
                rollouts
                    .append(&id, RolloutEvent::ThreadCreated {
                        thread: thread.clone(),
                        model,
                        system_prompt,
                        cwd,
                        ts: RolloutStore::now(),
                    })
                    .await;
            }
        }
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
        forked.path = self
            .rollouts
            .as_ref()
            .filter(|_| !ephemeral)
            .map(|r| r.thread_path(&new_id).to_string_lossy().into_owned());
        let stored = StoredThread {
            thread: forked.clone(),
            turns: src.turns.clone(),
            model: src.model.clone(),
            system_prompt: src.system_prompt.clone(),
            cwd: src.cwd.clone(),
            name: None,
            goal: None,
            subscribed: true,
            git_info: None,
            memory_mode: None,
            settings: Default::default(),
        };
        guard.threads.insert(new_id.clone(), stored);
        drop(guard);
        if !ephemeral {
            if let Some(rollouts) = &self.rollouts {
                rollouts
                    .append(&new_id, RolloutEvent::ThreadCreated {
                        thread: forked.clone(),
                        model: src.model,
                        system_prompt: src.system_prompt,
                        cwd: src.cwd,
                        ts: RolloutStore::now(),
                    })
                    .await;
                // Copy the source's history so resumes look identical.
                for turn in &src.turns {
                    let user_input: Vec<InputChunk> = turn
                        .items
                        .iter()
                        .filter_map(|item| match item {
                            Item::UserMessage { content, .. } => Some(content.clone()),
                            _ => None,
                        })
                        .flatten()
                        .collect();
                    rollouts
                        .append(&new_id, RolloutEvent::TurnStarted {
                            turn_id: turn.id.clone(),
                            input: user_input,
                            ts: RolloutStore::now(),
                        })
                        .await;
                    for item in &turn.items {
                        if matches!(item, Item::UserMessage { .. }) { continue; }
                        rollouts
                            .append(&new_id, RolloutEvent::ItemAppended {
                                turn_id: turn.id.clone(),
                                item: item.clone(),
                                ts: RolloutStore::now(),
                            })
                            .await;
                    }
                    rollouts
                        .append(&new_id, RolloutEvent::TurnCompleted {
                            turn_id: turn.id.clone(),
                            status: turn.status,
                            ts: RolloutStore::now(),
                        })
                        .await;
                }
            }
        }
        Some(forked)
    }

    pub async fn archive(&self, id: &str) -> Option<Thread> {
        let mut guard = self.inner.lock().await;
        let Some(stored) = guard.threads.remove(id) else { return None; };
        let thread = stored.thread.clone();
        guard.archived.insert(id.to_string(), stored);
        drop(guard);
        if let Some(rollouts) = &self.rollouts {
            rollouts
                .append(id, RolloutEvent::Archived { ts: RolloutStore::now() })
                .await;
        }
        Some(thread)
    }

    pub async fn unarchive(&self, id: &str) -> Option<Thread> {
        let mut guard = self.inner.lock().await;
        let Some(stored) = guard.archived.remove(id) else { return None; };
        let thread = stored.thread.clone();
        guard.threads.insert(id.to_string(), stored);
        drop(guard);
        if let Some(rollouts) = &self.rollouts {
            rollouts
                .append(id, RolloutEvent::Unarchived { ts: RolloutStore::now() })
                .await;
        }
        Some(thread)
    }

    pub async fn set_name(&self, id: &str, name: String) -> Option<String> {
        let mut guard = self.inner.lock().await;
        let stored = guard.threads.get_mut(id)?;
        stored.name = Some(name.clone());
        stored.thread.updated_at = Some(Utc::now().timestamp());
        drop(guard);
        if let Some(rollouts) = &self.rollouts {
            rollouts
                .append(id, RolloutEvent::NameSet { name: name.clone(), ts: RolloutStore::now() })
                .await;
        }
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

    pub async fn rollback(&self, thread_id: &str, n: u32) -> Option<Thread> {
        let mut guard = self.inner.lock().await;
        let stored = guard.threads.get_mut(thread_id)?;
        let drop_count = (n as usize).min(stored.turns.len());
        let new_len = stored.turns.len() - drop_count;
        stored.turns.truncate(new_len);
        stored.thread.turns = stored.turns.clone();
        stored.thread.updated_at = Some(Utc::now().timestamp());
        Some(stored.thread.clone())
    }

    pub async fn update_metadata(
        &self,
        thread_id: &str,
        git_info: Option<claude_app_server_protocol::GitInfo>,
    ) -> Option<Thread> {
        let mut guard = self.inner.lock().await;
        let stored = guard.threads.get_mut(thread_id)?;
        if let Some(gi) = git_info {
            stored.git_info = Some(gi);
        }
        stored.thread.updated_at = Some(Utc::now().timestamp());
        Some(stored.thread.clone())
    }

    pub async fn update_settings(
        &self,
        thread_id: &str,
        patch: claude_app_server_protocol::ThreadSettings,
    ) -> Option<claude_app_server_protocol::ThreadSettings> {
        let mut guard = self.inner.lock().await;
        let stored = guard.threads.get_mut(thread_id)?;
        if let Some(m) = patch.model { stored.settings.model = Some(m); stored.model = stored.settings.model.clone().unwrap(); }
        if let Some(sp) = patch.system_prompt { stored.settings.system_prompt = Some(sp.clone()); stored.system_prompt = Some(sp); }
        if let Some(pm) = patch.permission_mode { stored.settings.permission_mode = Some(pm); }
        if let Some(c) = patch.cwd { stored.settings.cwd = Some(c.clone()); stored.cwd = Some(c); }
        Some(stored.settings.clone())
    }

    pub async fn set_memory_mode(&self, thread_id: &str, mode: String) -> bool {
        let mut guard = self.inner.lock().await;
        if let Some(stored) = guard.threads.get_mut(thread_id) {
            stored.memory_mode = Some(mode);
            return true;
        }
        if let Some(stored) = guard.archived.get_mut(thread_id) {
            stored.memory_mode = Some(mode);
            return true;
        }
        false
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
            content: input.clone(),
        };
        let turn = Turn {
            id: turn_id.clone(),
            status: TurnStatus::InProgress,
            items: vec![user_item],
            error: None,
        };
        stored.turns.push(turn.clone());
        stored.thread.turns = stored.turns.clone();
        stored.thread.status = ThreadStatus::Active { active_flags: vec![] };
        let ephemeral = stored.thread.ephemeral.unwrap_or(false);
        drop(guard);
        if !ephemeral {
            if let Some(rollouts) = &self.rollouts {
                rollouts
                    .append(thread_id, RolloutEvent::TurnStarted {
                        turn_id: turn.id.clone(),
                        input,
                        ts: RolloutStore::now(),
                    })
                    .await;
            }
        }
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
        let appended_item = if agent_text.is_empty() {
            None
        } else {
            Some(Item::AgentMessage {
                id: format!("msg_{}", Uuid::now_v7()),
                text: agent_text.clone(),
            })
        };
        let updated_turn = {
            let turn = stored.turns.iter_mut().find(|t| t.id == turn_id)?;
            turn.status = status;
            if let Some(item) = appended_item.clone() {
                turn.items.push(item);
            }
            turn.clone()
        };
        stored.thread.status = ThreadStatus::Idle;
        stored.thread.preview = agent_text.chars().take(80).collect();
        stored.thread.updated_at = Some(Utc::now().timestamp());
        stored.thread.turns = stored.turns.clone();
        let ephemeral = stored.thread.ephemeral.unwrap_or(false);
        drop(guard);
        if !ephemeral {
            if let Some(rollouts) = &self.rollouts {
                if let Some(item) = appended_item {
                    rollouts
                        .append(thread_id, RolloutEvent::ItemAppended {
                            turn_id: turn_id.to_string(),
                            item,
                            ts: RolloutStore::now(),
                        })
                        .await;
                }
                rollouts
                    .append(thread_id, RolloutEvent::TurnCompleted {
                        turn_id: turn_id.to_string(),
                        status,
                        ts: RolloutStore::now(),
                    })
                    .await;
            }
        }
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
