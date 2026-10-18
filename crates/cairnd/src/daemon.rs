//! Daemon bootstrap: single-instance lock, sandbox, index integrity checks
//! and recovery, thread start-up and orderly shutdown.

use std::collections::HashSet;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex, RwLock};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use cairn_core::db::{self, Stage};
use cairn_core::vecstore::VecStore;
use cairn_core::{paths, sandbox};
use rusqlite::{params, Connection};

use crate::shared::Shared;

static SHUTDOWN: AtomicBool = AtomicBool::new(false);
/// Exit non-zero so systemd (Restart=on-failure) starts a fresh daemon.
static RESTART: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    SHUTDOWN.store(true, Ordering::SeqCst);
}

fn install_signals() {
    unsafe {
        // A client that disconnects mid-reply must cause EPIPE, not kill us.
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
        let h = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
        libc::signal(libc::SIGTERM, h);
        libc::signal(libc::SIGINT, h);
        libc::signal(libc::SIGHUP, h);
    }
}

fn init_logging() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_env("CAIRN_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    // stderr only: under systemd it goes to the journal, which enforces its
    // own size limits. We never write an unbounded log file (D-030).
    let _ = tracing_subscriber::fmt().with_env_filter(filter).with_target(false).with_writer(std::io::stderr).try_init();
}

fn lock_instance(data_dir: &Path) -> Result<std::fs::File> {
    let f = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(data_dir.join("cairnd.lock"))?;
    if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        bail!("another cairnd is already running for this user");
    }
    Ok(f)
}

/// Move a damaged index aside (never delete it) and start fresh.
fn quarantine(data_dir: &Path, why: &str) -> Result<()> {
    let dir = data_dir.join(format!("quarantine-{}", jiff::Timestamp::now().as_second()));
    paths::ensure_private_dir(&dir)?;
    for f in ["index.db", "index.db-wal", "index.db-shm", "text.vec", "clip.vec"] {
        let p = data_dir.join(f);
        if p.exists() {
            std::fs::rename(&p, dir.join(f))?;
        }
    }
    tracing::warn!(reason = why, moved_to = %dir.display(), "index quarantined; rebuilding from scratch (your files are untouched)");
    Ok(())
}

fn import_exclusions(data_dir: &Path, conn: &Connection) -> Result<()> {
    let Ok(bytes) = std::fs::read(data_dir.join("exclusions.json")) else { return Ok(()) };
    let v: serde_json::Value = serde_json::from_slice(&bytes)?;
    let Some(list) = v.get("hex_paths").and_then(|l| l.as_array()) else { return Ok(()) };
    for h in list.iter().filter_map(|x| x.as_str()) {
        let b: Option<Vec<u8>> = (0..h.len() / 2).map(|i| u8::from_str_radix(h.get(i * 2..i * 2 + 2)?, 16).ok()).collect();
        if let Some(b) = b {
            conn.execute("INSERT OR IGNORE INTO exclusions(path, created_ns) VALUES (?1, ?2)", params![b, cairn_core::timeutil::now_ns()])?;
        }
    }
    tracing::info!(count = list.len(), "restored exclusions from exclusions.json");
    Ok(())
}

