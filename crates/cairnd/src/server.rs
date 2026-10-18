//! Control socket. Same-user only (0700 directory, 0600 socket, SO_PEERCRED
//! check). One JSON line in, one JSON line out, bounded sizes and timeouts.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::{bail, Result};
use cairn_core::ipc::{Request, Response, Status, MAX_REQUEST_LINE};
use cairn_core::{db, paths};
use rusqlite::{params, Connection};

use crate::scanner::lookup_path;
use crate::shared::Shared;

const MAX_CLIENTS: usize = 16;

fn peer_uid_ok(s: &UnixStream) -> bool {
    use std::os::fd::AsRawFd;
    let mut cred = libc::ucred { pid: 0, uid: 0, gid: 0 };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let r = unsafe { libc::getsockopt(s.as_raw_fd(), libc::SOL_SOCKET, libc::SO_PEERCRED, &mut cred as *mut _ as *mut libc::c_void, &mut len) };
    r == 0 && cred.uid == unsafe { libc::getuid() }
}

pub fn server_thread(sh: &Shared, listener: UnixListener) -> Result<()> {
    listener.set_nonblocking(true)?;
    let active = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        while !sh.is_shutdown() {
            match listener.accept() {
                Ok((stream, _)) => {
                    if !peer_uid_ok(&stream) {
                        continue;
                    }
                    if active.load(Ordering::SeqCst) >= MAX_CLIENTS {
                        let _ = reply(&stream, &Response::Error { message: "busy".into() });
                        continue;
                    }
                    active.fetch_add(1, Ordering::SeqCst);
                    let active = &active;
                    scope.spawn(move || {
                        if let Err(e) = handle(sh, stream) {
                            tracing::debug!(error = %format!("{e:#}"), "client error");
                        }
                        active.fetch_sub(1, Ordering::SeqCst);
                    });
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(50)),
                Err(e) => {
                    tracing::warn!(error = %e, "accept failed");
                    std::thread::sleep(Duration::from_millis(200));
                }
            }
        }
    });
    Ok(())
}

fn reply(mut s: &UnixStream, r: &Response) -> Result<()> {
    let mut line = serde_json::to_vec(r)?;
    line.push(b'\n');
    s.write_all(&line)?;
    Ok(())
}

fn handle(sh: &Shared, stream: UnixStream) -> Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    let mut line = String::new();
    BufReader::new((&stream).take(MAX_REQUEST_LINE as u64)).read_line(&mut line)?;
    let resp = match serde_json::from_str::<Request>(&line) {
        Ok(req) => dispatch(sh, req).unwrap_or_else(|e| Response::Error { message: format!("{e:#}") }),
        Err(e) => Response::Error { message: format!("bad request: {e}") },
    };
    reply(&stream, &resp)
}

fn dispatch(sh: &Shared, req: Request) -> Result<Response> {
    let conn = sh.conn()?;
    match req {
        Request::Search { query, cwd, limit, scope, kinds } => {
            let kinds: Vec<cairn_core::classify::Category> = kinds.iter().filter_map(|k| cairn_core::classify::Category::parse(k)).collect();
            let r = crate::search::search(sh, &conn, &query, cwd.map(|c| c.bytes()), scope, &kinds, limit)?;
            Ok(Response::Search(r))
        }
        Request::Status => Ok(Response::Status(status(sh, &conn)?)),
        Request::Pause => {
            sh.paused.store(true, Ordering::SeqCst);
            Ok(Response::Ok { message: "indexing paused".into() })
        }
        Request::Resume => {
            sh.paused.store(false, Ordering::SeqCst);
            sh.notify_work();
            Ok(Response::Ok { message: "indexing resumed".into() })
        }
        Request::Rescan => {
            sh.full_rescan.store(true, Ordering::SeqCst);
            sh.notify_scan();
            Ok(Response::Ok { message: "full rescan scheduled".into() })
        }
        Request::Exclude { path } => exclude(sh, &conn, &path.bytes()),
        Request::Include { path } => include(sh, &conn, &path.bytes()),
    }
}

fn validate(sh: &Shared, p: &[u8]) -> Result<std::path::PathBuf> {
    let path = paths::from_bytes(p);
    if !path.is_absolute() || path.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
        bail!("path must be absolute and normalised");
    }
    if sh.roots.iter().any(|r| r == &path) {
        bail!("cannot exclude an index root; use `cairn pause` instead");
    }
    if !sh.roots.iter().any(|r| path.starts_with(r)) {
        bail!("{} is not inside an indexed root", path.display());
    }
    Ok(path)
}

