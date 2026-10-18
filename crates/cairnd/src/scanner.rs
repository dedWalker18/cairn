//! Filesystem scanner: per-directory diff between the listing on disk and
//! the index.
//!
//! Safety invariants (tested in tests/):
//!  * The scanner only reads user files and directories (lstat, readdir).
//!  * A directory that cannot be listed completely is never treated as empty:
//!    nothing under it is deleted from the index.
//!  * Deletions are deferred to the end of a pass and re-verified on disk, so
//!    a file moved between two directories keeps its node id (and vectors).
//!  * Symlinks are never followed; mount and subvolume boundaries are not
//!    crossed; FIFOs, sockets and devices are ignored.

use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::Instant;

use anyhow::{Context, Result};
use cairn_core::classify::{self, Category};
use cairn_core::db::{self, NewNode, Stage};
use cairn_core::paths;
use cairn_core::timeutil::now_ns;
use rusqlite::{Connection, OptionalExtension};

use crate::shared::Shared;

/// Files larger than this are indexed by name/metadata only.
const MAX_CONTENT_FILE: u64 = 64 << 20;
/// Folder context waits for its children to be extracted first.
fn folder_ctx_delay_ns() -> i64 {
    static D: std::sync::OnceLock<i64> = std::sync::OnceLock::new();
    *D.get_or_init(|| std::env::var("CAIRN_FOLDER_CTX_DELAY_MS").ok().and_then(|v| v.parse::<i64>().ok()).unwrap_or(20_000) * 1_000_000)
}
const DIRS_PER_TXN: usize = 400;

pub type OnDir<'a> = &'a dyn Fn(i64, &Path);

struct Vanished {
    id: i64,
    parent: i64,
    name: Vec<u8>,
    path: PathBuf,
}

pub fn fingerprint(size: u64, mtime_ns: i64, ino: u64) -> Vec<u8> {
    let mut h = blake3::Hasher::new();
    h.update(&size.to_le_bytes());
    h.update(&mtime_ns.to_le_bytes());
    h.update(&ino.to_le_bytes());
    h.finalize().as_bytes()[..16].to_vec()
}

fn mtime_ns(md: &std::fs::Metadata) -> i64 {
    md.mtime().saturating_mul(1_000_000_000).saturating_add(md.mtime_nsec())
}

fn ctime_ns(md: &std::fs::Metadata) -> i64 {
    md.ctime().saturating_mul(1_000_000_000).saturating_add(md.ctime_nsec())
}

fn btime_ns(md: &std::fs::Metadata) -> Option<i64> {
    let t = md.created().ok()?;
    let d = t.duration_since(std::time::UNIX_EPOCH).ok()?;
    i64::try_from(d.as_nanos()).ok()
}

pub struct Scanner<'a> {
    conn: Connection,
    on_dir: OnDir<'a>,
    pub ctx: Ctx<'a>,
}

/// Everything `diff_dir` mutates, kept apart from the connection so a
/// transaction can borrow the connection at the same time.
pub struct Ctx<'a> {
    sh: &'a Shared,
    excl_paths: HashSet<Vec<u8>>,
    excl_inodes: HashSet<(u64, u64)>,
    pub stats: ScanStats,
    /// Vectors tombstoned in the open transaction; dropped from memory only
    /// after it commits.
    pending_dead: Vec<db::DeadVec>,
    excl_seen: u64,
}

impl Ctx<'_> {
    /// Pick up exclusions added while a long scan is running.
    fn maybe_reload(&mut self, conn: &Connection) -> Result<()> {
        let v = self.sh.excl_version.load(Ordering::SeqCst);
        if v != self.excl_seen {
            load_exclusions_into(self, conn)?;
            self.excl_seen = v;
        }
        Ok(())
    }

    fn flush_dead(&mut self) {
        let dead = std::mem::take(&mut self.pending_dead);
        self.sh.kill_vecs(&dead);
    }

    fn is_excluded_dir(&self, path: &Path, md: &std::fs::Metadata) -> bool {
        // Never index ourselves (feedback loop) even if a root contains us.
        if path.starts_with(&self.sh.data_dir) {
            return true;
        }
        if self.excl_paths.contains(path.as_os_str().as_bytes()) || self.excl_inodes.contains(&(md.dev(), md.ino())) {
            return true;
        }
        classify::EXCLUDE_MARKERS.iter().any(|m| std::fs::symlink_metadata(path.join(m)).is_ok())
    }
}

