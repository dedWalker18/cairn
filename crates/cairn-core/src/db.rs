//! SQLite schema and data access.
//!
//! The database is a *rebuildable cache* of the filesystem plus a tiny amount
//! of user intent (exclusions), which is mirrored to a plain file so a rebuild
//! never loses it. WAL mode, synchronous=NORMAL: a power cut may lose the last
//! transactions but never corrupts the file.

use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};

use crate::chunk::Chunk;
use crate::classify::Category;

/// Bump when the derived-data layout changes. A mismatch rebuilds the index
/// (keeping exclusions) instead of attempting a fragile in-place migration.
pub const SCHEMA_VERSION: i64 = 1;

pub const SPACE_TEXT: i64 = 1;
pub const SPACE_CLIP: i64 = 2;

pub const VK_CHUNK: i64 = 0;
pub const VK_IMAGE: i64 = 1;
pub const VK_FOLDER: i64 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i64)]
pub enum Stage {
    ExtractText = 1,
    EmbedText = 2,
    ImageMeta = 3,
    ImageEmbed = 4,
    FolderCtx = 5,
}

impl Stage {
    pub fn from_i64(v: i64) -> Option<Stage> {
        Some(match v {
            1 => Stage::ExtractText,
            2 => Stage::EmbedText,
            3 => Stage::ImageMeta,
            4 => Stage::ImageEmbed,
            5 => Stage::FolderCtx,
            _ => return None,
        })
    }
}

pub const FLAG_SECRET: i64 = 1;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS nodes(
  id INTEGER PRIMARY KEY,
  parent INTEGER,
  name BLOB NOT NULL,
  is_dir INTEGER NOT NULL,
  dev INTEGER NOT NULL,
  ino INTEGER NOT NULL,
  size INTEGER NOT NULL,
  mtime_ns INTEGER NOT NULL,
  ctime_ns INTEGER NOT NULL,
  btime_ns INTEGER,
  category TEXT NOT NULL,
  ext TEXT NOT NULL,
  flags INTEGER NOT NULL DEFAULT 0,
  taken_civil TEXT,
  taken_offset INTEGER,
  fp BLOB NOT NULL,
  UNIQUE(parent, name)
);
CREATE UNIQUE INDEX IF NOT EXISTS nodes_root ON nodes(name) WHERE parent IS NULL;
CREATE INDEX IF NOT EXISTS nodes_ino ON nodes(dev, ino);
CREATE INDEX IF NOT EXISTS nodes_cat ON nodes(category);
CREATE INDEX IF NOT EXISTS nodes_taken ON nodes(taken_civil) WHERE taken_civil IS NOT NULL;

