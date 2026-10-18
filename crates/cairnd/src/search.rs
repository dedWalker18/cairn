//! Query execution: hybrid retrieval fused with reciprocal rank fusion.
//!
//! Signals (each a ranked list):
//!   content   bge vector hits on text chunks
//!   image     CLIP text->image hits
//!   name      full-text hits on file/folder names and folder notes
//!   text      full-text hits on extracted text
//!   folder    files inheriting context from matching folders (decays with depth)
//! Filters: kind, date (hard for photos/videos, a boost otherwise), scope.

use std::collections::HashMap;
use std::time::Instant;

use anyhow::Result;
use cairn_core::classify::Category;
use cairn_core::db::{self, NodeRow};
use cairn_core::ipc::{Hit, SearchResult, WirePath};
use cairn_core::query::{self, ParsedQuery};
use cairn_core::timeutil::{self, CivilRange};
use jiff::civil::DateTime;
use jiff::tz::TimeZone;
use jiff::Timestamp;
use rusqlite::{params_from_iter, Connection};

use crate::scanner::lookup_path;
use crate::shared::Shared;

const RRF_K: f32 = 60.0;
const W_CONTENT: f32 = 1.0;
const W_IMAGE: f32 = 1.0;
const W_NAME: f32 = 0.9;
const W_TEXT: f32 = 0.6;
const W_FOLDER: f32 = 0.85;
/// Similarity floors, calibrated on bge-small-en-v1.5 / CLIP ViT-B/32 LAION
/// (docs/DECISIONS.md D-034): relevant text >= ~0.63, unrelated <= ~0.57;
/// relevant images >= ~0.24, unrelated <= ~0.21.
const TEXT_MIN: f32 = 0.60;
const TEXT_REL: f32 = 0.10;
const CLIP_MIN: f32 = 0.22;
const CLIP_REL: f32 = 0.06;
const DATE_BOOST: f32 = 1.6;

#[derive(Default)]
struct Cand {
    score: f32,
    why: Vec<String>,
    chunk: Option<i64>,
}

struct Fusion {
    c: HashMap<i64, Cand>,
}

impl Fusion {
    fn add(&mut self, node: i64, rank: usize, weight: f32, why: Option<String>) -> &mut Cand {
        let e = self.c.entry(node).or_default();
        e.score += weight / (RRF_K + rank as f32);
        if let Some(w) = why {
            if !e.why.contains(&w) && e.why.len() < 4 {
                e.why.push(w);
            }
        }
        e
    }
}

/// FTS5 query: OR of terms. `prefix` adds `*` (only used on names_fts,
/// which has a prefix index; prefix scans on the multi-million-row chunk
/// index cost seconds, D-058).
fn fts_query(terms: &[String], prefix: bool) -> Option<String> {
    let parts: Vec<String> = terms
        .iter()
        .map(|t| t.chars().filter(|c| c.is_alphanumeric()).collect::<String>())
        .filter(|t| !t.is_empty())
        .map(|t| if prefix && t.chars().count() >= 3 { format!("\"{t}\"*") } else { format!("\"{t}\"") })
        .collect();
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" OR "))
    }
}

fn load_nodes(conn: &Connection, ids: &[i64]) -> Result<HashMap<i64, NodeRow>> {
    let mut out = HashMap::with_capacity(ids.len());
    for chunk in ids.chunks(500) {
        let q = format!("SELECT {} FROM nodes WHERE id IN ({})", db::NODE_COLS, vec!["?"; chunk.len()].join(","));
        let mut st = conn.prepare(&q)?;
        for r in st.query_map(params_from_iter(chunk.iter()), db::node_from_row)? {
            let r = r?;
            out.insert(r.id, r);
        }
    }
    Ok(out)
}

/// Best estimate of when a file came into being: capture time from EXIF if
/// known, else the earlier of birth and modification time (a copy without
/// preserved times has a new btime but often an old mtime; an edited file
/// has a new mtime but its old btime).
pub fn origin_civil(n: &NodeRow, tz: &TimeZone, now: Timestamp) -> Option<DateTime> {
    if let Some(t) = n.taken_civil.as_deref().and_then(timeutil::civil_from_string) {
        return Some(t);
    }
    let m = timeutil::fs_ns_to_civil(n.mtime_ns, tz, now);
    let b = n.btime_ns.and_then(|b| timeutil::fs_ns_to_civil(b, tz, now));
    match (m, b) {
        (Some(m), Some(b)) => Some(m.min(b)),
        (a, b) => a.or(b),
    }
}