#[derive(Default, Debug)]
pub struct ScanStats {
    pub dirs: u64,
    pub inserted: u64,
    pub updated: u64,
    pub moved: u64,
    pub deleted: u64,
    pub unreadable: u64,
}

impl<'a> Scanner<'a> {
    pub fn new(sh: &'a Shared, on_dir: OnDir<'a>) -> Result<Scanner<'a>> {
        let conn = sh.conn()?;
        let ctx = Ctx { sh, excl_paths: HashSet::new(), excl_inodes: HashSet::new(), stats: ScanStats::default(), pending_dead: Vec::new(), excl_seen: 0 };
        let mut s = Scanner { conn, on_dir, ctx };
        s.load_exclusions()?;
        Ok(s)
    }

    pub fn load_exclusions(&mut self) -> Result<()> {
        self.ctx.excl_seen = self.ctx.sh.excl_version.load(Ordering::SeqCst);
        load_exclusions_into(&mut self.ctx, &self.conn)
    }

    /// Ensure a root node exists; returns (id, dev).
    fn root(&mut self, root: &Path) -> Result<Option<(i64, u64)>> {
        let md = match std::fs::symlink_metadata(root) {
            Ok(m) if m.is_dir() => m,
            _ => {
                tracing::warn!(root = %root.display(), "index root missing or not a directory; skipping (nothing deleted)");
                return Ok(None);
            }
        };
        let name = paths::to_bytes(root);
        if let Some(id) = db::root_id(&self.conn, &name)? {
            return Ok(Some((id, md.dev())));
        }
        let fp = fingerprint(0, mtime_ns(&md), md.ino());
        let tx = db::write_tx(&self.conn)?;
        let id = db::insert_node(
            &tx,
            &NewNode {
                parent: None,
                name: &name,
                is_dir: true,
                dev: md.dev(),
                ino: md.ino(),
                size: 0,
                mtime_ns: mtime_ns(&md),
                ctime_ns: ctime_ns(&md),
                btime_ns: btime_ns(&md),
                category: Category::Dir,
                ext: "",
                flags: 0,
                fp: &fp,
            },
        )?;
        tx.commit()?;
        Ok(Some((id, md.dev())))
    }

    /// Walk every root completely. Deletions happen only if the pass finished.
    pub fn full_scan(&mut self) -> Result<()> {
        let started = Instant::now();
        self.load_exclusions()?;
        let roots = self.ctx.sh.roots.clone();
        let mut queue: VecDeque<(i64, PathBuf, u64)> = VecDeque::new();
        for r in &roots {
            if let Some((id, dev)) = self.root(r)? {
                queue.push_back((id, r.clone(), dev));
            }
        }
        let mut vanished = Vec::new();
        let mut tx = db::write_tx(&self.conn)?;
        let mut in_tx = 0usize;
        let mut tx_started = Instant::now();
        while let Some((id, path, dev)) = queue.pop_front() {
            if self.ctx.sh.is_shutdown() {
                tx.commit()?;
                self.ctx.flush_dead();
                return Ok(()); // partial pass: no deletions
            }
            let subdirs = diff_dir(&tx, &mut self.ctx, id, &path, dev, true, &mut vanished)?;
            (self.on_dir)(id, &path);
            queue.extend(subdirs);
            in_tx += 1;
            // Short write transactions: the pipeline and searches must not wait long.
            if in_tx >= DIRS_PER_TXN || tx_started.elapsed() > std::time::Duration::from_millis(250) {
                tx.commit()?;
                self.ctx.flush_dead();
                self.ctx.sh.notify_work();
                tx = db::write_tx(&self.conn)?;
                in_tx = 0;
                tx_started = Instant::now();
            }
        }
        tx.commit()?;
        self.ctx.flush_dead();
        self.apply_vanished(vanished)?;
        self.ctx.sh.notify_work();
        *self.ctx.sh.scan_done_at.lock().unwrap() = Some(Instant::now());
        self.ctx.sh.notify_stats();
        tracing::info!(took = ?started.elapsed(), stats = ?self.ctx.stats, "full scan complete");
        Ok(())
    }

    /// Re-list specific directories (from the watcher). New subdirectories are
    /// scanned recursively; existing ones are not descended into.
    pub fn rescan_dirs(&mut self, dirs: &[i64]) -> Result<()> {
        let mut vanished = Vec::new();
        let mut queue: VecDeque<(i64, PathBuf, u64)> = VecDeque::new();
        let mut newly_excluded = Vec::new();
        for &id in dirs {
            let Some(path) = db::path_of(&self.conn, id)? else { continue };
            let path = paths::from_bytes(&path);
            let is_root = self.ctx.sh.roots.iter().any(|r| r == &path);
            match std::fs::symlink_metadata(&path) {
                // A marker file (.noindex, CACHEDIR.TAG, ...) just appeared in
                // this folder: the folder itself is now excluded.
                Ok(md) if md.is_dir() && !is_root && self.ctx.is_excluded_dir(&path, &md) => newly_excluded.push(id),
                Ok(md) if md.is_dir() => queue.push_back((id, path, md.dev())),
                _ => {} // gone: its parent's rescan handles it
            }
        }
        if !newly_excluded.is_empty() {
            let tx = db::write_tx(&self.conn)?;
            let mut dead = Vec::new();
            for id in newly_excluded {
                let ids = db::subtree_ids(&tx, id)?;
                self.ctx.stats.deleted += ids.len() as u64;
                dead.extend(db::delete_nodes(&tx, &ids)?);
            }
            tx.commit()?;
            self.ctx.sh.kill_vecs(&dead);
        }
        let tx = db::write_tx(&self.conn)?;
        let mut first = queue.len();
        while let Some((id, path, dev)) = queue.pop_front() {
            if self.ctx.sh.is_shutdown() {
                break;
            }
            let recurse_all = first == 0; // dirs discovered below are new: scan fully
            first = first.saturating_sub(1);
            let subdirs = diff_dir(&tx, &mut self.ctx, id, &path, dev, recurse_all, &mut vanished)?;
            (self.on_dir)(id, &path);
            queue.extend(subdirs);
        }
        tx.commit()?;
        self.ctx.flush_dead();
        if !self.ctx.sh.is_shutdown() {
            self.apply_vanished(vanished)?;
        }
        self.ctx.sh.notify_work();
        Ok(())
    }

    fn apply_vanished(&mut self, vanished: Vec<Vanished>) -> Result<()> {
        if vanished.is_empty() {
            return Ok(());
        }
        let tx = db::write_tx(&self.conn)?;
        let mut dead = Vec::new();
        for v in vanished {
            // Still where we last saw it? (It may have been moved since.)
            let still: Option<i64> = tx
                .query_row("SELECT id FROM nodes WHERE id=?1 AND parent=?2 AND name=?3", rusqlite::params![v.id, v.parent, v.name], |r| r.get(0))
                .optional()?;
            if still.is_none() {
                continue;
            }
            // Re-verify on disk right before deleting.
            if std::fs::symlink_metadata(&v.path).is_ok() {
                continue;
            }
            let ids = db::subtree_ids(&tx, v.id)?;
            self.ctx.stats.deleted += ids.len() as u64;
            dead.extend(db::delete_nodes(&tx, &ids)?);
            if classify::is_context_file(&v.name) {
                if let Some(fp) = dir_fp(&tx, v.parent)? {
                    db::enqueue_at(&tx, v.parent, Stage::FolderCtx, &fp, now_ns() + folder_ctx_delay_ns())?;
                }
            }
        }
        tx.commit()?;
        self.ctx.sh.kill_vecs(&dead);
        Ok(())
    }
}

