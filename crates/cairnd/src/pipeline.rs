//! Background job pipeline. Every job is keyed by (node, stage, fingerprint)
//! and its result commits in the same SQLite transaction that removes the
//! job (DBOS-style step checkpoint, D-020). A crash at any point re-runs at
//! most the in-flight batch; re-running is harmless because results are
//! replaced, never appended twice, and stale results (file changed while we
//! worked) are discarded by the fingerprint check.

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use cairn_core::chunk::chunk_text;
use cairn_core::classify;
use cairn_core::db::{self, Job, NodeRow, Stage};
use cairn_core::extract::{ExtractRequest, ExtractResponse};
use cairn_core::sandbox;
use cairn_core::timeutil::now_ns;
use rusqlite::{params, Connection, OptionalExtension};

use crate::shared::Shared;
use crate::worker_main::{EmbedRequest, EmbedResponse};
use crate::workers::Worker;

const EXTRACT_TIMEOUT: Duration = Duration::from_secs(90);
const EMBED_TIMEOUT: Duration = Duration::from_secs(600);
const MAX_EMBED_BATCH: usize = 48;
const FOLDER_CONTEXT_CHARS: usize = 1800;

struct Pipeline<'a> {
    sh: &'a Shared,
    conn: Connection,
    extract: Vec<Worker>,
    embed: Worker,
    text_model_ok: bool,
    clip_model_ok: bool,
    models_checked: Option<Instant>,
}

fn throttle_reason() -> Option<String> {
    if std::env::var_os("CAIRN_IGNORE_THROTTLE").is_some() {
        return None;
    }
    if sandbox::on_battery() && std::env::var_os("CAIRN_RUN_ON_BATTERY").is_none() {
        return Some("on battery".into());
    }
    if let Some(m) = sandbox::mem_available() {
        if m < 1536 << 20 {
            return Some("low memory".into());
        }
    }
    if sandbox::psi_avg10("memory").is_some_and(|v| v > 5.0) {
        return Some("memory pressure".into());
    }
    if sandbox::psi_avg10("io").is_some_and(|v| v > 25.0) {
        return Some("disk busy".into());
    }
    if sandbox::psi_avg10("cpu").is_some_and(|v| v > 40.0) {
        return Some("CPU busy".into());
    }
    None
}

pub fn pipeline_thread(sh: &Shared) -> Result<()> {
    sandbox::set_thread_idle();
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2);
    let mut p = Pipeline {
        sh,
        conn: sh.conn()?,
        extract: (0..2).map(|_| Worker::new("extract", 1)).collect(),
        embed: Worker::new("embed", (cores / 2).clamp(1, 6)),
        text_model_ok: false,
        clip_model_ok: false,
        models_checked: None,
    };
    let mut last_report = Instant::now();
    let mut done_since_report = 0u64;
    let mut last_err: Option<(String, Instant, u64)> = None;
    let mut was_idle = false;
    while !sh.is_shutdown() {
        p.reap_idle();
        if sh.paused.load(Ordering::SeqCst) {
            sh.wait_on(&sh.work, 1000);
            continue;
        }
        if let Some(reason) = throttle_reason() {
            if reason.contains("memory") {
                p.embed.kill(); // give the RAM back right away
            }
            *sh.throttle.lock().unwrap() = Some(reason);
            sh.wait_on(&sh.work, 5000);
            continue;
        }
        *sh.throttle.lock().unwrap() = None;
        p.check_models();
        match p.run_one_batch() {
            Ok(0) => {
                // Work just ran out: recount now so status says "idle" promptly.
                if done_since_report > 0 || !was_idle {
                    sh.notify_stats();
                }
                was_idle = true;
                sh.wait_on(&sh.work, 3000)
            }
            Ok(n) => {
                done_since_report += n as u64;
                was_idle = false;
            }
            Err(e) => {
                // Never let a repeating error flood the journal: one line per
                // distinct message per minute, with a repeat count.
                let msg = format!("{e:#}");
                if last_err.as_ref().is_some_and(|(m, t, _)| *m == msg && t.elapsed() < Duration::from_secs(60)) {
                    if let Some((_, _, n)) = last_err.as_mut() {
                        *n += 1;
                    }
                } else {
                    let repeats = last_err.take().map(|(_, _, n)| n).unwrap_or(0);
                    tracing::error!(error = %msg, suppressed_repeats = repeats, "pipeline batch failed");
                    last_err = Some((msg, Instant::now(), 0));
                }
                std::thread::sleep(Duration::from_secs(2));
            }
        }
        if last_report.elapsed() > Duration::from_secs(600) && done_since_report > 0 {
            tracing::info!(jobs = done_since_report, "indexing progress (last 10 min)");
            done_since_report = 0;
            last_report = Instant::now();
        }
    }
    Ok(())
}

