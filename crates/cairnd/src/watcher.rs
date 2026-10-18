//! inotify watcher. Unprivileged, so one watch per directory, within a
//! budget that leaves most of the per-user limit to editors and other tools
//! (D-024). Directories beyond the budget are covered by periodic rescans.
//!
//! Events only mark directories dirty; the scanner re-lists them, so there
//! is exactly one code path that mutates the index from the filesystem.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use anyhow::Result;
use inotify::{EventMask, Inotify, WatchDescriptor, WatchMask, Watches};

use crate::shared::Shared;

pub struct WatchSet {
    watches: Mutex<Watches>,
    map: Mutex<HashMap<WatchDescriptor, i64>>,
    count: AtomicUsize,
    budget: usize,
    warned: std::sync::atomic::AtomicBool,
}

pub fn budget() -> usize {
    let max: usize = std::fs::read_to_string("/proc/sys/fs/inotify/max_user_watches").ok().and_then(|s| s.trim().parse().ok()).unwrap_or(8192);
    let b = (max / 2).min(200_000);
    std::env::var("CAIRN_WATCH_BUDGET").ok().and_then(|v| v.parse().ok()).unwrap_or(b)
}

impl WatchSet {
    pub fn add(&self, dir_id: i64, path: &Path) {
        if self.count.load(Ordering::Relaxed) >= self.budget {
            if !self.warned.swap(true, Ordering::Relaxed) {
                tracing::info!(budget = self.budget, "inotify watch budget reached; remaining folders are covered by periodic rescans");
            }
            return;
        }
        let mask = WatchMask::CREATE
            | WatchMask::DELETE
            | WatchMask::MOVED_FROM
            | WatchMask::MOVED_TO
            | WatchMask::CLOSE_WRITE
            | WatchMask::ATTRIB
            | WatchMask::DELETE_SELF
            | WatchMask::ONLYDIR
            | WatchMask::DONT_FOLLOW
            | WatchMask::EXCL_UNLINK;
        let r = self.watches.lock().unwrap().add(path, mask);
        match r {
            Ok(wd) => {
                if self.map.lock().unwrap().insert(wd, dir_id).is_none() {
                    self.count.fetch_add(1, Ordering::Relaxed);
                }
            }
            Err(e) => {
                if e.raw_os_error() == Some(libc::ENOSPC) {
                    // System-wide limit hit (other apps): stop adding.
                    self.count.store(usize::MAX / 2, Ordering::Relaxed);
                }
            }
        }
    }
}

pub fn create() -> Result<(Inotify, WatchSet)> {
    let ino = Inotify::init()?;
    let ws = WatchSet {
        watches: Mutex::new(ino.watches()),
        map: Mutex::new(HashMap::new()),
        count: AtomicUsize::new(0),
        budget: budget(),
        warned: std::sync::atomic::AtomicBool::new(false),
    };
    Ok((ino, ws))
}

pub fn watcher_thread(sh: &Shared, mut ino: Inotify, ws: &WatchSet) -> Result<()> {
    let mut buf = vec![0u8; 64 * 1024];
    while !sh.is_shutdown() {
        let events = match ino.read_events(&mut buf) {
            Ok(ev) => ev,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(std::time::Duration::from_millis(250));
                continue;
            }
            Err(e) => {
                tracing::error!(error = %e, "inotify read failed; falling back to periodic rescans");
                return Ok(());
            }
        };
        let mut any = false;
        {
            let mut map = ws.map.lock().unwrap();
            for ev in events {
                if ev.mask.contains(EventMask::Q_OVERFLOW) {
                    sh.full_rescan.store(true, Ordering::SeqCst);
                    any = true;
                    continue;
                }
                if ev.mask.contains(EventMask::IGNORED) {
                    if map.remove(&ev.wd).is_some() {
                        ws.count.fetch_sub(1, Ordering::Relaxed);
                    }
                    continue;
                }
                if let Some(&dir) = map.get(&ev.wd) {
                    sh.mark_dirty(dir);
                    any = true;
                }
            }
        }
        if any {
            sh.notify_scan();
        }
    }
    Ok(())
}