fn load_exclusions_into(ctx: &mut Ctx, conn: &Connection) -> Result<()> {
    ctx.excl_paths.clear();
    ctx.excl_inodes.clear();
    let mut st = conn.prepare("SELECT path, dev, ino FROM exclusions")?;
    let rows = st.query_map([], |r| Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, Option<i64>>(1)?, r.get::<_, Option<i64>>(2)?)))?;
    for r in rows {
        let (p, d, i) = r?;
        ctx.excl_paths.insert(p);
        if let (Some(d), Some(i)) = (d, i) {
            ctx.excl_inodes.insert((d as u64, i as u64));
        }
    }
    Ok(())
}

fn dir_fp(conn: &Connection, id: i64) -> Result<Option<Vec<u8>>> {
    Ok(conn.query_row("SELECT fp FROM nodes WHERE id=?1", [id], |r| r.get(0)).optional()?)
}

/// Resolve an absolute path to a node id by walking from its root.
pub fn lookup_path(conn: &Connection, roots: &[PathBuf], path: &[u8]) -> Result<Option<i64>> {
    let p = paths::from_bytes(path);
    for root in roots {
        let Ok(rest) = p.strip_prefix(root) else { continue };
        let Some(mut id) = db::root_id(conn, &paths::to_bytes(root))? else { continue };
        let mut st = conn.prepare_cached("SELECT id FROM nodes WHERE parent=?1 AND name=?2")?;
        for comp in rest.components() {
            let name = comp.as_os_str().as_bytes();
            match st.query_row(rusqlite::params![id, name], |r| r.get::<_, i64>(0)).optional()? {
                Some(c) => id = c,
                None => return Ok(None),
            }
        }
        return Ok(Some(id));
    }
    Ok(None)
}

