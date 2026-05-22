//! `fs/*` endpoints. Filesystem operations exposed to clients.
//!
//! Paths must be absolute. Recursive defaults match codex (`remove` and
//! `createDirectory` default to `recursive: true`).
//!
//! `fs/watch` uses the cross-platform `notify` crate. Each `fs/watch`
//! registers a `watch_id`-keyed watcher; `fs/unwatch` cancels it. Changed
//! paths are coalesced per debounce tick and emitted as `fs/changed`
//! notifications.

use crate::outgoing::OutgoingSender;
use base64::{engine::general_purpose, Engine};
use claude_app_server_protocol as proto;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use proto::{
    FsChangedEvent, FsCopyParams, FsCreateDirectoryParams, FsDirEntry, FsMetadataParams,
    FsMetadataResult, FsReadDirectoryParams, FsReadDirectoryResult, FsReadFileParams,
    FsReadFileResult, FsRemoveParams, FsUnwatchParams, FsWatchParams, FsWatchResult,
    FsWriteFileParams,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::Mutex;

const MAX_READ_BYTES: u64 = 50 * 1024 * 1024;

#[derive(Debug, Error)]
pub enum FsError {
    #[error("path must be absolute: {0}")]
    NotAbsolute(String),
    #[error("file too large: {0} bytes (cap {1})")]
    TooLarge(u64, u64),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("base64 decode: {0}")]
    Base64(#[from] base64::DecodeError),
    #[error("watch error: {0}")]
    Watch(#[from] notify::Error),
    #[error("no such watch: {0}")]
    NoSuchWatch(String),
}

impl FsError {
    pub fn error_code(&self) -> i64 {
        match self {
            FsError::NotAbsolute(_) | FsError::Base64(_) | FsError::NoSuchWatch(_) => {
                proto::INVALID_PARAMS
            }
            FsError::TooLarge(_, _) | FsError::Io(_) | FsError::Watch(_) => proto::INTERNAL_ERROR,
        }
    }
}

fn require_absolute(p: &str) -> Result<PathBuf, FsError> {
    let path = PathBuf::from(p);
    if !path.is_absolute() {
        return Err(FsError::NotAbsolute(p.to_string()));
    }
    Ok(path)
}

pub fn read_file(params: FsReadFileParams) -> Result<FsReadFileResult, FsError> {
    let path = require_absolute(&params.path)?;
    let metadata = std::fs::metadata(&path)?;
    if metadata.len() > MAX_READ_BYTES {
        return Err(FsError::TooLarge(metadata.len(), MAX_READ_BYTES));
    }
    let bytes = std::fs::read(&path)?;
    Ok(FsReadFileResult {
        data_base64: general_purpose::STANDARD.encode(bytes),
    })
}

pub fn write_file(params: FsWriteFileParams) -> Result<(), FsError> {
    let path = require_absolute(&params.path)?;
    let bytes = general_purpose::STANDARD.decode(params.data_base64.as_bytes())?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, bytes)?;
    Ok(())
}

pub fn create_directory(params: FsCreateDirectoryParams) -> Result<(), FsError> {
    let path = require_absolute(&params.path)?;
    if params.recursive {
        std::fs::create_dir_all(&path)?;
    } else {
        std::fs::create_dir(&path)?;
    }
    Ok(())
}

pub fn get_metadata(params: FsMetadataParams) -> Result<FsMetadataResult, FsError> {
    let path = require_absolute(&params.path)?;
    let md = std::fs::symlink_metadata(&path)?;
    let is_symlink = md.file_type().is_symlink();
    let (is_dir, is_file) = if is_symlink {
        // For symlinks, also stat the target to mirror codex semantics.
        match std::fs::metadata(&path) {
            Ok(target) => (target.is_dir(), target.is_file()),
            Err(_) => (false, false),
        }
    } else {
        (md.is_dir(), md.is_file())
    };
    let created_at_ms = md
        .created()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64);
    let modified_at_ms = md
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64);
    Ok(FsMetadataResult {
        is_directory: is_dir,
        is_file,
        is_symlink,
        created_at_ms,
        modified_at_ms,
        size_bytes: Some(md.len()),
    })
}