fn exclude(sh: &Shared, conn: &Connection, p: &[u8]) -> Result<Response> {
    use std::os::unix::fs::MetadataExt;
    let path = validate(sh, p)?;
    let (dev, ino) = match std::fs::symlink_metadata(&path) {
        Ok(m) => (Some(m.dev() as i64), Some(m.ino() as i64)),
        Err(_) => (None, None),
    };
    let tx = db::write_tx(&conn)?;
    tx.execute(
        "INSERT INTO exclusions(path, dev, ino, created_ns) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(path) DO UPDATE SET dev=excluded.dev, ino=excluded.ino",
        params![p, dev, ino, cairn_core::timeutil::now_ns()],
    )?;
    let mut removed = 0;
    let mut dead = Vec::new();
    if let Some(id) = lookup_path(&tx, &sh.roots, p)? {
        let ids = db::subtree_ids(&tx, id)?;
        removed = ids.len();
        dead = db::delete_nodes(&tx, &ids)?;
    }
    tx.commit()?;
    sh.kill_vecs(&dead);
    sh.excl_version.fetch_add(1, Ordering::SeqCst);
    mirror_exclusions(sh, conn)?;
    Ok(Response::Ok { message: format!("excluded {} ({removed} items removed from the index)", path.display()) })
}

fn include(sh: &Shared, conn: &Connection, p: &[u8]) -> Result<Response> {
    let path = validate(sh, p)?;
    let n = conn.execute("DELETE FROM exclusions WHERE path=?1", [p])?;
    sh.excl_version.fetch_add(1, Ordering::SeqCst);
    mirror_exclusions(sh, conn)?;
    if let Some(parent) = path.parent() {
        if let Some(pid) = lookup_path(conn, &sh.roots, &paths::to_bytes(parent))? {
            sh.mark_dirty(pid);
            sh.notify_scan();
        }
    }
    let msg = if n > 0 { format!("{} will be indexed again", path.display()) } else { format!("{} was not excluded", path.display()) };
    Ok(Response::Ok { message: msg })
}

/// User intent survives an index rebuild: exclusions are mirrored to a file.
pub fn mirror_exclusions(sh: &Shared, conn: &Connection) -> Result<()> {
    let mut st = conn.prepare("SELECT path FROM exclusions ORDER BY path")?;
    let list: Vec<String> = st
        .query_map([], |r| r.get::<_, Vec<u8>>(0))?
        .map(|r| r.map(|b| b.iter().map(|x| format!("{x:02x}")).collect::<String>()))
        .collect::<rusqlite::Result<_>>()?;
    let json = serde_json::to_vec_pretty(&serde_json::json!({ "version": 1, "hex_paths": list }))?;
    paths::atomic_write(&sh.data_dir.join("exclusions.json"), &json)
}

fn status(sh: &Shared, conn: &Connection) -> Result<Status> {
    let st = sh.stats.lock().unwrap().clone();
    let mut q = conn.prepare("SELECT path FROM exclusions ORDER BY path")?;
    let exclusions = q.query_map([], |r| r.get::<_, Vec<u8>>(0))?.filter_map(|r| r.ok()).map(|b| paths::display(&b)).collect();
    let db_bytes = ["index.db", "index.db-wal"].iter().filter_map(|f| std::fs::metadata(sh.data_dir.join(f)).ok()).map(|m| m.len()).sum::<u64>()
        + std::fs::metadata(sh.text_vecs.read().unwrap().path()).map(|m| m.len()).unwrap_or(0)
        + std::fs::metadata(sh.clip_vecs.read().unwrap().path()).map(|m| m.len()).unwrap_or(0);
    Ok(Status {
        roots: sh.roots.iter().map(|r| r.display().to_string()).collect(),
        files: st.files,
        dirs: st.dirs,
        chunks: st.chunks,
        text_vectors: sh.text_vecs.read().unwrap().live() as i64,
        image_vectors: sh.clip_vecs.read().unwrap().live() as i64,
        pending: st.pending,
        failed: st.failed,
        paused: sh.paused.load(Ordering::SeqCst),
        throttle: sh.throttle.lock().unwrap().clone(),
        // "Settled" only when the counts were taken after the last full scan
        // finished; otherwise a fresh daemon could claim "idle, 0 files".
        scanning: {
            let done = *sh.scan_done_at.lock().unwrap();
            let settled = sh.initial_scan_done.load(Ordering::SeqCst) && matches!((st.computed_at, done), (Some(c), Some(d)) if c >= d);
            sh.scanning.load(Ordering::SeqCst) || !st.fresh || !settled
        },
        models: cairn_ml::ALL_MODELS.iter().map(|m| (m.id.to_string(), cairn_ml::verify(&sh.models_dir, m).is_ok())).collect(),
        sandbox: sh.sandbox.lock().unwrap().clone(),
        db_bytes,
        exclusions,
    })
}