fn enqueue_content(tx: &Connection, id: i64, parent: i64, name: &[u8], cat: Category, ext: &str, size: u64, secret: bool, fp: &[u8]) -> Result<()> {
    if secret {
        return Ok(());
    }
    if cat == Category::Image {
        db::enqueue(tx, id, Stage::ImageMeta, fp)?;
    }
    if size <= MAX_CONTENT_FILE && classify::has_extractable_text(cat, ext) {
        db::enqueue(tx, id, Stage::ExtractText, fp)?;
    }
    if classify::is_context_file(name) {
        if let Some(pfp) = dir_fp(tx, parent)? {
            db::enqueue_at(tx, parent, Stage::FolderCtx, &pfp, now_ns() + folder_ctx_delay_ns())?;
        }
    }
    Ok(())
}

/// Diff one directory. Returns subdirectories to visit next.
fn diff_dir(
    tx: &Connection,
    sc: &mut Ctx,
    dir_id: i64,
    dir_path: &Path,
    dir_dev: u64,
    recurse_all: bool,
    vanished: &mut Vec<Vanished>,
) -> Result<Vec<(i64, PathBuf, u64)>> {
    sc.stats.dirs += 1;
    sc.maybe_reload(tx)?;
    let rd = match std::fs::read_dir(dir_path) {
        Ok(rd) => rd,
        Err(e) => {
            sc.stats.unreadable += 1;
            tracing::debug!(dir = %dir_path.display(), error = %e, "cannot list directory; index left unchanged");
            return Ok(Vec::new());
        }
    };
    let mut names = Vec::new();
    let mut complete = true;
    for e in rd {
        match e {
            Ok(e) => names.push(e.file_name()),
            Err(_) => complete = false,
        }
    }
    let mut known: HashMap<Vec<u8>, db::NodeRow> = db::children(tx, dir_id)?.into_iter().map(|n| (n.name.clone(), n)).collect();
    let mut subdirs = Vec::new();
    let mut listing_changed = false;

    for name_os in names {
        let name = name_os.as_bytes().to_vec();
        let path = dir_path.join(&name_os);
        let md = match std::fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(_) => {
                // Transient failure: keep whatever we knew.
                known.remove(&name);
                continue;
            }
        };
        let ft = md.file_type();
        if ft.is_symlink() || !(ft.is_file() || ft.is_dir()) {
            continue;
        }
        if md.dev() != dir_dev {
            continue; // another filesystem, mount or btrfs subvolume
        }
        let is_dir = ft.is_dir();
        if is_dir && (classify::is_skipped_dir_name(&name) || sc.is_excluded_dir(&path, &md)) {
            continue; // left in `known` -> removed from the index if present
        }
        let (cat, ext) = if is_dir { (Category::Dir, String::new()) } else { (classify::category_for(&name), classify::extension(&name)) };
        let secret = !is_dir && classify::is_secret(&name);
        let size = if is_dir { 0 } else { md.size() };
        let mt = mtime_ns(&md);
        let fp = fingerprint(size, mt, md.ino());
        let bt = btime_ns(&md);
        let nn = NewNode {
            parent: Some(dir_id),
            name: &name,
            is_dir,
            dev: md.dev(),
            ino: md.ino(),
            size: size as i64,
            mtime_ns: mt,
            ctime_ns: ctime_ns(&md),
            btime_ns: bt,
            category: cat,
            ext: &ext,
            flags: if secret { db::FLAG_SECRET } else { 0 },
            fp: &fp,
        };

        match known.remove(&name) {
            Some(old) if old.is_dir == is_dir => {
                if old.fp != fp || old.category != cat {
                    db::update_node_stat(tx, old.id, &nn)?;
                    sc.stats.updated += 1;
                    if is_dir {
                        db::enqueue_at(tx, old.id, Stage::FolderCtx, &fp, now_ns() + folder_ctx_delay_ns())?;
                    } else {
                        enqueue_content(tx, old.id, dir_id, &name, cat, &ext, size, secret, &fp)?;
                    }
                }
                if is_dir && recurse_all {
                    subdirs.push((old.id, path, md.dev()));
                }
            }
            Some(old) => {
                // File became a directory or vice versa.
                let ids = db::subtree_ids(tx, old.id)?;
                let dead = db::delete_nodes(tx, &ids)?;
                sc.pending_dead.extend(dead);
                let id = db::insert_node(tx, &nn)?;
                sc.stats.inserted += 1;
                listing_changed = true;
                if is_dir {
                    db::enqueue_at(tx, id, Stage::FolderCtx, &fp, now_ns() + folder_ctx_delay_ns())?;
                    subdirs.push((id, path, md.dev()));
                } else {
                    enqueue_content(tx, id, dir_id, &name, cat, &ext, size, secret, &fp)?;
                }
            }
            None => {
                listing_changed = true;
                if let Some(moved) = find_moved(tx, &md, is_dir, size, mt)? {
                    db::move_node(tx, moved, dir_id, &name)?;
                    db::update_node_stat(tx, moved, &nn)?;
                    sc.stats.moved += 1;
                    if is_dir {
                        db::enqueue_at(tx, moved, Stage::FolderCtx, &fp, now_ns() + folder_ctx_delay_ns())?;
                        if recurse_all {
                            subdirs.push((moved, path, md.dev()));
                        }
                    } else if classify::is_context_file(&name) {
                        enqueue_content(tx, moved, dir_id, &name, cat, &ext, size, secret, &fp)?;
                    }
                    continue;
                }
                let id = db::insert_node(tx, &nn)?;
                sc.stats.inserted += 1;
                if is_dir {
                    db::enqueue_at(tx, id, Stage::FolderCtx, &fp, now_ns() + folder_ctx_delay_ns())?;
                    subdirs.push((id, path, md.dev()));
                } else {
                    enqueue_content(tx, id, dir_id, &name, cat, &ext, size, secret, &fp)?;
                }
            }
        }
    }

    if complete {
        for (name, row) in known {
            listing_changed = true;
            let path = dir_path.join(OsStr::from_bytes(&name));
            vanished.push(Vanished { id: row.id, parent: dir_id, name, path });
        }
    }
    if listing_changed {
        if let Some(fp) = dir_fp(tx, dir_id)? {
            db::enqueue_at(tx, dir_id, Stage::FolderCtx, &fp, now_ns() + folder_ctx_delay_ns())?;
        }
    }
    Ok(subdirs)
}