pub fn read_directory(params: FsReadDirectoryParams) -> Result<FsReadDirectoryResult, FsError> {
    let path = require_absolute(&params.path)?;
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(&path)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        entries.push(FsDirEntry {
            file_name: entry.file_name().to_string_lossy().into_owned(),
            is_directory: file_type.is_dir(),
            is_file: file_type.is_file(),
        });
    }
    entries.sort_by(|a, b| a.file_name.cmp(&b.file_name));
    Ok(FsReadDirectoryResult { entries })
}

pub fn remove(params: FsRemoveParams) -> Result<(), FsError> {
    let path = require_absolute(&params.path)?;
    let md = match std::fs::symlink_metadata(&path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && params.force => return Ok(()),
        Err(e) => return Err(FsError::Io(e)),
    };
    if md.is_dir() && !md.file_type().is_symlink() {
        if params.recursive {
            std::fs::remove_dir_all(&path)?;
        } else {
            std::fs::remove_dir(&path)?;
        }
    } else {
        std::fs::remove_file(&path)?;
    }
    Ok(())
}

pub fn copy(params: FsCopyParams) -> Result<(), FsError> {
    let src = require_absolute(&params.source)?;
    let dst = require_absolute(&params.destination)?;
    let md = std::fs::symlink_metadata(&src)?;
    if md.is_dir() {
        if !params.recursive {
            return Err(FsError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "source is a directory; pass recursive: true",
            )));
        }
        copy_dir_recursive(&src, &dst)?;
    } else {
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(&src, &dst)?;
    }
    Ok(())
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            std::fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}

// --- watcher --------------------------------------------------------------

struct WatchEntry {
    _watcher: RecommendedWatcher,
}

#[derive(Default, Clone)]
pub struct FsWatchRegistry {
    inner: Arc<Mutex<HashMap<String, WatchEntry>>>,
}

impl FsWatchRegistry {
    pub fn new() -> Self { Self::default() }

    pub async fn watch(
        &self,
        params: FsWatchParams,
        out: OutgoingSender,
    ) -> Result<FsWatchResult, FsError> {
        let path = require_absolute(&params.path)?;
        let canonical = std::fs::canonicalize(&path)?;
        let watch_id = params.watch_id.clone();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Event>();

        let event_handler = move |res: Result<Event, notify::Error>| {
            if let Ok(ev) = res {
                let _ = tx.send(ev);
            }
        };
        let mut watcher: RecommendedWatcher = notify::recommended_watcher(event_handler)?;
        let mode = if canonical.is_dir() {
            RecursiveMode::Recursive
        } else {
            RecursiveMode::NonRecursive
        };
        watcher.watch(&canonical, mode)?;

        let watch_id_for_pump = watch_id.clone();
        tokio::spawn(async move {
            // Coalesce events on a short tick window so we don't spam the
            // client with thousands of separate paths during a big change.
            let mut buffer: Vec<String> = Vec::new();
            loop {
                tokio::select! {
                    maybe_event = rx.recv() => {
                        match maybe_event {
                            Some(ev) => {
                                if matches!(
                                    ev.kind,
                                    EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
                                ) {
                                    for p in ev.paths {
                                        buffer.push(p.to_string_lossy().into_owned());
                                    }
                                }
                            }
                            None => break,
                        }
                    }
                    _ = tokio::time::sleep(std::time::Duration::from_millis(120)) => {
                        if !buffer.is_empty() {
                            let mut paths = std::mem::take(&mut buffer);
                            paths.sort();
                            paths.dedup();
                            out.notify(
                                proto::notification::FS_CHANGED,
                                &FsChangedEvent {
                                    watch_id: watch_id_for_pump.clone(),
                                    changed_paths: paths,
                                },
                            )
                            .await;
                        }
                    }
                }
            }
        });

        self.inner
            .lock()
            .await
            .insert(watch_id, WatchEntry { _watcher: watcher });
        Ok(FsWatchResult {
            path: canonical.to_string_lossy().into_owned(),
        })
    }

    pub async fn unwatch(&self, params: FsUnwatchParams) -> Result<(), FsError> {
        let mut guard = self.inner.lock().await;
        if guard.remove(&params.watch_id).is_none() {
            return Err(FsError::NoSuchWatch(params.watch_id));
        }
        Ok(())
    }
}
