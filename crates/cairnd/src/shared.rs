//! State shared between daemon threads.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex, RwLock};

use anyhow::Result;
use cairn_core::db::{self, DeadVec};
use cairn_core::vecstore::VecStore;

/// Expensive counts, refreshed in the background (D-058): `count(*)` over
/// millions of rows takes seconds and must never sit on a request path.
#[derive(Default, Clone)]
pub struct Stats {
    pub files: i64,
    pub dirs: i64,
    pub chunks: i64,
    pub failed: i64,
    pub pending: Vec<(String, i64)>,
    pub fresh: bool,
    /// When this snapshot started being computed.
    pub computed_at: Option<std::time::Instant>,
}

pub struct Shared {
    pub data_dir: PathBuf,
    pub db_path: PathBuf,
    pub models_dir: PathBuf,
    pub roots: Vec<PathBuf>,
    pub text_vecs: RwLock<VecStore>,
    pub clip_vecs: RwLock<VecStore>,
    pub text_model: Mutex<Option<cairn_ml::TextEmbedder>>,
    pub clip_model: Mutex<Option<cairn_ml::ClipText>>,
    pub shutdown: AtomicBool,
    pub paused: AtomicBool,
    pub scanning: AtomicBool,
    pub initial_scan_done: AtomicBool,
    pub throttle: Mutex<Option<String>>,
    pub sandbox: Mutex<String>,
    pub stats: Mutex<Stats>,
    /// When the last full scan finished; stats older than this are stale.
    pub scan_done_at: Mutex<Option<std::time::Instant>>,
    /// Wakes the stats thread (work ran out, exclusions changed, ...).
    pub stats_signal: (Mutex<bool>, Condvar),
    /// Directories (node ids) whose listing changed; drained by the scanner.
    pub dirty: Mutex<HashSet<i64>>,
    pub full_rescan: AtomicBool,
    /// Bumped whenever exclusions change; the scanner reloads its cache.
    pub excl_version: std::sync::atomic::AtomicU64,
    /// Wakes the pipeline when new work is queued.
    pub work: (Mutex<bool>, Condvar),
    /// Wakes the scanner.
    pub scan_signal: (Mutex<bool>, Condvar),
}

impl Shared {
    pub fn is_shutdown(&self) -> bool {
        self.shutdown.load(Ordering::SeqCst)
    }

    pub fn conn(&self) -> Result<rusqlite::Connection> {
        db::open(&self.db_path)
    }

    pub fn kill_vecs(&self, dead: &[DeadVec]) {
        if dead.is_empty() {
            return;
        }
        let mut t = self.text_vecs.write().unwrap();
        let mut c = self.clip_vecs.write().unwrap();
        for (space, slot) in dead {
            match *space {
                db::SPACE_TEXT => t.kill(*slot),
                db::SPACE_CLIP => c.kill(*slot),
                _ => {}
            }
        }
    }

    pub fn notify_work(&self) {
        let (m, c) = &self.work;
        *m.lock().unwrap() = true;
        c.notify_all();
    }

    pub fn notify_stats(&self) {
        let (m, c) = &self.stats_signal;
        *m.lock().unwrap() = true;
        c.notify_all();
    }

    pub fn notify_scan(&self) {
        let (m, c) = &self.scan_signal;
        *m.lock().unwrap() = true;
        c.notify_all();
    }

    /// Sleep up to `ms`, returning early on shutdown or a signal on `pair`.
    pub fn wait_on(&self, pair: &(Mutex<bool>, Condvar), ms: u64) {
        let (m, c) = pair;
        let g = m.lock().unwrap();
        let (mut g, _) = c.wait_timeout_while(g, std::time::Duration::from_millis(ms), |flag| !*flag && !self.is_shutdown()).unwrap();
        *g = false;
    }

    pub fn mark_dirty(&self, dir: i64) {
        self.dirty.lock().unwrap().insert(dir);
    }
}