/// A node with the same inode whose old location no longer holds it: a move.
fn find_moved(tx: &Connection, md: &std::fs::Metadata, is_dir: bool, size: u64, mtime: i64) -> Result<Option<i64>> {
    let mut st = tx.prepare_cached("SELECT id, is_dir, size, mtime_ns FROM nodes WHERE dev=?1 AND ino=?2 AND parent IS NOT NULL")?;
    let cands: Vec<(i64, bool, i64, i64)> = st
        .query_map(rusqlite::params![md.dev() as i64, md.ino() as i64], |r| Ok((r.get(0)?, r.get::<_, i64>(1)? != 0, r.get(2)?, r.get(3)?)))?
        .collect::<rusqlite::Result<_>>()?;
    for (id, d, s, m) in cands {
        if d != is_dir || (!is_dir && (s != size as i64 || m != mtime)) {
            continue;
        }
        let Some(old_path) = db::path_of(tx, id)? else { continue };
        match std::fs::symlink_metadata(paths::from_bytes(&old_path)) {
            Ok(old) if old.ino() == md.ino() && old.dev() == md.dev() => continue, // hard link
            _ => return Ok(Some(id)),
        }
    }
    Ok(None)
}

pub fn scanner_thread(sh: &Shared, on_dir: OnDir) -> Result<()> {
    cairn_core::sandbox::set_thread_idle();
    let mut sc = Scanner::new(sh, on_dir).context("scanner init")?;
    sh.scanning.store(true, Ordering::SeqCst);
    // A transient error must not kill the scanner: retry with backoff.
    let mut backoff = 5u64;
    while let Err(e) = sc.full_scan() {
        tracing::error!(error = %format!("{e:#}"), retry_in_s = backoff, "full scan failed; retrying");
        for _ in 0..backoff * 5 {
            if sh.is_shutdown() {
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
        backoff = (backoff * 2).min(300);
    }
    sh.scanning.store(false, Ordering::SeqCst);
    sh.initial_scan_done.store(true, Ordering::SeqCst);
    let mut last_full = Instant::now();
    const FULL_EVERY: std::time::Duration = std::time::Duration::from_secs(3 * 3600);
    while !sh.is_shutdown() {
        sh.wait_on(&sh.scan_signal, 1500);
        if sh.is_shutdown() {
            break;
        }
        if sh.full_rescan.swap(false, Ordering::SeqCst) || last_full.elapsed() > FULL_EVERY {
            sh.scanning.store(true, Ordering::SeqCst);
            sc.ctx.stats = ScanStats::default();
            if let Err(e) = sc.full_scan() {
                tracing::error!(error = %format!("{e:#}"), "full scan failed");
            }
            sh.scanning.store(false, Ordering::SeqCst);
            last_full = Instant::now();
            continue;
        }
        // Debounce: let a burst of events settle before re-listing.
        let dirty: Vec<i64> = {
            let mut d = sh.dirty.lock().unwrap();
            d.drain().collect()
        };
        if !dirty.is_empty() {
            std::thread::sleep(std::time::Duration::from_millis(500));
            let mut all = dirty;
            all.extend(sh.dirty.lock().unwrap().drain());
            all.sort_unstable();
            all.dedup();
            if let Err(e) = sc.load_exclusions().and_then(|_| sc.rescan_dirs(&all)) {
                tracing::error!(error = %format!("{e:#}"), "rescan failed");
            }
        }
    }
    Ok(())
}
