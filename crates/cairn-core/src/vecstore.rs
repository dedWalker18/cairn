//! Append-only vector store, one file per embedding space.
//!
//! Layout: 16-byte header (magic, version, dim) then fixed-size records of
//! `dim` int8 values + one f32 scale. Slots are never rewritten.
//!
//! Crash safety (D-021): vectors are appended and fsynced *before* the
//! SQLite transaction that references them commits. The database stores the
//! committed slot count; on open, any tail beyond it is truncated. A tail can
//! only come from a crash between the two steps, and nothing references it.
//!
//! Search: brute-force Hamming distance over 1-bit sign codes held in RAM,
//! then exact rescoring of the best candidates by reading int8 records with
//! pread (no mmap: a mapped file that is truncated raises SIGBUS).

use std::collections::BinaryHeap;
use std::fs::{File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

const MAGIC: &[u8; 4] = b"CRNV";
const VERSION: u32 = 1;
const HEADER: u64 = 16;

pub struct VecStore {
    path: PathBuf,
    file: File,
    dim: usize,
    words: usize,
    /// Slots persisted and referenced by the database.
    committed: u64,
    codes: Vec<u64>,
    alive: Vec<bool>,
    /// Database row id of the vector in each slot (-1 when dead/unknown).
    ids: Vec<i64>,
    live: usize,
}

pub fn quantize(v: &[f32]) -> (Vec<i8>, f32) {
    let max = v.iter().fold(0f32, |m, x| m.max(x.abs()));
    if max == 0.0 || !max.is_finite() {
        return (vec![0; v.len()], 0.0);
    }
    let scale = max / 127.0;
    (v.iter().map(|x| (x / scale).round().clamp(-127.0, 127.0) as i8).collect(), scale)
}

fn sign_code(v: &[f32], out: &mut [u64]) {
    out.iter_mut().for_each(|w| *w = 0);
    for (i, x) in v.iter().enumerate() {
        if *x > 0.0 {
            out[i / 64] |= 1 << (i % 64);
        }
    }
}

fn sign_code_i8(v: &[i8], out: &mut [u64]) {
    out.iter_mut().for_each(|w| *w = 0);
    for (i, x) in v.iter().enumerate() {
        if *x > 0 {
            out[i / 64] |= 1 << (i % 64);
        }
    }
}

pub fn normalize(v: &mut [f32]) {
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if n > 0.0 && n.is_finite() {
        v.iter_mut().for_each(|x| *x /= n);
    }
}

impl VecStore {
    fn rec_size(&self) -> u64 {
        self.dim as u64 + 4
    }

    /// Open (or create) the store and reconcile it with the committed count
    /// from the database. Errors mean the file is shorter than the database
    /// claims: the caller must discard this space and re-embed.
    pub fn open(path: &Path, dim: usize, committed: u64) -> Result<VecStore> {
        if dim == 0 || dim > 4096 {
            bail!("bad dimension {dim}");
        }
        let file = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path)
            .with_context(|| format!("opening {}", path.display()))?;
        let len = file.metadata()?.len();
        if len == 0 {
            let mut h = [0u8; HEADER as usize];
            h[0..4].copy_from_slice(MAGIC);
            h[4..8].copy_from_slice(&VERSION.to_le_bytes());
            h[8..12].copy_from_slice(&(dim as u32).to_le_bytes());
            file.write_all_at(&h, 0)?;
            file.sync_all()?;
            if committed != 0 {
                bail!("vector file {} is empty but {committed} slots are committed", path.display());
            }
        } else {
            let mut h = [0u8; HEADER as usize];
            file.read_exact_at(&mut h, 0).context("reading header")?;
            if &h[0..4] != MAGIC || u32::from_le_bytes(h[4..8].try_into().unwrap()) != VERSION {
                bail!("vector file {} has a bad header", path.display());
            }
            if u32::from_le_bytes(h[8..12].try_into().unwrap()) as usize != dim {
                bail!("vector file {} has a different dimension", path.display());
            }
        }
        let words = dim.div_ceil(64);
        let mut s = VecStore {
            path: path.to_path_buf(),
            file,
            dim,
            words,
            committed: 0,
            codes: Vec::new(),
            alive: Vec::new(),
            ids: Vec::new(),
            live: 0,
        };
        let expected = HEADER + committed * s.rec_size();
        let len = s.file.metadata()?.len();
        if len < expected {
            bail!("vector file {} is truncated ({len} < {expected})", path.display());
        }
        if len > expected {
            // Uncommitted tail from an interrupted append.
            s.file.set_len(expected)?;
            s.file.sync_all()?;
        }
        s.committed = committed;
        s.load_codes()?;
        Ok(s)
    }

    fn load_codes(&mut self) -> Result<()> {
        let n = self.committed as usize;
        self.codes = vec![0; n * self.words];
        self.alive = vec![false; n];
        self.ids = vec![-1; n];
        let rec = self.rec_size() as usize;
        let batch = 4096usize;
        let mut buf = vec![0u8; rec * batch];
        let mut slot = 0usize;
        while slot < n {
            let take = batch.min(n - slot);
            let bytes = &mut buf[..take * rec];
            self.file.read_exact_at(bytes, HEADER + slot as u64 * rec as u64)?;
            for i in 0..take {
                let r = &bytes[i * rec..i * rec + self.dim];
                let q: &[i8] = unsafe { std::slice::from_raw_parts(r.as_ptr() as *const i8, r.len()) };
                let w = (slot + i) * self.words;
                sign_code_i8(q, &mut self.codes[w..w + self.words]);
            }
            slot += take;
        }
        drop_page_cache(&self.file);
        Ok(())
    }

    /// Mark which slots are live, from the database.
    pub fn set_live(&mut self, rows: impl IntoIterator<Item = (i64, u64)>) {
        for (id, slot) in rows {
            let s = slot as usize;
            if s < self.alive.len() {
                if !self.alive[s] {
                    self.live += 1;
                }
                self.alive[s] = true;
                self.ids[s] = id;
            }
        }
    }

    pub fn dim(&self) -> usize {
        self.dim
    }

    pub fn committed(&self) -> u64 {
        self.committed
    }

    pub fn live(&self) -> usize {
        self.live
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Step 1 of an append: write and fsync. Returns the first new slot.
    /// Nothing is visible to search until `publish`.
    pub fn write_pending(&mut self, vecs: &[Vec<f32>]) -> Result<u64> {
        let rec = self.rec_size() as usize;
        let mut buf = Vec::with_capacity(rec * vecs.len());
        for v in vecs {
            if v.len() != self.dim {
                bail!("vector has dimension {}, store has {}", v.len(), self.dim);
            }
            let (q, scale) = quantize(v);
            buf.extend(q.iter().map(|x| *x as u8));
            buf.extend_from_slice(&scale.to_le_bytes());
        }
        let first = self.committed;
        let off = HEADER + first * rec as u64;
        self.file.set_len(off)?; // drop any earlier failed pending write
        self.file.write_all_at(&buf, off)?;
        self.file.sync_data()?;
        Ok(first)
    }

    /// Discard a pending write (database commit failed).
    pub fn rollback_pending(&mut self) -> Result<()> {
        self.file.set_len(HEADER + self.committed * self.rec_size())?;
        Ok(())
    }

    /// Step 2, after the database commit: make the vectors searchable.
    pub fn publish(&mut self, first: u64, vecs: &[Vec<f32>], ids: &[i64]) {
        debug_assert_eq!(first, self.committed);
        let mut code = vec![0u64; self.words];
        for (v, id) in vecs.iter().zip(ids) {
            sign_code(v, &mut code);
            self.codes.extend_from_slice(&code);
            self.alive.push(true);
            self.ids.push(*id);
            self.live += 1;
        }
        self.committed += vecs.len() as u64;
    }

    pub fn kill(&mut self, slot: u64) {
        let s = slot as usize;
        if s < self.alive.len() && self.alive[s] {
            self.alive[s] = false;
            self.live -= 1;
        }
    }

    fn read_record(&self, slot: u64, buf: &mut [u8]) -> Result<()> {
        self.file.read_exact_at(buf, HEADER + slot * self.rec_size())?;
        Ok(())
    }

    /// Approximate nearest neighbours by cosine similarity (inputs are
    /// normalised). `allow` restricts the candidate slots when present.
    pub fn search(&self, query: &[f32], k: usize, allow: Option<&dyn Fn(u64) -> bool>) -> Result<Vec<(i64, f32)>> {
        if query.len() != self.dim || k == 0 || self.live == 0 {
            return Ok(Vec::new());
        }
        let mut qcode = vec![0u64; self.words];
        sign_code(query, &mut qcode);
        let oversample = (k * 8).max(256);
        // Max-heap on distance keeps the `oversample` smallest distances.
        let mut heap: BinaryHeap<(u32, usize)> = BinaryHeap::with_capacity(oversample + 1);
        let n = self.alive.len();
        for s in 0..n {
            if !self.alive[s] {
                continue;
            }
            if let Some(f) = allow {
                if !f(s as u64) {
                    continue;
                }
            }
            let c = &self.codes[s * self.words..(s + 1) * self.words];
            let d: u32 = c.iter().zip(&qcode).map(|(a, b)| (a ^ b).count_ones()).sum();
            if heap.len() < oversample {
                heap.push((d, s));
            } else if let Some(&(worst, _)) = heap.peek() {
                if d < worst {
                    heap.pop();
                    heap.push((d, s));
                }
            }
        }
        let mut cands: Vec<usize> = heap.into_iter().map(|(_, s)| s).collect();
        cands.sort_unstable(); // sequential-ish reads
        let rec = self.rec_size() as usize;
        let mut buf = vec![0u8; rec];
        let mut scored = Vec::with_capacity(cands.len());
        for s in cands {
            self.read_record(s as u64, &mut buf)?;
            let scale = f32::from_le_bytes(buf[self.dim..self.dim + 4].try_into().unwrap());
            let dot: f32 = buf[..self.dim].iter().zip(query).map(|(q, x)| (*q as i8) as f32 * x).sum();
            scored.push((self.ids[s], dot * scale));
        }
        scored.sort_by(|a, b| b.1.total_cmp(&a.1));
        scored.truncate(k);
        Ok(scored)
    }
}

/// Ask the kernel to drop our pages so indexing does not evict the user's
/// working set from the page cache.
pub fn drop_page_cache(f: &File) {
    use std::os::fd::AsRawFd;
    unsafe {
        libc::posix_fadvise(f.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("cairn-vec-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn rand_vec(seed: &mut u64, dim: usize) -> Vec<f32> {
        let mut v: Vec<f32> = (0..dim)
            .map(|_| {
                *seed ^= *seed << 13;
                *seed ^= *seed >> 7;
                *seed ^= *seed << 17;
                (*seed % 2000) as f32 / 1000.0 - 1.0
            })
            .collect();
        normalize(&mut v);
        v
    }

    #[test]
    fn finds_itself_and_survives_reopen() {
        let d = tmpdir("basic");
        let p = d.join("t.vec");
        let mut seed = 42u64;
        let vecs: Vec<Vec<f32>> = (0..500).map(|_| rand_vec(&mut seed, 384)).collect();
        let ids: Vec<i64> = (1000..1500).collect();
        let mut s = VecStore::open(&p, 384, 0).unwrap();
        let first = s.write_pending(&vecs).unwrap();
        s.publish(first, &vecs, &ids);
        let r = s.search(&vecs[123], 5, None).unwrap();
        assert_eq!(r[0].0, 1123);
        assert!(r[0].1 > 0.95);
        drop(s);
        let mut s = VecStore::open(&p, 384, 500).unwrap();
        s.set_live(ids.iter().enumerate().map(|(i, id)| (*id, i as u64)));
        let r = s.search(&vecs[77], 3, None).unwrap();
        assert_eq!(r[0].0, 1077);
        s.kill(77);
        let r = s.search(&vecs[77], 3, None).unwrap();
        assert!(r.iter().all(|(id, _)| *id != 1077));
    }

    #[test]
    fn uncommitted_tail_is_truncated() {
        let d = tmpdir("tail");
        let p = d.join("t.vec");
        let mut seed = 7u64;
        let vecs: Vec<Vec<f32>> = (0..10).map(|_| rand_vec(&mut seed, 64)).collect();
        let mut s = VecStore::open(&p, 64, 0).unwrap();
        let _ = s.write_pending(&vecs).unwrap();
        // "crash" before the database commit: committed count stays 0
        drop(s);
        let s = VecStore::open(&p, 64, 0).unwrap();
        assert_eq!(s.committed(), 0);
        assert_eq!(std::fs::metadata(&p).unwrap().len(), HEADER);
        // database claims more than the file has: refuse
        drop(s);
        assert!(VecStore::open(&p, 64, 5).is_err());
    }

    #[test]
    fn allow_filter() {
        let d = tmpdir("allow");
        let p = d.join("t.vec");
        let mut seed = 9u64;
        let vecs: Vec<Vec<f32>> = (0..100).map(|_| rand_vec(&mut seed, 128)).collect();
        let ids: Vec<i64> = (0..100).collect();
        let mut s = VecStore::open(&p, 128, 0).unwrap();
        let f = s.write_pending(&vecs).unwrap();
        s.publish(f, &vecs, &ids);
        let only_even = |slot: u64| slot % 2 == 0;
        let r = s.search(&vecs[3], 10, Some(&only_even)).unwrap();
        assert!(r.iter().all(|(id, _)| id % 2 == 0));
    }
}
