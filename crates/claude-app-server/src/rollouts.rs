//! JSONL rollout persistence. Mirrors codex semantics: every thread has a
//! `$CLAUDE_HOME/sessions/<thread_id>.jsonl` file. Each line is one
//! `RolloutEvent` capturing a meaningful state transition (thread created,
//! turn started/completed, item added, archived, etc.). On restart, threads
//! are reconstructed by replaying the file.
//!
//! Concurrent writers per thread are serialized through a `Mutex` so the
//! file stays line-aligned. Reads scan once on startup; subsequent listing
//! uses the in-memory `ThreadStore`.

use chrono::Utc;
use claude_app_server_protocol::{
    InputChunk, Item, Thread, ThreadStatus, Turn, TurnStatus,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::Mutex;
use tracing::warn;

#[derive(Debug, Error)]
pub enum RolloutError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("encode: {0}")]
    Encode(#[from] serde_json::Error),
}

/// Per-event record written to the rollout file.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RolloutEvent {
    ThreadCreated {
        thread: Thread,
        model: String,
        system_prompt: Option<String>,
        cwd: Option<String>,
        ts: i64,
    },
    NameSet { name: String, ts: i64 },
    TurnStarted { turn_id: String, input: Vec<InputChunk>, ts: i64 },
    ItemAppended { turn_id: String, item: Item, ts: i64 },
    TurnCompleted { turn_id: String, status: TurnStatus, ts: i64 },
    Archived { ts: i64 },
    Unarchived { ts: i64 },
    Closed { ts: i64 },
}

#[derive(Clone)]
pub struct RolloutStore {
    dir: PathBuf,
    inner: Arc<Mutex<HashMap<String, Arc<Mutex<File>>>>>,
}

impl RolloutStore {
    pub fn open(dir: PathBuf) -> Result<Self, RolloutError> {
        if let Err(e) = std::fs::create_dir_all(&dir) {
            warn!("rollout dir {} could not be created: {e}", dir.display());
        }
        Ok(Self {
            dir,
            inner: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    pub fn dir(&self) -> &Path { &self.dir }

    pub fn thread_path(&self, thread_id: &str) -> PathBuf {
        self.dir.join(format!("{thread_id}.jsonl"))
    }

    /// Append one event to the thread's rollout. Errors are logged but not
    /// propagated upward — persistence failures must not break live RPC.
    pub async fn append(&self, thread_id: &str, event: RolloutEvent) {
        if let Err(e) = self.append_inner(thread_id, event).await {
            warn!("rollout append for {thread_id} failed: {e}");
        }
    }

    async fn append_inner(
        &self,
        thread_id: &str,
        event: RolloutEvent,
    ) -> Result<(), RolloutError> {
        let handle = {
            let mut guard = self.inner.lock().await;
            if let Some(existing) = guard.get(thread_id) {
                existing.clone()
            } else {
                let path = self.thread_path(thread_id);
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                let file = OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)?;
                let arc = Arc::new(Mutex::new(file));
                guard.insert(thread_id.to_string(), arc.clone());
                arc
            }
        };
        let mut line = serde_json::to_string(&event)?;
        line.push('\n');
        let mut file = handle.lock().await;
        file.write_all(line.as_bytes())?;
        file.flush()?;
        Ok(())
    }

    /// Drop the cached file handle so the next append reopens. Used after
    /// archive/unarchive so the file path stays correct.
    pub async fn forget(&self, thread_id: &str) {
        self.inner.lock().await.remove(thread_id);
    }

    /// Replay every rollout file under the dir and reconstruct stored
    /// threads. Returns the rebuilt `StoredThread` records.
    pub fn replay_all(&self) -> Result<Vec<RebuiltThread>, RolloutError> {
        let mut out = Vec::new();
        if !self.dir.exists() {
            return Ok(out);
        }
        for entry in std::fs::read_dir(&self.dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            match Self::replay_one(&path) {
                Ok(Some(thread)) => out.push(thread),
                Ok(None) => {}
                Err(e) => warn!("could not replay rollout {}: {e}", path.display()),
            }
        }
        Ok(out)
    }

    fn replay_one(path: &Path) -> Result<Option<RebuiltThread>, RolloutError> {
        let file = File::open(path)?;
        let reader = BufReader::new(file);
        let mut thread: Option<Thread> = None;
        let mut model = String::new();
        let mut system_prompt: Option<String> = None;
        let mut cwd: Option<String> = None;
        let mut name: Option<String> = None;
        let mut turns: Vec<Turn> = Vec::new();
        let mut archived = false;

        for line in reader.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let event: RolloutEvent = match serde_json::from_str(&line) {
                Ok(e) => e,
                Err(e) => {
                    warn!("skipping invalid rollout line in {}: {e}", path.display());
                    continue;
                }
            };
            match event {
                RolloutEvent::ThreadCreated {
                    thread: t,
                    model: m,
                    system_prompt: sp,
                    cwd: c,
                    ..
                } => {
                    thread = Some(t);
                    model = m;
                    system_prompt = sp;
                    cwd = c;
                }
                RolloutEvent::NameSet { name: n, .. } => name = Some(n),
                RolloutEvent::TurnStarted { turn_id, input, .. } => {
                    turns.push(Turn {
                        id: turn_id.clone(),
                        status: TurnStatus::InProgress,
                        items: vec![Item::UserMessage { id: turn_id, content: input }],
                        error: None,
                    });
                }
                RolloutEvent::ItemAppended { turn_id, item, .. } => {
                    if let Some(t) = turns.iter_mut().find(|t| t.id == turn_id) {
                        t.items.push(item);
                    }
                }
                RolloutEvent::TurnCompleted { turn_id, status, .. } => {
                    if let Some(t) = turns.iter_mut().find(|t| t.id == turn_id) {
                        t.status = status;
                    }
                }
                RolloutEvent::Archived { .. } => archived = true,
                RolloutEvent::Unarchived { .. } => archived = false,
                RolloutEvent::Closed { .. } => {}
            }
        }

        let Some(mut thread) = thread else { return Ok(None); };
        thread.turns = turns.clone();
        thread.status = ThreadStatus::NotLoaded;
        thread.updated_at = Some(Utc::now().timestamp());
        Ok(Some(RebuiltThread {
            thread,
            turns,
            model,
            system_prompt,
            cwd,
            name,
            archived,
        }))
    }

    pub fn now() -> i64 { Utc::now().timestamp() }
}

#[derive(Debug, Clone)]
pub struct RebuiltThread {
    pub thread: Thread,
    pub turns: Vec<Turn>,
    pub model: String,
    pub system_prompt: Option<String>,
    pub cwd: Option<String>,
    pub name: Option<String>,
    pub archived: bool,
}

/// Resolve the rollout directory: `$CLAUDE_APP_SERVER_SESSIONS_DIR` if set,
/// else `$CLAUDE_HOME/sessions`, else `~/.claude/sessions`, else `./sessions`.
pub fn default_dir() -> PathBuf {
    if let Ok(p) = std::env::var("CLAUDE_APP_SERVER_SESSIONS_DIR") {
        return PathBuf::from(p);
    }
    let home = std::env::var("CLAUDE_HOME")
        .or_else(|_| std::env::var("HOME").map(|h| format!("{h}/.claude")))
        .unwrap_or_else(|_| ".claude".into());
    PathBuf::from(home).join("sessions")
}