CREATE TABLE IF NOT EXISTS jobs(
  node_id INTEGER NOT NULL,
  stage INTEGER NOT NULL,
  fp BLOB NOT NULL,
  attempts INTEGER NOT NULL DEFAULT 0,
  not_before INTEGER NOT NULL DEFAULT 0,
  last_error TEXT,
  PRIMARY KEY(node_id, stage)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS jobs_stage ON jobs(stage, not_before);

CREATE TABLE IF NOT EXISTS chunks(
  id INTEGER PRIMARY KEY,
  node_id INTEGER NOT NULL,
  ord INTEGER NOT NULL,
  line_start INTEGER NOT NULL,
  line_end INTEGER NOT NULL,
  text TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS chunks_node ON chunks(node_id);
CREATE VIRTUAL TABLE IF NOT EXISTS chunks_fts USING fts5(
  text, content='chunks', content_rowid='id',
  tokenize="unicode61 remove_diacritics 2 separators '._-/'"
);
CREATE TRIGGER IF NOT EXISTS chunks_ai AFTER INSERT ON chunks BEGIN
  INSERT INTO chunks_fts(rowid, text) VALUES (new.id, new.text);
END;
CREATE TRIGGER IF NOT EXISTS chunks_ad AFTER DELETE ON chunks BEGIN
  INSERT INTO chunks_fts(chunks_fts, rowid, text) VALUES ('delete', old.id, old.text);
END;

-- rowid = node id. Files: name tokens. Folders: name tokens + context text.
CREATE VIRTUAL TABLE IF NOT EXISTS names_fts USING fts5(
  name, context, prefix='2 3 4',
  tokenize="unicode61 remove_diacritics 2 separators '._-/'"
);

CREATE TABLE IF NOT EXISTS folder_ctx(
  node_id INTEGER PRIMARY KEY,
  text TEXT NOT NULL,
  hash BLOB NOT NULL
);

CREATE TABLE IF NOT EXISTS vecs(
  id INTEGER PRIMARY KEY,
  space INTEGER NOT NULL,
  kind INTEGER NOT NULL,
  owner INTEGER NOT NULL,
  node_id INTEGER NOT NULL,
  slot INTEGER NOT NULL,
  alive INTEGER NOT NULL DEFAULT 1
);
CREATE INDEX IF NOT EXISTS vecs_node ON vecs(node_id) WHERE alive = 1;
CREATE INDEX IF NOT EXISTS vecs_owner ON vecs(space, kind, owner) WHERE alive = 1;
CREATE UNIQUE INDEX IF NOT EXISTS vecs_slot ON vecs(space, slot);

CREATE TABLE IF NOT EXISTS segments(
  space INTEGER PRIMARY KEY,
  dim INTEGER NOT NULL,
  committed_slots INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS exclusions(
  path BLOB PRIMARY KEY,
  dev INTEGER,
  ino INTEGER,
  created_ns INTEGER NOT NULL
);

-- Last error per node and stage; bounded by the number of nodes, never a log.
CREATE TABLE IF NOT EXISTS errors(
  node_id INTEGER NOT NULL,
  stage INTEGER NOT NULL,
  message TEXT NOT NULL,
  at_ns INTEGER NOT NULL,
  PRIMARY KEY(node_id, stage)
) WITHOUT ROWID;
"#;

/// Start a write transaction with BEGIN IMMEDIATE. A DEFERRED transaction
/// that reads first and writes later cannot wait for a concurrent writer: in
/// WAL mode the read->write upgrade fails at once with SQLITE_BUSY, bypassing
/// busy_timeout (D-053). IMMEDIATE takes the write lock up front and waits.
pub fn write_tx(conn: &Connection) -> Result<rusqlite::Transaction<'_>> {
    Ok(rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?)
}

pub fn open(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
    configure(&conn)?;
    Ok(conn)
}

pub fn configure(conn: &Connection) -> Result<()> {
    conn.busy_timeout(std::time::Duration::from_secs(10))?;
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA synchronous=NORMAL;
         PRAGMA foreign_keys=OFF;
         PRAGMA temp_store=MEMORY;
         PRAGMA cache_size=-16000;
         PRAGMA mmap_size=0;
         PRAGMA journal_size_limit=67108864;
         PRAGMA wal_autocheckpoint=1000;",
    )?;
    Ok(())
}

/// Create tables. Returns false when an existing database has a different
/// schema version (caller rebuilds).
pub fn init_schema(conn: &Connection) -> Result<bool> {
    conn.execute_batch(SCHEMA)?;
    let v: Option<String> = conn.query_row("SELECT value FROM meta WHERE key='schema_version'", [], |r| r.get(0)).optional()?;
    migrate(conn)?;
    match v {
        None => {
            conn.execute("INSERT INTO meta(key, value) VALUES('schema_version', ?1)", [SCHEMA_VERSION.to_string()])?;
            Ok(true)
        }
        Some(v) => Ok(v == SCHEMA_VERSION.to_string()),
    }
}

/// In-place migrations that do not need a re-index.
fn migrate(conn: &Connection) -> Result<()> {
    // m1: prefix index on names_fts (fast "invoice" -> "invoices").
    if meta_get(conn, "m1_names_prefix")?.is_none() {
        let sql: String = conn.query_row("SELECT sql FROM sqlite_master WHERE name='names_fts'", [], |r| r.get(0))?;
        if !sql.contains("prefix=") {
            conn.execute_batch(
                "BEGIN IMMEDIATE;
                 CREATE VIRTUAL TABLE names_fts_new USING fts5(name, context, prefix='2 3 4',
                   tokenize=\"unicode61 remove_diacritics 2 separators '._-/'\");
                 INSERT INTO names_fts_new(rowid, name, context) SELECT rowid, name, context FROM names_fts;
                 DROP TABLE names_fts;
                 ALTER TABLE names_fts_new RENAME TO names_fts;
                 COMMIT;",
            )?;
        }
        meta_set(conn, "m1_names_prefix", "1")?;
    }
    Ok(())
}

/// `PRAGMA quick_check`; true when the database is healthy.
pub fn quick_check(conn: &Connection) -> Result<bool> {
    let r: String = conn.query_row("PRAGMA quick_check", [], |r| r.get(0))?;
    Ok(r == "ok")
}

#[derive(Debug, Clone)]
pub struct NodeRow {
    pub id: i64,
    pub parent: Option<i64>,
    pub name: Vec<u8>,
    pub is_dir: bool,
    pub dev: u64,
    pub ino: u64,
    pub size: i64,
    pub mtime_ns: i64,
    pub ctime_ns: i64,
    pub btime_ns: Option<i64>,
    pub category: Category,
    pub ext: String,
    pub flags: i64,
    pub taken_civil: Option<String>,
    pub fp: Vec<u8>,
}

pub const NODE_COLS: &str = "id, parent, name, is_dir, dev, ino, size, mtime_ns, ctime_ns, btime_ns, category, ext, flags, taken_civil, fp";

pub fn node_from_row(r: &rusqlite::Row) -> rusqlite::Result<NodeRow> {
    let cat: String = r.get(10)?;
    Ok(NodeRow {
        id: r.get(0)?,
        parent: r.get(1)?,
        name: r.get(2)?,
        is_dir: r.get::<_, i64>(3)? != 0,
        dev: r.get::<_, i64>(4)? as u64,
        ino: r.get::<_, i64>(5)? as u64,
        size: r.get(6)?,
        mtime_ns: r.get(7)?,
        ctime_ns: r.get(8)?,
        btime_ns: r.get(9)?,
        category: Category::parse(&cat).unwrap_or(Category::Other),
        ext: r.get(11)?,
        flags: r.get(12)?,
        taken_civil: r.get(13)?,
        fp: r.get(14)?,
    })
}

pub fn get_node(conn: &Connection, id: i64) -> Result<Option<NodeRow>> {
    Ok(conn
        .query_row(&format!("SELECT {NODE_COLS} FROM nodes WHERE id=?1"), [id], node_from_row)
        .optional()?)
}

pub fn children(conn: &Connection, parent: i64) -> Result<Vec<NodeRow>> {
    let mut st = conn.prepare_cached(&format!("SELECT {NODE_COLS} FROM nodes WHERE parent=?1"))?;
    let rows = st.query_map([parent], node_from_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub fn root_id(conn: &Connection, root_path: &[u8]) -> Result<Option<i64>> {
    Ok(conn
        .query_row("SELECT id FROM nodes WHERE parent IS NULL AND name=?1", [root_path], |r| r.get(0))
        .optional()?)
}

/// Full path of a node, rebuilt from its ancestors. Roots store full paths.
pub fn path_of(conn: &Connection, id: i64) -> Result<Option<Vec<u8>>> {
    let mut st = conn.prepare_cached(
        "WITH RECURSIVE up(id, parent, name, depth) AS (
             SELECT id, parent, name, 0 FROM nodes WHERE id=?1
             UNION ALL
             SELECT n.id, n.parent, n.name, up.depth+1 FROM nodes n JOIN up ON n.id = up.parent WHERE up.depth < 4096
         ) SELECT name FROM up ORDER BY depth DESC",
    )?;
    let parts: Vec<Vec<u8>> = st.query_map([id], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
    if parts.is_empty() {
        return Ok(None);
    }
    let mut out = Vec::new();
    for (i, p) in parts.iter().enumerate() {
        if i > 0 && out.last() != Some(&b'/') {
            out.push(b'/');
        }
        out.extend_from_slice(p);
    }
    Ok(Some(out))
}

/// Ids of a node's ancestors, nearest first (excluding the node itself).
pub fn ancestors(conn: &Connection, id: i64, max: usize) -> Result<Vec<i64>> {
    let mut out = Vec::new();
    let mut cur = id;
    let mut st = conn.prepare_cached("SELECT parent FROM nodes WHERE id=?1")?;
    while out.len() < max {
        let p: Option<Option<i64>> = st.query_row([cur], |r| r.get(0)).optional()?;
        match p.flatten() {
            Some(p) => {
                out.push(p);
                cur = p;
            }
            None => break,
        }
    }
    Ok(out)
}

/// Make a file name searchable: split camelCase and separators.
pub fn name_tokens(name: &[u8]) -> String {
    let s = String::from_utf8_lossy(name);
    let mut out = String::with_capacity(s.len() + 8);
    let mut prev: Option<char> = None;
    for c in s.chars() {
        if let Some(p) = prev {
            // Split camelCase only; "Q4" and "IMG1234" stay whole tokens.
            if p.is_lowercase() && c.is_uppercase() {
                out.push(' ');
            }
        }
        out.push(if c == '_' || c == '-' || c == '.' { ' ' } else { c });
        prev = Some(c);
    }
    out
}

pub struct NewNode<'a> {
    pub parent: Option<i64>,
    pub name: &'a [u8],
    pub is_dir: bool,
    pub dev: u64,
    pub ino: u64,
    pub size: i64,
    pub mtime_ns: i64,
    pub ctime_ns: i64,
    pub btime_ns: Option<i64>,
    pub category: Category,
    pub ext: &'a str,
    pub flags: i64,
    pub fp: &'a [u8],
}

pub fn insert_node(tx: &Connection, n: &NewNode) -> Result<i64> {
    tx.prepare_cached(
        "INSERT INTO nodes(parent, name, is_dir, dev, ino, size, mtime_ns, ctime_ns, btime_ns, category, ext, flags, fp)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
    )?
    .execute(params![
        n.parent,
        n.name,
        n.is_dir as i64,
        n.dev as i64,
        n.ino as i64,
        n.size,
        n.mtime_ns,
        n.ctime_ns,
        n.btime_ns,
        n.category.as_str(),
        n.ext,
        n.flags,
        n.fp
    ])?;
    let id = tx.last_insert_rowid();
    tx.prepare_cached("INSERT INTO names_fts(rowid, name, context) VALUES (?1, ?2, '')")?
        .execute(params![id, name_tokens(n.name)])?;
    Ok(id)
}

pub fn update_node_stat(tx: &Connection, id: i64, n: &NewNode) -> Result<()> {
    tx.prepare_cached(
        "UPDATE nodes SET dev=?2, ino=?3, size=?4, mtime_ns=?5, ctime_ns=?6, btime_ns=?7, category=?8, ext=?9, flags=?10, fp=?11 WHERE id=?1",
    )?
    .execute(params![
        id,
        n.dev as i64,
        n.ino as i64,
        n.size,
        n.mtime_ns,
        n.ctime_ns,
        n.btime_ns,
        n.category.as_str(),
        n.ext,
        n.flags,
        n.fp
    ])?;
    Ok(())
}

/// Rename/move: keeps the node id, so content vectors survive.
pub fn move_node(tx: &Connection, id: i64, new_parent: i64, new_name: &[u8]) -> Result<()> {
    tx.prepare_cached("UPDATE nodes SET parent=?2, name=?3 WHERE id=?1")?.execute(params![id, new_parent, new_name])?;
    let ctx: String = tx
        .query_row("SELECT context FROM names_fts WHERE rowid=?1", [id], |r| r.get(0))
        .optional()?
        .unwrap_or_default();
    tx.prepare_cached("DELETE FROM names_fts WHERE rowid=?1")?.execute([id])?;
    tx.prepare_cached("INSERT INTO names_fts(rowid, name, context) VALUES (?1, ?2, ?3)")?
        .execute(params![id, name_tokens(new_name), ctx])?;
    Ok(())
}

/// All node ids in a subtree, including the root of it.
pub fn subtree_ids(conn: &Connection, id: i64) -> Result<Vec<i64>> {
    let mut st = conn.prepare_cached(
        "WITH RECURSIVE sub(id) AS (SELECT ?1 UNION ALL SELECT n.id FROM nodes n JOIN sub ON n.parent = sub.id)
         SELECT id FROM sub",
    )?;
    let ids = st.query_map([id], |r| r.get(0))?.collect::<rusqlite::Result<Vec<i64>>>()?;
    Ok(ids)
}

/// (space, slot) of a tombstoned vector, so the in-memory index can drop it.
pub type DeadVec = (i64, u64);

/// Remove nodes and everything derived from them. Vectors are tombstoned
/// (alive=0) and returned.
pub fn delete_nodes(tx: &Connection, ids: &[i64]) -> Result<Vec<DeadVec>> {
    // Set-based through a temp table: excluding a 500k-file folder must take
    // seconds, not minutes of per-row statements holding the write lock.
    tx.execute_batch("CREATE TEMP TABLE IF NOT EXISTS del_ids(id INTEGER PRIMARY KEY); DELETE FROM del_ids;")?;
    {
        let mut ins = tx.prepare_cached("INSERT OR IGNORE INTO del_ids(id) VALUES (?1)")?;
        for &id in ids {
            ins.execute([id])?;
        }
    }
    let dead = {
        let mut st = tx.prepare("SELECT space, slot FROM vecs WHERE alive=1 AND node_id IN (SELECT id FROM del_ids)")?;
        let v = st.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)? as u64)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        v
    };
    tx.execute_batch(
        "UPDATE vecs SET alive=0 WHERE alive=1 AND node_id IN (SELECT id FROM del_ids);
         DELETE FROM chunks WHERE node_id IN (SELECT id FROM del_ids);
         DELETE FROM names_fts WHERE rowid IN (SELECT id FROM del_ids);
         DELETE FROM folder_ctx WHERE node_id IN (SELECT id FROM del_ids);
         DELETE FROM jobs WHERE node_id IN (SELECT id FROM del_ids);
         DELETE FROM errors WHERE node_id IN (SELECT id FROM del_ids);
         DELETE FROM nodes WHERE id IN (SELECT id FROM del_ids);
         DELETE FROM del_ids;",
    )?;
    Ok(dead)
}

pub fn enqueue(tx: &Connection, node_id: i64, stage: Stage, fp: &[u8]) -> Result<()> {
    enqueue_at(tx, node_id, stage, fp, 0)
}

/// Enqueue (or reset) a job that may not run before `not_before` (ns).
pub fn enqueue_at(tx: &Connection, node_id: i64, stage: Stage, fp: &[u8], not_before: i64) -> Result<()> {
    tx.prepare_cached(
        "INSERT INTO jobs(node_id, stage, fp, attempts, not_before) VALUES (?1, ?2, ?3, 0, ?4)
         ON CONFLICT(node_id, stage) DO UPDATE SET fp=excluded.fp, attempts=0, not_before=excluded.not_before, last_error=NULL",
    )?
    .execute(params![node_id, stage as i64, fp, not_before])?;
    Ok(())
}

#[derive(Debug, Clone)]
pub struct Job {
    pub node_id: i64,
    pub stage: Stage,
    pub fp: Vec<u8>,
    pub attempts: i64,
}

pub fn next_jobs(conn: &Connection, stage: Stage, now_ns: i64, limit: usize) -> Result<Vec<Job>> {
    let mut st = conn.prepare_cached(
        "SELECT node_id, fp, attempts FROM jobs WHERE stage=?1 AND not_before<=?2 ORDER BY not_before, node_id LIMIT ?3",
    )?;
    let jobs = st
        .query_map(params![stage as i64, now_ns, limit as i64], |r| {
            Ok(Job { node_id: r.get(0)?, stage, fp: r.get(1)?, attempts: r.get(2)? })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(jobs)
}

pub fn job_done(tx: &Connection, node_id: i64, stage: Stage, fp: &[u8]) -> Result<()> {
    // Only delete if no newer version of the job was enqueued meanwhile.
    tx.prepare_cached("DELETE FROM jobs WHERE node_id=?1 AND stage=?2 AND fp=?3")?
        .execute(params![node_id, stage as i64, fp])?;
    tx.prepare_cached("DELETE FROM errors WHERE node_id=?1 AND stage=?2")?.execute(params![node_id, stage as i64])?;
    Ok(())
}

pub const MAX_ATTEMPTS: i64 = 3;

/// Record a failure. After MAX_ATTEMPTS the job is dropped (poison file) and
/// only the error row remains; it is retried when the file changes.
pub fn job_failed(conn: &Connection, job: &Job, err: &str, now_ns: i64) -> Result<()> {
    let msg: String = err.chars().take(500).collect();
    let attempts = job.attempts + 1;
    if attempts >= MAX_ATTEMPTS {
        conn.execute("DELETE FROM jobs WHERE node_id=?1 AND stage=?2 AND fp=?3", params![job.node_id, job.stage as i64, job.fp])?;
    } else {
        let backoff = 60_000_000_000i64 * (1 << (attempts * 2)); // 4 min, 16 min
        conn.execute(
            "UPDATE jobs SET attempts=?3, not_before=?4, last_error=?5 WHERE node_id=?1 AND stage=?2 AND fp=?6",
            params![job.node_id, job.stage as i64, attempts, now_ns + backoff, msg, job.fp],
        )?;
    }
    conn.execute(
        "INSERT INTO errors(node_id, stage, message, at_ns) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(node_id, stage) DO UPDATE SET message=excluded.message, at_ns=excluded.at_ns",
        params![job.node_id, job.stage as i64, msg, now_ns],
    )?;
    Ok(())
}

/// Tombstone a node's vectors of one kind; returns them.
pub fn kill_vecs(tx: &Connection, node_id: i64, kind: i64) -> Result<Vec<DeadVec>> {
    let dead = {
        let mut st = tx.prepare_cached("SELECT space, slot FROM vecs WHERE node_id=?1 AND kind=?2 AND alive=1")?;
        let v = st.query_map(params![node_id, kind], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)? as u64)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        v
    };
    tx.prepare_cached("UPDATE vecs SET alive=0 WHERE node_id=?1 AND kind=?2 AND alive=1")?.execute(params![node_id, kind])?;
    Ok(dead)
}

/// Replace a node's chunks. Returns tombstoned vectors of the old chunks.
pub fn replace_chunks(tx: &Connection, node_id: i64, chunks: &[Chunk]) -> Result<Vec<DeadVec>> {
    let dead = kill_vecs(tx, node_id, VK_CHUNK)?;
    tx.prepare_cached("DELETE FROM chunks WHERE node_id=?1")?.execute([node_id])?;
    let mut ins = tx.prepare_cached("INSERT INTO chunks(node_id, ord, line_start, line_end, text) VALUES (?1, ?2, ?3, ?4, ?5)")?;
    for (i, c) in chunks.iter().enumerate() {
        ins.execute(params![node_id, i as i64, c.line_start, c.line_end, c.text])?;
    }
    Ok(dead)
}

pub fn set_folder_context(tx: &Connection, node_id: i64, name: &[u8], text: &str, hash: &[u8]) -> Result<()> {
    tx.prepare_cached(
        "INSERT INTO folder_ctx(node_id, text, hash) VALUES (?1, ?2, ?3)
         ON CONFLICT(node_id) DO UPDATE SET text=excluded.text, hash=excluded.hash",
    )?
    .execute(params![node_id, text, hash])?;
    tx.prepare_cached("DELETE FROM names_fts WHERE rowid=?1")?.execute([node_id])?;
    tx.prepare_cached("INSERT INTO names_fts(rowid, name, context) VALUES (?1, ?2, ?3)")?
        .execute(params![node_id, name_tokens(name), text])?;
    Ok(())
}

pub fn meta_get(conn: &Connection, key: &str) -> Result<Option<String>> {
    Ok(conn.query_row("SELECT value FROM meta WHERE key=?1", [key], |r| r.get(0)).optional()?)
}

pub fn meta_set(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO meta(key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        params![key, value],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        configure(&c).unwrap();
        assert!(init_schema(&c).unwrap());
        c
    }

    fn nn<'a>(parent: Option<i64>, name: &'a [u8], is_dir: bool) -> NewNode<'a> {
        NewNode {
            parent,
            name,
            is_dir,
            dev: 1,
            ino: 1,
            size: 0,
            mtime_ns: 0,
            ctime_ns: 0,
            btime_ns: None,
            category: if is_dir { Category::Dir } else { Category::Text },
            ext: "",
            flags: 0,
            fp: b"x",
        }
    }

    #[test]
    fn paths_moves_and_deletes() {
        let mut c = mem();
        let tx = c.transaction().unwrap();
        let root = insert_node(&tx, &nn(None, b"/home/u", true)).unwrap();
        let a = insert_node(&tx, &nn(Some(root), b"Trips", true)).unwrap();
        let b = insert_node(&tx, &nn(Some(a), b"caf\xe9.txt", false)).unwrap();
        tx.commit().unwrap();
        assert_eq!(path_of(&c, b).unwrap().unwrap(), b"/home/u/Trips/caf\xe9.txt".to_vec());
        assert_eq!(ancestors(&c, b, 8).unwrap(), vec![a, root]);

        let tx = c.transaction().unwrap();
        let d = insert_node(&tx, &nn(Some(root), b"Archive", true)).unwrap();
        move_node(&tx, a, d, b"Manali Dec 2025").unwrap();
        tx.commit().unwrap();
        assert_eq!(path_of(&c, b).unwrap().unwrap(), b"/home/u/Archive/Manali Dec 2025/caf\xe9.txt".to_vec());
        let hits: i64 = c.query_row("SELECT count(*) FROM names_fts WHERE names_fts MATCH 'manali'", [], |r| r.get(0)).unwrap();
        assert_eq!(hits, 1);

        let ids = subtree_ids(&c, d).unwrap();
        assert_eq!(ids.len(), 3);
        let tx = c.transaction().unwrap();
        delete_nodes(&tx, &ids).unwrap();
        tx.commit().unwrap();
        let left: i64 = c.query_row("SELECT count(*) FROM nodes", [], |r| r.get(0)).unwrap();
        assert_eq!(left, 1);
    }

    #[test]
    fn jobs_are_idempotent() {
        let mut c = mem();
        let tx = c.transaction().unwrap();
        enqueue(&tx, 5, Stage::ExtractText, b"v1").unwrap();
        enqueue(&tx, 5, Stage::ExtractText, b"v2").unwrap();
        tx.commit().unwrap();
        let jobs = next_jobs(&c, Stage::ExtractText, 0, 10).unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].fp, b"v2");
        // finishing a stale version must not remove the newer job
        let tx = c.transaction().unwrap();
        job_done(&tx, 5, Stage::ExtractText, b"v1").unwrap();
        tx.commit().unwrap();
        assert_eq!(next_jobs(&c, Stage::ExtractText, 0, 10).unwrap().len(), 1);
        // poison after MAX_ATTEMPTS
        let mut j = jobs[0].clone();
        for _ in 0..MAX_ATTEMPTS {
            job_failed(&c, &j, "boom", 0).unwrap();
            j.attempts += 1;
        }
        assert_eq!(next_jobs(&c, Stage::ExtractText, i64::MAX, 10).unwrap().len(), 0);
    }

    #[test]
    fn chunks_fts_sync() {
        let mut c = mem();
        let tx = c.transaction().unwrap();
        replace_chunks(&tx, 7, &[Chunk { line_start: 1, line_end: 3, text: "terraform apply -auto-approve".into() }]).unwrap();
        tx.commit().unwrap();
        let n: i64 = c.query_row("SELECT count(*) FROM chunks_fts WHERE chunks_fts MATCH 'terraform'", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1);
        let tx = c.transaction().unwrap();
        replace_chunks(&tx, 7, &[]).unwrap();
        tx.commit().unwrap();
        let n: i64 = c.query_row("SELECT count(*) FROM chunks_fts WHERE chunks_fts MATCH 'terraform'", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn camel_tokens() {
        assert_eq!(name_tokens(b"SalesReportQ4_2025.xlsx"), "Sales Report Q4 2025 xlsx");
    }
}