impl Pipeline<'_> {
    fn reap_idle(&mut self) {
        for w in &mut self.extract {
            if w.running() && w.idle_for() > Duration::from_secs(60) {
                w.kill();
            }
        }
        if self.embed.running() && self.embed.idle_for() > Duration::from_secs(120) {
            self.embed.kill();
        }
    }

    fn check_models(&mut self) {
        if self.models_checked.is_some_and(|t| t.elapsed() < Duration::from_secs(600)) {
            return;
        }
        self.text_model_ok = cairn_ml::verify(&self.sh.models_dir, &cairn_ml::TEXT_MODEL).is_ok();
        self.clip_model_ok = cairn_ml::verify(&self.sh.models_dir, &cairn_ml::CLIP_MODEL).is_ok();
        if !self.text_model_ok || !self.clip_model_ok {
            tracing::warn!(text = self.text_model_ok, clip = self.clip_model_ok, "embedding models missing; run `cairn models fetch` (or install offline). Text/name search still works.");
        }
        self.models_checked = Some(Instant::now());
    }

    fn run_one_batch(&mut self) -> Result<usize> {
        let now = now_ns();
        let mut order = vec![(Stage::ExtractText, 32), (Stage::ImageMeta, 64)];
        if self.text_model_ok {
            order.push((Stage::EmbedText, 12));
            order.push((Stage::FolderCtx, 32));
        }
        if self.clip_model_ok {
            order.push((Stage::ImageEmbed, 16));
        }
        for (stage, n) in order {
            let jobs = if stage == Stage::EmbedText { next_text_jobs(&self.conn, now, n)? } else { db::next_jobs(&self.conn, stage, now, n)? };
            if jobs.is_empty() {
                continue;
            }
            let count = jobs.len();
            match stage {
                Stage::ExtractText => self.extract_text(jobs)?,
                Stage::ImageMeta => self.image_meta(jobs)?,
                Stage::EmbedText => self.embed_text(jobs)?,
                Stage::FolderCtx => self.folder_ctx(jobs)?,
                Stage::ImageEmbed => self.image_embed(jobs)?,
            }
            return Ok(count);
        }
        Ok(0)
    }

    /// Current node for a job, or None (and the job removed) if it is stale.
    fn live_node(&self, job: &Job) -> Result<Option<(NodeRow, Vec<u8>)>> {
        let node = db::get_node(&self.conn, job.node_id)?;
        match node {
            Some(n) if n.fp == job.fp => {
                let Some(path) = db::path_of(&self.conn, n.id)? else { return Ok(None) };
                Ok(Some((n, path)))
            }
            Some(_) => Ok(None), // changed: a newer job with the new fp exists
            None => {
                self.conn.execute("DELETE FROM jobs WHERE node_id=?1", [job.node_id])?;
                Ok(None)
            }
        }
    }

    /// Run extraction requests across the worker pool in parallel. `None`
    /// means "not attempted" (its worker died on an earlier file in the same
    /// batch); such jobs stay queued without counting as a failure, so one
    /// poison file cannot take innocent neighbours down with it.
    fn run_extract(&mut self, reqs: Vec<ExtractRequest>) -> Vec<Option<Result<(ExtractResponse, Vec<u8>)>>> {
        // Spawn here, on the long-lived pipeline thread (see ensure_started).
        for w in &mut self.extract {
            if let Err(e) = w.ensure_started() {
                tracing::error!(error = %format!("{e:#}"), "cannot start extract worker");
            }
        }
        let n = self.extract.len();
        let mut buckets: Vec<Vec<(usize, ExtractRequest)>> = (0..n).map(|_| Vec::new()).collect();
        for (i, r) in reqs.into_iter().enumerate() {
            buckets[i % n].push((i, r));
        }
        let mut out: Vec<(usize, Option<Result<(ExtractResponse, Vec<u8>)>>)> = Vec::new();
        std::thread::scope(|s| {
            let handles: Vec<_> = self
                .extract
                .iter_mut()
                .zip(buckets)
                .map(|(w, bucket)| {
                    s.spawn(move || {
                        sandbox::set_thread_idle();
                        let mut res = Vec::with_capacity(bucket.len());
                        for (i, r) in bucket {
                            if !w.running() {
                                res.push((i, None));
                                continue;
                            }
                            res.push((i, Some(w.call::<_, ExtractResponse>(&r, &[], EXTRACT_TIMEOUT))));
                        }
                        res
                    })
                })
                .collect();
            for h in handles {
                if let Ok(v) = h.join() {
                    out.extend(v);
                }
            }
        });
        out.sort_by_key(|(i, _)| *i);
        out.into_iter().map(|(_, r)| r).collect()
    }

    fn fail(&self, job: &Job, err: &str) -> Result<()> {
        tracing::debug!(node = job.node_id, stage = ?job.stage, error = err, "job failed");
        db::job_failed(&self.conn, job, err, now_ns())
    }

    fn extract_text(&mut self, jobs: Vec<Job>) -> Result<()> {
        let mut live = Vec::new();
        for j in jobs {
            match self.live_node(&j)? {
                Some((n, p)) => live.push((j, n, p)),
                None => continue,
            }
        }
        let reqs = live.iter().map(|(_, n, p)| ExtractRequest::Text { path: p.clone(), ext: n.ext.clone() }).collect();
        let results = self.run_extract(reqs);
        let tx = db::write_tx(&self.conn)?;
        let mut dead = Vec::new();
        let mut failures = Vec::new();
        for ((job, node, _), res) in live.into_iter().zip(results) {
            let Some(res) = res else { continue };
            let text = match res {
                Ok((ExtractResponse::Text { text }, _)) => text,
                Ok((ExtractResponse::Error { message }, _)) => {
                    failures.push((job, message));
                    continue;
                }
                Ok(_) => {
                    failures.push((job, "unexpected worker response".into()));
                    continue;
                }
                Err(e) => {
                    failures.push((job, format!("{e:#}")));
                    continue;
                }
            };
            if !still_same(&tx, &job)? {
                continue;
            }
            let chunks = chunk_text(&text);
            dead.extend(db::replace_chunks(&tx, node.id, &chunks)?);
            if !chunks.is_empty() {
                db::enqueue(&tx, node.id, Stage::EmbedText, &job.fp)?;
            }
            if classify::is_context_file(&node.name) {
                if let Some(parent) = node.parent {
                    if let Some(pfp) = tx.query_row("SELECT fp FROM nodes WHERE id=?1", [parent], |r| r.get::<_, Vec<u8>>(0)).optional()? {
                        db::enqueue_at(&tx, parent, Stage::FolderCtx, &pfp, now_ns() + 5_000_000_000)?;
                    }
                }
            }
            db::job_done(&tx, node.id, Stage::ExtractText, &job.fp)?;
        }
        tx.commit()?;
        self.sh.kill_vecs(&dead);
        for (job, msg) in failures {
            self.fail(&job, &msg)?;
        }
        Ok(())
    }

    fn image_meta(&mut self, jobs: Vec<Job>) -> Result<()> {
        let mut live = Vec::new();
        for j in jobs {
            if let Some((n, p)) = self.live_node(&j)? {
                live.push((j, n, p));
            }
        }
        let reqs = live.iter().map(|(_, _, p)| ExtractRequest::ImageMeta { path: p.clone() }).collect();
        let results = self.run_extract(reqs);
        let tx = db::write_tx(&self.conn)?;
        let mut failures = Vec::new();
        for ((job, node, _), res) in live.into_iter().zip(results) {
            let Some(res) = res else { continue };
            let meta = match res {
                Ok((ExtractResponse::ImageMeta(m), _)) => m,
                Ok((ExtractResponse::Error { message }, _)) => {
                    failures.push((job, message));
                    continue;
                }
                Ok(_) => continue,
                Err(e) => {
                    failures.push((job, format!("{e:#}")));
                    continue;
                }
            };
            if !still_same(&tx, &job)? {
                continue;
            }
            tx.execute("UPDATE nodes SET taken_civil=?2, taken_offset=?3 WHERE id=?1", params![node.id, meta.taken_civil, meta.taken_offset])?;
            if classify::has_decodable_image(&node.ext) {
                db::enqueue(&tx, node.id, Stage::ImageEmbed, &job.fp)?;
            }
            db::job_done(&tx, node.id, Stage::ImageMeta, &job.fp)?;
        }
        tx.commit()?;
        for (job, msg) in failures {
            self.fail(&job, &msg)?;
        }
        Ok(())
    }

    fn call_embed(&mut self, req: EmbedRequest, payload: &[u8], dim: usize) -> Result<Vec<Vec<f32>>> {
        self.embed.ensure_started()?;
        let (resp, bytes) = self.embed.call::<_, EmbedResponse>(&req, payload, EMBED_TIMEOUT)?;
        match resp {
            EmbedResponse::Vectors { count, dim: d } => {
                if d != dim || bytes.len() != count * d * 4 {
                    bail!("embed worker returned malformed vectors");
                }
                let floats: Vec<f32> = bytes.chunks_exact(4).map(|c| f32::from_le_bytes(c.try_into().unwrap())).collect();
                let vecs: Vec<Vec<f32>> = floats.chunks_exact(d).map(|c| c.to_vec()).collect();
                if vecs.iter().any(|v| v.iter().any(|x| !x.is_finite())) {
                    bail!("embed worker returned non-finite values");
                }
                Ok(vecs)
            }
            EmbedResponse::Error { message } => bail!("embed worker: {message}"),
        }
    }

    /// Append vectors and their rows atomically (file fsync, then DB commit,
    /// then publish to search). `rows`: (kind, owner, node_id, alive).
    fn store_vectors(&self, space: i64, vecs: &[Vec<f32>], rows: &[(i64, i64, i64, bool)], tx: &rusqlite::Transaction) -> Result<Box<dyn FnOnce() + '_>> {
        let store = if space == db::SPACE_TEXT { &self.sh.text_vecs } else { &self.sh.clip_vecs };
        let first = store.write().unwrap().write_pending(vecs)?;
        let mut ids = Vec::with_capacity(rows.len());
        let mut dead_slots = Vec::new();
        for (i, (kind, owner, node, alive)) in rows.iter().enumerate() {
            let slot = first + i as u64;
            tx.execute(
                "INSERT INTO vecs(space, kind, owner, node_id, slot, alive) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![space, kind, owner, node, slot as i64, *alive as i64],
            )?;
            ids.push(tx.last_insert_rowid());
            if !alive {
                dead_slots.push(slot);
            }
        }
        tx.execute("UPDATE segments SET committed_slots=?2 WHERE space=?1", params![space, (first + vecs.len() as u64) as i64])?;
        let vecs = vecs.to_vec();
        Ok(Box::new(move || {
            let mut s = store.write().unwrap();
            s.publish(first, &vecs, &ids);
            for d in dead_slots {
                s.kill(d);
            }
        }))
    }

    fn rollback_store(&self, space: i64) {
        let store = if space == db::SPACE_TEXT { &self.sh.text_vecs } else { &self.sh.clip_vecs };
        let _ = store.write().unwrap().rollback_pending();
    }

    fn embed_text(&mut self, jobs: Vec<Job>) -> Result<()> {
        // Gather chunks of the jobs' nodes, bounded per embed call.
        let mut items: Vec<(Job, i64, Vec<(i64, String)>)> = Vec::new();
        let mut total = 0usize;
        for j in jobs {
            let Some((node, path)) = self.live_node(&j)? else { continue };
            let label = context_label(&path);
            // Semantic vectors for the start of each file only; every chunk
            // stays keyword-searchable (D-062).
            let cap = embed_cap(&node.ext);
            let mut st = self.conn.prepare_cached("SELECT id, text FROM chunks WHERE node_id=?1 ORDER BY ord LIMIT ?2")?;
            let chunks: Vec<(i64, String)> = st
                .query_map(params![node.id, cap], |r| Ok((r.get(0)?, r.get::<_, String>(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?
                .into_iter()
                .map(|(id, t)| (id, format!("{label}\n{t}")))
                .collect();
            total += chunks.len();
            items.push((j, node.id, chunks));
            if total >= MAX_EMBED_BATCH * 4 {
                break;
            }
        }
        if items.is_empty() {
            return Ok(());
        }
        let texts: Vec<String> = items.iter().flat_map(|(_, _, c)| c.iter().map(|(_, t)| t.clone())).collect();
        let mut vecs = Vec::with_capacity(texts.len());
        for batch in texts.chunks(MAX_EMBED_BATCH) {
            match self.call_embed(EmbedRequest::Text { texts: batch.to_vec() }, &[], cairn_ml::TEXT_DIM) {
                Ok(v) => vecs.extend(v),
                Err(e) => {
                    let msg = format!("{e:#}");
                    for (j, _, _) in &items {
                        self.fail(j, &msg)?;
                    }
                    return Ok(());
                }
            }
        }
        let tx = db::write_tx(&self.conn)?;
        let mut rows = Vec::with_capacity(vecs.len());
        let mut dead = Vec::new();
        for (job, node_id, chunks) in &items {
            let same = still_same(&tx, job)?;
            if same {
                dead.extend(db::kill_vecs(&tx, *node_id, db::VK_CHUNK)?);
            }
            for (cid, _) in chunks {
                rows.push((db::VK_CHUNK, *cid, *node_id, same));
            }
        }
        let publish = match self.store_vectors(db::SPACE_TEXT, &vecs, &rows, &tx) {
            Ok(p) => p,
            Err(e) => {
                drop(tx);
                self.rollback_store(db::SPACE_TEXT);
                return Err(e);
            }
        };
        for (job, node_id, _) in &items {
            db::job_done(&tx, *node_id, Stage::EmbedText, &job.fp)?;
        }
        if let Err(e) = tx.commit() {
            drop(publish);
            self.rollback_store(db::SPACE_TEXT);
            return Err(e.into());
        }
        self.sh.kill_vecs(&dead);
        publish();
        Ok(())
    }

    fn folder_ctx(&mut self, jobs: Vec<Job>) -> Result<()> {
        let mut todo: Vec<(Job, i64, Vec<u8>, String, Vec<u8>, String)> = Vec::new();
        for j in jobs {
            let Some((node, _path)) = self.live_node(&j)? else { continue };
            if node.parent.is_none() || !node.is_dir {
                let tx = db::write_tx(&self.conn)?;
                db::job_done(&tx, node.id, Stage::FolderCtx, &j.fp)?;
                tx.commit()?;
                continue;
            }
            let (text, fts_text) = build_folder_context(&self.conn, &node)?;
            let hash = blake3::hash(text.as_bytes()).as_bytes()[..16].to_vec();
            let old: Option<Vec<u8>> = self.conn.query_row("SELECT hash FROM folder_ctx WHERE node_id=?1", [node.id], |r| r.get(0)).optional()?;
            let has_vec: bool = self.conn.query_row("SELECT EXISTS(SELECT 1 FROM vecs WHERE node_id=?1 AND kind=?2 AND alive=1)", params![node.id, db::VK_FOLDER], |r| r.get(0))?;
            if old.as_deref() == Some(&hash[..]) && has_vec {
                let tx = db::write_tx(&self.conn)?;
                db::job_done(&tx, node.id, Stage::FolderCtx, &j.fp)?;
                tx.commit()?;
                continue;
            }
            todo.push((j, node.id, node.name.clone(), text, hash, fts_text));
        }
        if todo.is_empty() {
            return Ok(());
        }
        let texts: Vec<String> = todo.iter().map(|t| t.3.clone()).collect();
        let vecs = match self.call_embed(EmbedRequest::Text { texts }, &[], cairn_ml::TEXT_DIM) {
            Ok(v) => v,
            Err(e) => {
                let msg = format!("{e:#}");
                for t in &todo {
                    self.fail(&t.0, &msg)?;
                }
                return Ok(());
            }
        };
        let tx = db::write_tx(&self.conn)?;
        let mut rows = Vec::new();
        let mut dead = Vec::new();
        for (job, id, name, _text, hash, notes) in &todo {
            let same = still_same(&tx, job)?;
            if same {
                dead.extend(db::kill_vecs(&tx, *id, db::VK_FOLDER)?);
                db::set_folder_context(&tx, *id, name, notes, hash)?;
            }
            rows.push((db::VK_FOLDER, *id, *id, same));
        }
        let publish = match self.store_vectors(db::SPACE_TEXT, &vecs, &rows, &tx) {
            Ok(p) => p,
            Err(e) => {
                drop(tx);
                self.rollback_store(db::SPACE_TEXT);
                return Err(e);
            }
        };
        for (job, id, ..) in &todo {
            db::job_done(&tx, *id, Stage::FolderCtx, &job.fp)?;
        }
        if let Err(e) = tx.commit() {
            drop(publish);
            self.rollback_store(db::SPACE_TEXT);
            return Err(e.into());
        }
        self.sh.kill_vecs(&dead);
        publish();
        Ok(())
    }

    fn image_embed(&mut self, jobs: Vec<Job>) -> Result<()> {
        let mut live = Vec::new();
        for j in jobs {
            if let Some((n, p)) = self.live_node(&j)? {
                live.push((j, n, p));
            }
        }
        let reqs = live.iter().map(|(_, n, p)| ExtractRequest::ImagePixels { path: p.clone(), ext: n.ext.clone() }).collect();
        let results = self.run_extract(reqs);
        let mut ok = Vec::new();
        let mut payload = Vec::new();
        let sz = cairn_ml::CLIP_IMAGE * cairn_ml::CLIP_IMAGE * 3;
        for ((job, node, _), res) in live.into_iter().zip(results) {
            let Some(res) = res else { continue };
            match res {
                Ok((ExtractResponse::ImagePixels, px)) if px.len() == sz => {
                    payload.extend_from_slice(&px);
                    ok.push((job, node.id));
                }
                Ok((ExtractResponse::Error { message }, _)) => self.fail(&job, &message)?,
                Ok(_) => self.fail(&job, "bad pixel buffer")?,
                Err(e) => self.fail(&job, &format!("{e:#}"))?,
            }
        }
        if ok.is_empty() {
            return Ok(());
        }
        let vecs = match self.call_embed(EmbedRequest::Images { count: ok.len() }, &payload, cairn_ml::CLIP_DIM) {
            Ok(v) => v,
            Err(e) => {
                let msg = format!("{e:#}");
                for (j, _) in &ok {
                    self.fail(j, &msg)?;
                }
                return Ok(());
            }
        };
        let tx = db::write_tx(&self.conn)?;
        let mut rows = Vec::new();
        let mut dead = Vec::new();
        for (job, id) in &ok {
            let same = still_same(&tx, job)?;
            if same {
                dead.extend(db::kill_vecs(&tx, *id, db::VK_IMAGE)?);
            }
            rows.push((db::VK_IMAGE, *id, *id, same));
        }
        let publish = match self.store_vectors(db::SPACE_CLIP, &vecs, &rows, &tx) {
            Ok(p) => p,
            Err(e) => {
                drop(tx);
                self.rollback_store(db::SPACE_CLIP);
                return Err(e);
            }
        };
        for (job, id) in &ok {
            db::job_done(&tx, *id, Stage::ImageEmbed, &job.fp)?;
        }
        if let Err(e) = tx.commit() {
            drop(publish);
            self.rollback_store(db::SPACE_CLIP);
            return Err(e.into());
        }
        self.sh.kill_vecs(&dead);
        publish();
        Ok(())
    }
}

/// Chunks per file that get semantic vectors: data files only their header
/// and first rows, everything else its first 16 chunks (D-062).
fn embed_cap(ext: &str) -> i64 {
    match ext {
        "csv" | "tsv" | "json" | "jsonl" | "ndjson" | "xml" | "log" | "srt" | "vtt" => 1,
        _ => 16,
    }
}

/// Text embedding jobs, documents and notes first, then code, then data.
fn next_text_jobs(conn: &Connection, now_ns: i64, limit: usize) -> Result<Vec<Job>> {
    let mut st = conn.prepare_cached(
        "SELECT j.node_id, j.fp, j.attempts FROM jobs j JOIN nodes n ON n.id = j.node_id
         WHERE j.stage=?1 AND j.not_before<=?2
         ORDER BY CASE n.category WHEN 'pdf' THEN 0 WHEN 'document' THEN 0 WHEN 'presentation' THEN 0 WHEN 'text' THEN 1
                  WHEN 'spreadsheet' THEN 2 WHEN 'code' THEN 2 ELSE 3 END, j.node_id
         LIMIT ?3",
    )?;
    let jobs = st
        .query_map(params![Stage::EmbedText as i64, now_ns, limit as i64], |r| Ok(Job { node_id: r.get(0)?, stage: Stage::EmbedText, fp: r.get(1)?, attempts: r.get(2)? }))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(jobs)
}

/// The job still describes the current file version.
fn still_same(tx: &Connection, job: &Job) -> Result<bool> {
    let fp: Option<Vec<u8>> = tx.query_row("SELECT fp FROM nodes WHERE id=?1", [job.node_id], |r| r.get(0)).optional()?;
    Ok(fp.as_deref() == Some(&job.fp[..]))
}

/// "parent/name" of a path, prepended to chunks so the embedder sees where
/// the text lives.
fn context_label(path: &[u8]) -> String {
    let s = String::from_utf8_lossy(path);
    let mut parts = s.rsplit('/');
    let name = parts.next().unwrap_or("");
    let parent = parts.next().unwrap_or("");
    format!("{parent}/{name}")
}

/// Folder context: its name, user-written notes inside it, and what it holds.
fn build_folder_context(conn: &Connection, node: &NodeRow) -> Result<(String, String)> {
    let kids = db::children(conn, node.id)?;
    let mut notes = String::new();
    let mut names = Vec::new();
    for k in &kids {
        if names.len() < 40 {
            names.push(db::name_tokens(&k.name));
        }
        if !k.is_dir && k.flags & db::FLAG_SECRET == 0 && classify::is_context_file(&k.name) && notes.len() < FOLDER_CONTEXT_CHARS {
            let mut st = conn.prepare_cached("SELECT text FROM chunks WHERE node_id=?1 ORDER BY ord LIMIT 2")?;
            for t in st.query_map([k.id], |r| r.get::<_, String>(0))? {
                let t = t?;
                notes.push_str(&t);
                notes.push('\n');
            }
        }
    }
    let mut notes: String = notes.chars().take(FOLDER_CONTEXT_CHARS).collect();
    notes = notes.trim().to_string();
    let title = db::name_tokens(&node.name);
    let mut text = title.clone();
    if !notes.is_empty() {
        text.push('\n');
        text.push_str(&notes);
    }
    if !names.is_empty() {
        text.push_str("\nContains: ");
        text.push_str(&names.join(", "));
    }
    let text: String = text.chars().take(2400).collect();
    Ok((text, notes))
}