fn open_index(data_dir: &Path) -> Result<(PathBuf, Connection)> {
    let db_path = data_dir.join("index.db");
    let existed = db_path.exists();
    // The integrity check is O(database size) (minutes on a multi-GB index),
    // so it runs in the background after start-up (D-054). A failure there
    // leaves this marker and restarts the daemon, which quarantines here.
    let marker = data_dir.join("needs-quarantine");
    if existed && marker.exists() {
        quarantine(data_dir, "background integrity check failed")?;
        let _ = std::fs::remove_file(&marker);
    } else if existed && db::open(&db_path).is_err() {
        quarantine(data_dir, "cannot open index")?;
    }
    let mut conn = db::open(&db_path)?;
    if !db::init_schema(&conn)? {
        drop(conn);
        quarantine(data_dir, "schema version changed")?;
        conn = db::open(&db_path)?;
        db::init_schema(&conn)?;
    }
    let fresh: i64 = conn.query_row("SELECT count(*) FROM nodes", [], |r| r.get(0))?;
    if fresh == 0 {
        import_exclusions(data_dir, &conn)?;
    }
    for (space, dim) in [(db::SPACE_TEXT, cairn_ml::TEXT_DIM), (db::SPACE_CLIP, cairn_ml::CLIP_DIM)] {
        conn.execute("INSERT OR IGNORE INTO segments(space, dim, committed_slots) VALUES (?1, ?2, 0)", params![space, dim as i64])?;
    }
    Ok((db_path, conn))
}

/// Open a vector space, rebuilding it (re-queueing its jobs) if the file and
/// the database disagree.
fn open_space(conn: &Connection, data_dir: &Path, space: i64, dim: usize, file: &str) -> Result<VecStore> {
    let path = data_dir.join(file);
    // A different embedding model makes old vectors meaningless: start the
    // space over (re-queueing its jobs) instead of mixing models (D-021).
    if space == db::SPACE_TEXT && db::meta_get(conn, "text_model")?.as_deref() != Some(cairn_ml::TEXT_MODEL_TAG) {
        let had: i64 = conn.query_row("SELECT committed_slots FROM segments WHERE space=?1", [space], |r| r.get(0))?;
        if had > 0 {
            tracing::info!(model = cairn_ml::TEXT_MODEL_TAG, "text model changed; re-embedding text in the background");
            reset_space(conn, &path, space)?;
        }
        db::meta_set(conn, "text_model", cairn_ml::TEXT_MODEL_TAG)?;
    }
    let committed: i64 = conn.query_row("SELECT committed_slots FROM segments WHERE space=?1", [space], |r| r.get(0))?;
    let mut store = match VecStore::open(&path, dim, committed as u64) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(space, error = %format!("{e:#}"), "vector file inconsistent; re-embedding this space");
            reset_space(conn, &path, space)?;
            VecStore::open(&path, dim, 0)?
        }
    };
    let mut st = conn.prepare("SELECT id, slot FROM vecs WHERE space=?1 AND alive=1")?;
    let rows: Vec<(i64, u64)> = st.query_map([space], |r| Ok((r.get(0)?, r.get::<_, i64>(1)? as u64)))?.collect::<rusqlite::Result<_>>()?;
    store.set_live(rows);
    Ok(store)
}

fn reset_space(conn: &Connection, path: &Path, space: i64) -> Result<()> {
    let tx = db::write_tx(conn)?;
    tx.execute("DELETE FROM vecs WHERE space=?1", [space])?;
    tx.execute("UPDATE segments SET committed_slots=0 WHERE space=?1", [space])?;
    if space == db::SPACE_TEXT {
        tx.execute("INSERT OR IGNORE INTO jobs(node_id, stage, fp) SELECT DISTINCT c.node_id, ?1, n.fp FROM chunks c JOIN nodes n ON n.id=c.node_id", [Stage::EmbedText as i64])?;
        tx.execute("DELETE FROM folder_ctx", [])?;
        tx.execute("INSERT OR IGNORE INTO jobs(node_id, stage, fp) SELECT id, ?1, fp FROM nodes WHERE is_dir=1 AND parent IS NOT NULL", [Stage::FolderCtx as i64])?;
    } else {
        tx.execute("INSERT OR IGNORE INTO jobs(node_id, stage, fp) SELECT id, ?1, fp FROM nodes WHERE category='image'", [Stage::ImageMeta as i64])?;
    }
    tx.commit()?;
    let _ = std::fs::remove_file(path);
    Ok(())
}