fn civil_to_ns(dt: DateTime, tz: &TimeZone) -> i64 {
    tz.to_ambiguous_zoned(dt).compatible().map(|z| z.timestamp().as_nanosecond() as i64).unwrap_or(0)
}

pub fn search(sh: &Shared, conn: &Connection, query_text: &str, cwd: Option<Vec<u8>>, scope_flag: bool, kinds_override: &[Category], limit: usize) -> Result<SearchResult> {
    let t0 = Instant::now();
    let limit = limit.clamp(1, 200);
    let now_z = timeutil::now_zoned();
    let tz = now_z.time_zone().clone();
    let now = now_z.timestamp();
    let mut pq: ParsedQuery = query::parse(query_text, &now_z);
    let mut scope_id = if pq.scope_here || scope_flag { cwd.as_deref().map(|c| lookup_path(conn, &sh.roots, c)).transpose()?.flatten() } else { None };
    // "fire photos from downloads": a phrase that names exactly one folder
    // limits the search to it (D-056). Unresolved phrases stay search words.
    if scope_id.is_none() {
        let places = std::mem::take(&mut pq.places);
        for place in places {
            if let Some(id) = resolve_folder(conn, &place.phrase)? {
                pq = *place.without;
                scope_id = Some(id);
                break;
            }
        }
    }
    if !kinds_override.is_empty() {
        pq.kinds = kinds_override.to_vec();
    }
    let media_only = !pq.kinds.is_empty() && pq.kinds.iter().all(|k| k.is_media());
    let want_images = pq.kinds.is_empty() || pq.kinds.contains(&Category::Image);

    let mut fusion = Fusion { c: HashMap::new() };

    if !pq.semantic.trim().is_empty() {
        // Embed the query with both models in parallel.
        let (qv, cv) = std::thread::scope(|s| {
            let t = s.spawn(|| sh.text_model.lock().ok().and_then(|m| m.as_ref().and_then(|m| m.embed_query(&pq.semantic).ok())));
            let c = s.spawn(|| {
                if !want_images {
                    return None;
                }
                sh.clip_model.lock().ok().and_then(|m| m.as_ref().and_then(|m| m.embed(&format!("a photo of {}", pq.semantic)).ok()))
            });
            (t.join().ok().flatten(), c.join().ok().flatten())
        });

        let mut folder_rank: Vec<(i64, f32)> = Vec::new();

        if let Some(qv) = qv {
            let hits = sh.text_vecs.read().unwrap().search(&qv, 300, None)?;
            let top = hits.first().map(|h| h.1).unwrap_or(0.0);
            tracing::debug!(query = %pq.semantic, scores = ?hits.iter().take(12).map(|h| (h.0, (h.1 * 1000.0).round() / 1000.0)).collect::<Vec<_>>(), "text vector hits");
            let hits: Vec<(i64, f32)> = hits.into_iter().filter(|h| h.1 >= TEXT_MIN && h.1 >= top - TEXT_REL).collect();
            let rows = vec_rows(conn, &hits.iter().map(|h| h.0).collect::<Vec<_>>())?;
            let mut seen = std::collections::HashSet::new();
            let mut rank = 0;
            for (vid, _score) in &hits {
                let Some(&(kind, owner, node)) = rows.get(vid) else { continue };
                if kind == db::VK_FOLDER {
                    folder_rank.push((node, W_FOLDER / (RRF_K + folder_rank.len() as f32)));
                    continue;
                }
                if kind == db::VK_CHUNK && seen.insert(node) {
                    let c = fusion.add(node, rank, W_CONTENT, Some("content".into()));
                    if c.chunk.is_none() {
                        c.chunk = Some(owner);
                    }
                    rank += 1;
                }
            }
        }

        if let Some(cv) = cv {
            let hits = sh.clip_vecs.read().unwrap().search(&cv, 200, None)?;
            let top = hits.first().map(|h| h.1).unwrap_or(0.0);
            tracing::debug!(query = %pq.semantic, scores = ?hits.iter().take(12).map(|h| (h.0, (h.1 * 1000.0).round() / 1000.0)).collect::<Vec<_>>(), "clip hits");
            let rows = vec_rows(conn, &hits.iter().map(|h| h.0).collect::<Vec<_>>())?;
            let w = if pq.kinds.contains(&Category::Image) { W_IMAGE * 1.3 } else { W_IMAGE };
            let mut rank = 0;
            for (vid, score) in hits {
                if score < CLIP_MIN || score < top - CLIP_REL {
                    break;
                }
                if let Some(&(_, _, node)) = rows.get(&vid) {
                    fusion.add(node, rank, w, Some(format!("looks like “{}”", pq.semantic)));
                    rank += 1;
                }
            }
        }

        if let (Some(fq), Some(fq_exact)) = (fts_query(&pq.terms, true), fts_query(&pq.terms, false)) {
            let mut st = conn.prepare_cached("SELECT rowid FROM names_fts WHERE names_fts MATCH ?1 ORDER BY bm25(names_fts, 2.0, 1.0) LIMIT 300")?;
            let ids: Vec<i64> = st.query_map([&fq], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
            let kinds = load_nodes(conn, &ids)?;
            let mut rank = 0;
            for id in ids {
                let Some(n) = kinds.get(&id) else { continue };
                if n.is_dir {
                    folder_rank.push((id, W_FOLDER * 0.9 / (RRF_K + folder_rank.len() as f32)));
                }
                fusion.add(id, rank, W_NAME, Some("name".into()));
                rank += 1;
            }
            let mut st = conn.prepare_cached(
                "SELECT c.node_id, c.id FROM chunks_fts JOIN chunks c ON c.id = chunks_fts.rowid
                 WHERE chunks_fts MATCH ?1 ORDER BY rank LIMIT 300",
            )?;
            // All words first: far fewer matches to rank on a multi-million
            // chunk index; any-word only if that finds too little (D-058).
            let fq_all = fq_exact.replace(" OR ", " AND ");
            let mut rows: Vec<(i64, i64)> = st.query_map([&fq_all], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
            if rows.len() < 20 && fq_all != fq_exact {
                rows = st.query_map([&fq_exact], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
            }
            let mut seen = std::collections::HashSet::new();
            let mut rank = 0;
            for (node, chunk) in rows {
                if seen.insert(node) {
                    let c = fusion.add(node, rank, W_TEXT, Some("text".into()));
                    if c.chunk.is_none() {
                        c.chunk = Some(chunk);
                    }
                    rank += 1;
                }
            }
        }

        // Context inheritance: files (not subfolders) below matching folders.
        let mut agg: HashMap<i64, f32> = HashMap::new();
        for (f, s) in folder_rank {
            *agg.entry(f).or_default() += s;
        }
        let mut folders: Vec<(i64, f32)> = agg.into_iter().collect();
        folders.sort_by(|a, b| b.1.total_cmp(&a.1));
        folders.truncate(40);
        // The most specific folder wins: if a folder and one of its
        // descendants both match, the ancestor does not pass context down
        // (otherwise "Pictures" would spread a match on "Pictures/Manali
        // trek" to every photo).
        let matched: std::collections::HashSet<i64> = folders.iter().map(|f| f.0).collect();
        let mut shadowed = std::collections::HashSet::new();
        for (f, _) in &folders {
            for a in db::ancestors(conn, *f, 64)? {
                if matched.contains(&a) {
                    shadowed.insert(a);
                }
            }
        }
        folders.retain(|f| !shadowed.contains(&f.0));
        folders.truncate(25);
        let mut expanded = 0usize;
        for (fi, (folder, fscore)) in folders.iter().enumerate() {
            let folder_name = db::get_node(conn, *folder)?.map(|n| String::from_utf8_lossy(&n.name).into_owned()).unwrap_or_default();
            fusion.add(*folder, fi, W_FOLDER, Some(format!("folder “{folder_name}”")));
            let mut st = conn.prepare_cached(
                "WITH RECURSIVE sub(id, depth, is_dir) AS (
                    SELECT id, 1, is_dir FROM nodes WHERE parent=?1
                    UNION ALL SELECT n.id, sub.depth+1, n.is_dir FROM nodes n JOIN sub ON n.parent=sub.id WHERE sub.depth < 3 AND sub.is_dir=1
                 ) SELECT id, depth FROM sub WHERE is_dir=0 LIMIT 600",
            )?;
            let desc: Vec<(i64, i64)> = st.query_map([folder], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
            for (id, depth) in desc {
                let decay = 0.6f32.powi(depth as i32 - 1);
                let e = fusion.c.entry(id).or_default();
                e.score += fscore * decay;
                let why = format!("in folder “{folder_name}”");
                if !e.why.contains(&why) && e.why.len() < 4 {
                    e.why.push(why);
                }
                expanded += 1;
            }
            if expanded > 6000 {
                break;
            }
        }
    } else if !pq.kinds.is_empty() || pq.range.is_some() || scope_id.is_some() {
        filter_only(conn, &pq, scope_id, &tz, &mut fusion)?;
    }

    // Filters and final scoring.
    let ids: Vec<i64> = fusion.c.keys().copied().collect();
    let nodes = load_nodes(conn, &ids)?;
    let mut anc_cache: HashMap<i64, bool> = HashMap::new();
    let mut scored: Vec<(i64, f32)> = Vec::new();
    for (id, cand) in &fusion.c {
        let Some(n) = nodes.get(id) else { continue };
        if n.parent.is_none() {
            continue; // never return a root itself
        }
        if !pq.kinds.is_empty() && !pq.kinds.contains(&n.category) {
            continue;
        }
        if let Some(scope) = scope_id {
            if !in_scope(conn, n, scope, &mut anc_cache)? {
                continue;
            }
        }
        let mut s = cand.score;
        if let Some(r) = pq.range {
            let inside = origin_civil(n, &tz, now).is_some_and(|d| r.contains(d));
            if media_only && !inside {
                continue;
            }
            if inside {
                s *= DATE_BOOST;
            }
        }
        scored.push((*id, s));
    }
    scored.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    scored.truncate(limit);

    let mut hits = Vec::with_capacity(scored.len());
    for (id, score) in scored {
        let n = &nodes[&id];
        let cand = &fusion.c[&id];
        let Some(path) = db::path_of(conn, id)? else { continue };
        let (mut line, mut line_end, mut snippet) = (None, None, None);
        if let Some(cid) = cand.chunk {
            if let Ok((ls, le, text)) = conn.query_row("SELECT line_start, line_end, text FROM chunks WHERE id=?1", [cid], |r| Ok((r.get::<_, u32>(0)?, r.get::<_, u32>(1)?, r.get::<_, String>(2)?))) {
                line = Some(ls);
                line_end = Some(le);
                snippet = Some(text.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(220).collect());
            }
        }
        let mut why = cand.why.clone();
        if let (Some(r), Some(label)) = (pq.range, pq.range_label.as_ref()) {
            if origin_civil(n, &tz, now).is_some_and(|d| r.contains(d)) {
                why.push(format!("dated {label}"));
            }
        }
        hits.push(Hit {
            path: WirePath::from_bytes(&path),
            category: n.category.as_str().to_string(),
            is_dir: n.is_dir,
            line,
            line_end,
            snippet,
            date: origin_civil(n, &tz, now).map(|d| d.date().to_string()),
            score,
            why,
        });
    }
    let pending: i64 = sh.stats.lock().unwrap().pending.iter().map(|p| p.1).sum();
    let mut parsed = pq.summary();
    if let Some(sid) = scope_id {
        parsed.scope = db::path_of(conn, sid)?.map(|p| cairn_core::paths::display(&p));
    }
    Ok(SearchResult {
        hits,
        parsed,
        took_ms: t0.elapsed().as_millis() as u64,
        partial: !sh.initial_scan_done.load(std::sync::atomic::Ordering::SeqCst) || pending > 0,
    })
}

fn vec_rows(conn: &Connection, ids: &[i64]) -> Result<HashMap<i64, (i64, i64, i64)>> {
    let mut out = HashMap::with_capacity(ids.len());
    for chunk in ids.chunks(500) {
        let q = format!("SELECT id, kind, owner, node_id FROM vecs WHERE alive=1 AND id IN ({})", vec!["?"; chunk.len()].join(","));
        let mut st = conn.prepare(&q)?;
        for r in st.query_map(params_from_iter(chunk.iter()), |r| Ok((r.get::<_, i64>(0)?, (r.get(1)?, r.get(2)?, r.get(3)?))))? {
            let (id, v) = r?;
            out.insert(id, v);
        }
    }
    Ok(out)
}

fn in_scope(conn: &Connection, n: &NodeRow, scope: i64, cache: &mut HashMap<i64, bool>) -> Result<bool> {
    if n.id == scope {
        return Ok(true);
    }
    let Some(parent) = n.parent else { return Ok(false) };
    if let Some(&v) = cache.get(&parent) {
        return Ok(v);
    }
    let anc = db::ancestors(conn, n.id, 256)?;
    let v = anc.contains(&scope);
    cache.insert(parent, v);
    Ok(v)
}

/// No semantic words: list by kind/date/scope, newest first.
fn filter_only(conn: &Connection, pq: &ParsedQuery, scope: Option<i64>, tz: &TimeZone, fusion: &mut Fusion) -> Result<()> {
    let mut sql = String::from("SELECT id FROM nodes WHERE parent IS NOT NULL");
    let mut args: Vec<rusqlite::types::Value> = Vec::new();
    if let Some(s) = scope {
        sql = String::from(
            "WITH RECURSIVE sub(id) AS (SELECT ?1 UNION ALL SELECT n.id FROM nodes n JOIN sub ON n.parent=sub.id)
             SELECT id FROM nodes WHERE parent IS NOT NULL AND id IN (SELECT id FROM sub)",
        );
        args.push(s.into());
    }
    if !pq.kinds.is_empty() {
        sql.push_str(&format!(" AND category IN ({})", vec!["?"; pq.kinds.len()].join(",")));
        for k in &pq.kinds {
            args.push(k.as_str().to_string().into());
        }
    }
    if let Some(CivilRange { start, end }) = pq.range {
        sql.push_str(
            " AND ((taken_civil IS NOT NULL AND taken_civil >= ? AND taken_civil < ?)
               OR (taken_civil IS NULL AND min(mtime_ns, coalesce(btime_ns, mtime_ns)) >= ? AND min(mtime_ns, coalesce(btime_ns, mtime_ns)) < ?))",
        );
        args.push(timeutil::civil_to_string(start).into());
        args.push(timeutil::civil_to_string(end).into());
        args.push(civil_to_ns(start, tz).into());
        args.push(civil_to_ns(end, tz).into());
    }
    sql.push_str(" ORDER BY coalesce(taken_civil, '') DESC, mtime_ns DESC LIMIT 1000");
    let mut st = conn.prepare(&sql)?;
    let ids: Vec<i64> = st.query_map(params_from_iter(args.iter()), |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
    for (rank, id) in ids.into_iter().enumerate() {
        fusion.add(id, rank, 1.0, None);
    }
    Ok(())
}

fn norm(s: &str) -> String {
    s.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect()
}

/// The folder a phrase names: exact name match (ignoring case, spaces and
/// punctuation, singular/plural), shallowest wins, a tie means ambiguous.
fn resolve_folder(conn: &Connection, phrase: &str) -> Result<Option<i64>> {
    let want = norm(phrase);
    if want.len() < 2 {
        return Ok(None);
    }
    let words: Vec<String> = phrase.split_whitespace().map(|w| w.chars().filter(|c| c.is_alphanumeric()).collect::<String>()).filter(|w| !w.is_empty()).collect();
    if words.is_empty() {
        return Ok(None);
    }
    let fq = words.iter().map(|w| format!("\"{w}\"*")).collect::<Vec<_>>().join(" AND ");
    let mut st = conn.prepare_cached("SELECT rowid FROM names_fts WHERE name MATCH ?1 LIMIT 500")?;
    let ids: Vec<i64> = st.query_map([&fq], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
    let nodes = load_nodes(conn, &ids)?;
    let mut best: Option<(usize, i64)> = None;
    let mut tie = false;
    for n in nodes.values() {
        if !n.is_dir || n.parent.is_none() {
            continue;
        }
        let name = norm(&String::from_utf8_lossy(&n.name));
        if !(name == want || name == format!("{want}s") || format!("{name}s") == want) {
            continue;
        }
        let depth = db::ancestors(conn, n.id, 256)?.len();
        match best {
            Some((d, _)) if depth > d => {}
            Some((d, _)) if depth == d => tie = true,
            _ => {
                best = Some((depth, n.id));
                tie = false;
            }
        }
    }
    Ok(if tie { None } else { best.map(|b| b.1) })
}