pub fn run() -> Result<()> {
    init_logging();
    install_signals();
    let data_dir = paths::data_dir()?;
    let runtime_dir = paths::runtime_dir()?;
    let models_dir = paths::models_dir()?;
    paths::ensure_private_dir(&data_dir)?;
    paths::ensure_private_dir(&runtime_dir)?;
    paths::ensure_private_dir(&models_dir)?;
    let _lock = lock_instance(&data_dir)?;

    let enforcement = sandbox::restrict_writes(&[&data_dir, &runtime_dir, Path::new("/dev/null")]).context("applying Landlock sandbox")?;
    let sandbox_desc = format!("landlock: {enforcement:?} (writes only to {} and {})", data_dir.display(), runtime_dir.display());
    if enforcement == sandbox::Enforcement::Unsupported {
        tracing::warn!("Landlock unavailable: the daemon cannot be kernel-restricted from writing outside its data dir");
    }
    tracing::info!("{sandbox_desc}");

    let (db_path, conn) = open_index(&data_dir)?;
    let text_vecs = open_space(&conn, &data_dir, db::SPACE_TEXT, cairn_ml::TEXT_DIM, "text.vec")?;
    let clip_vecs = open_space(&conn, &data_dir, db::SPACE_CLIP, cairn_ml::CLIP_DIM, "clip.vec")?;
    drop(conn);

    let roots = paths::index_roots()?;
    let sh = Shared {
        data_dir: data_dir.clone(),
        db_path,
        models_dir: models_dir.clone(),
        roots,
        text_vecs: RwLock::new(text_vecs),
        clip_vecs: RwLock::new(clip_vecs),
        text_model: Mutex::new(None),
        clip_model: Mutex::new(None),
        shutdown: AtomicBool::new(false),
        paused: AtomicBool::new(std::env::var_os("CAIRN_START_PAUSED").is_some()),
        scanning: AtomicBool::new(false),
        initial_scan_done: AtomicBool::new(false),
        throttle: Mutex::new(None),
        sandbox: Mutex::new(sandbox_desc),
        stats: Mutex::new(crate::shared::Stats::default()),
        scan_done_at: Mutex::new(None),
        stats_signal: (Mutex::new(false), Condvar::new()),
        dirty: Mutex::new(HashSet::new()),
        full_rescan: AtomicBool::new(false),
        excl_version: std::sync::atomic::AtomicU64::new(0),
        work: (Mutex::new(false), Condvar::new()),
        scan_signal: (Mutex::new(false), Condvar::new()),
    };

    let sock = paths::socket_path()?;
    if sock.exists() {
        // We hold the instance lock, so any existing socket is stale.
        std::fs::remove_file(&sock)?;
    }
    let listener = std::os::unix::net::UnixListener::bind(&sock).with_context(|| format!("binding {}", sock.display()))?;
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600))?;
    }

    let (inotify, watchset) = crate::watcher::create()?;
    tracing::info!(roots = ?sh.roots, watch_budget = crate::watcher::budget(), "cairnd starting");

    std::thread::scope(|s| -> Result<()> {
        let sh = &sh;
        let ws = &watchset;
        s.spawn(move || {
            // Query-side models load in the background; until then search
            // uses names and full text only.
            if cairn_ml::verify(&sh.models_dir, &cairn_ml::TEXT_MODEL).is_ok() {
                match cairn_ml::TextEmbedder::load(&sh.models_dir) {
                    Ok(m) => *sh.text_model.lock().unwrap() = Some(m),
                    Err(e) => tracing::error!(error = %format!("{e:#}"), "loading text model"),
                }
            }
            if cairn_ml::verify(&sh.models_dir, &cairn_ml::CLIP_MODEL).is_ok() {
                match cairn_ml::ClipText::load(&sh.models_dir) {
                    Ok(m) => *sh.clip_model.lock().unwrap() = Some(m),
                    Err(e) => tracing::error!(error = %format!("{e:#}"), "loading CLIP text model"),
                }
            }
            tracing::info!("query models ready");
        });
        let scanner = s.spawn(move || {
            let on_dir = |id: i64, p: &Path| ws.add(id, p);
            if let Err(e) = crate::scanner::scanner_thread(sh, &on_dir) {
                tracing::error!(error = %format!("{e:#}"), "scanner stopped");
            }
        });
        let watcher = s.spawn(move || {
            if let Err(e) = crate::watcher::watcher_thread(sh, inotify, ws) {
                tracing::error!(error = %format!("{e:#}"), "watcher stopped");
            }
        });
        let pipeline = s.spawn(move || {
            if let Err(e) = crate::pipeline::pipeline_thread(sh, ) {
                tracing::error!(error = %format!("{e:#}"), "pipeline stopped");
            }
        });
        let server = s.spawn(move || {
            if let Err(e) = crate::server::server_thread(sh, listener) {
                tracing::error!(error = %format!("{e:#}"), "server stopped");
            }
        });
        s.spawn(move || {
            cairn_core::sandbox::set_thread_idle();
            std::thread::sleep(Duration::from_secs(30));
            let ok = sh.conn().and_then(|c| db::quick_check(&c));
            match ok {
                Ok(true) => tracing::info!("background integrity check passed"),
                Ok(false) | Err(_) => {
                    tracing::error!("index integrity check FAILED; restarting to rebuild it (your files are untouched)");
                    let _ = std::fs::write(sh.data_dir.join("needs-quarantine"), b"1");
                    SHUTDOWN.store(true, Ordering::SeqCst);
                    RESTART.store(true, Ordering::SeqCst);
                }
            }
        });
        s.spawn(move || {
            cairn_core::sandbox::set_thread_idle();
            while !sh.is_shutdown() {
                let started = std::time::Instant::now();
                if let Ok(c) = sh.conn() {
                    let one = |q: &str| -> i64 { c.query_row(q, [], |r| r.get(0)).unwrap_or(0) };
                    let mut st = crate::shared::Stats {
                        files: one("SELECT count(*) FROM nodes WHERE is_dir=0"),
                        dirs: one("SELECT count(*) FROM nodes WHERE is_dir=1"),
                        chunks: one("SELECT max(id) FROM chunks"), // approximate, O(1)
                        failed: one("SELECT count(*) FROM errors"),
                        pending: Vec::new(),
                        fresh: true,
                        computed_at: Some(started),
                    };
                    if let Ok(mut q) = c.prepare("SELECT stage, count(*) FROM jobs GROUP BY stage ORDER BY stage") {
                        st.pending = q
                            .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))
                            .map(|it| it.filter_map(|r| r.ok()).map(|(s, n)| (db::Stage::from_i64(s).map(|s| format!("{s:?}")).unwrap_or_else(|| format!("stage{s}")), n)).collect())
                            .unwrap_or_default();
                    }
                    *sh.stats.lock().unwrap() = st;
                }
                sh.wait_on(&sh.stats_signal, 20_000);
            }
        });
        // Maintenance: WAL checkpoints keep the -wal file from growing.
        let mut ticks = 0u64;
        while !SHUTDOWN.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(200));
            ticks += 1;
            if ticks % 3000 == 0 {
                if let Ok(c) = sh.conn() {
                    let _ = c.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
                }
            }
        }
        tracing::info!("shutting down");
        sh.shutdown.store(true, Ordering::SeqCst);
        sh.notify_work();
        sh.notify_scan();
        for h in [scanner, watcher, pipeline, server] {
            let _ = h.join();
        }
        Ok(())
    })?;
    if let Ok(c) = sh.conn() {
        let _ = c.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
    }
    let _ = std::fs::remove_file(&sock);
    if RESTART.load(Ordering::SeqCst) {
        anyhow::bail!("restarting to rebuild the index");
    }
    tracing::info!("stopped cleanly");
    Ok(())
}
