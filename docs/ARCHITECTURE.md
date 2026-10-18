# Architecture and implementation plan

## Goal

Find anything on a Linux machine by describing it ("photos of mountains from
last December", "the script that runs terraform on AWS", "the SOP the client
gave for this project"). It is fast, local, airgap-capable, and it never slows
the user down or risks their data.

The core idea: **context propagates**. A folder name ("Manali trek Dec 25")
and notes inside a folder describe every file below it. A photo dump with no
context is understood by an image model. A file is defined by its content, its
location, its dates and its neighbours.

## Process model

```
┌──────────── cairnd (user service, normal priority for queries) ─────────────┐
│ server thread   unix socket (0600, SO_PEERCRED) → search / status / exclude  │
│ scanner thread  SCHED_IDLE · per-directory diff · moves · verified deletes   │
│ watcher thread  inotify (budgeted) → marks directories dirty                 │
│ pipeline thread SCHED_IDLE · durable jobs · throttling (PSI, battery, RAM)   │
│ query models    bge-small (text) + CLIP text tower, loaded once              │
│ Landlock: writes only to ~/.local/share/cairn and $XDG_RUNTIME_DIR/cairn,    │
│           no TCP at all                                                       │
└───────┬──────────────────────────────────────────────┬──────────────────────┘
        │ frames over pipes                             │ frames over pipes
┌───────▼────────────────────┐            ┌─────────────▼───────────────────────┐
│ cairnd worker extract  ×2  │            │ cairnd worker embed  ×1 (on demand) │
│ PDF/Office/XML/EXIF/images │            │ bge-small passages, CLIP vision     │
│ Landlock: no writes        │            │ Landlock: no writes                 │
│ RLIMIT_AS 1.5G, FSIZE 0    │            │ SCHED_IDLE, reaped after 120 s idle │
│ watchdog + pidfd SIGKILL   │            │ killed under memory pressure        │
└────────────────────────────┘            └─────────────────────────────────────┘

cairn (CLI) ─┐
cairn-gui ───┼── unix socket ──► cairnd
Nautilus ext ┘  (the extension only spawns cairn / cairn-gui)
```

## Storage (`~/.local/share/cairn`, mode 0700)

| File | Content | Rebuildable |
|---|---|---|
| `index.db` (SQLite WAL) | nodes, jobs, chunks + FTS5, names FTS5, folder context, vector rows, errors | yes |
| `text.vec` | bge vectors, int8 + scale, append-only | yes |
| `clip.vec` | CLIP image vectors, int8 + scale | yes |
| `exclusions.json` | mirror of user exclusions | **user intent**, restored on rebuild |
| `models/` | pinned, SHA-256-verified weights | re-fetch / re-install |
| `quarantine-*/` | damaged indexes moved aside, never deleted | – |

Schema essentials (`crates/cairn-core/src/db.rs`):

* `nodes(id, parent, name BLOB, is_dir, dev, ino, size, mtime_ns, ctime_ns, btime_ns, category, ext, flags, taken_civil, fp)`:
  a tree keyed by (parent, name). Names are raw bytes. Paths are rebuilt from
  ancestors, so a rename touches one row.
* `jobs(node_id, stage, fp, attempts, not_before, last_error)`: the durable
  queue.
* `chunks` + `chunks_fts`: extracted text with line ranges (`file:line`).
* `names_fts(name, context)`: file and folder names, and folder notes.
* `folder_ctx(node_id, text, hash)`: the hash makes re-embedding idempotent.
* `vecs(id, space, kind, owner, node_id, slot, alive)`: maps vector slots to
  chunks, images and folders. Deletions are tombstones.
* `segments(space, dim, committed_slots)`: the crash-consistency anchor for the
  `.vec` files.

Size estimate for a typical developer home: 900k files, about 2M text chunks, 300k images.
* RAM: about 100 MB for 1-bit search codes.
* Disk: about 0.8 GB of int8 vectors plus the text DB.

## Indexing pipeline

1. **Scan** (`scanner.rs`). A full BFS of each root, plus per-directory
   re-lists when inotify marks a directory dirty. Each directory listing is
   diffed against the DB children:
   * new entries are inserted, or recognised as moves via (dev, inode);
   * changed entries are updated (fingerprint = size, mtime, inode);
   * vanished entries are deleted at the end of the pass, after verifying
     on disk.

   It enqueues jobs. It never follows symlinks or crosses `st_dev`, and treats
   unreadable directories as unknown, not empty.
2. **Jobs** (`pipeline.rs`), in priority order:
   1. `ExtractText`: text, PDF, Office, SVG, CAD-text → chunks with line ranges.
   2. `ImageMeta`: EXIF capture time, offset, GPS, orientation. This pass is
      fast, so date queries over photos work early.
   3. `EmbedText`: chunk text prefixed with `parent/name` → bge vectors.
   4. `FolderCtx`: name + notes + child names → bge vector.
   5. `ImageEmbed`: decode (JPEG DCT-downscaled), orient, resize, crop → CLIP
      vector.

   A job's result and its removal commit atomically. Results whose file changed
   meanwhile are discarded.
3. **Vectors**: append + fsync → DB commit → publish (D-021).

The index is useful immediately: names after the scan (seconds to minutes),
photo dates after `ImageMeta`, semantic search as embedding progresses.

## Query path (`search.rs`)

1. Deterministic parse (`query.rs`) into kinds, a civil date range, a scope
   ("here"), the semantic text, and keyword terms.
2. In parallel: the bge query embedding, and the CLIP text embedding of
   "a photo of {semantic}" when images are wanted.
3. Candidate lists:
   * content: bge chunk hits, floor 0.60
   * image: CLIP hits, floor 0.22
   * name: FTS5 on names and folder notes, prefix match
   * text: FTS5 on chunks
   * folder: files under the best-matching folders (depth ≤ 3, decay 0.6, most
     specific folder wins)
4. Reciprocal rank fusion (k = 60) with per-list weights.
5. Filters:
   * kind,
   * scope (ancestor check),
   * date: hard for photo/video queries, ×1.6 boost otherwise; origin date =
     EXIF, else min(btime, mtime).
6. Results carry their path, `line`–`line_end` and a snippet for text, the
   date, and a "why" list ("looks like “mountains”", "in folder “Manali trek
   Dec 25”", "dated December 2025").

Kind/date-only queries ("all photos from December last year") skip the models
and run as pure SQL in under 1 ms.

Measured on a real 550k-file home (CPU only, laptop Ryzen 4000): queries take
0.03–0.4 s; query embedding is ~10 ms (ONNX int8).

## Never hinder the user

* Background threads and workers run at SCHED_IDLE with the idle I/O class.
  They only get CPU and disk that nothing else wants.
* PSI back-off (cpu/io/memory), a battery pause, and a low-memory pause. The
  embed worker is killed under memory pressure.
* `posix_fadvise(DONTNEED)` after reading user files, so the page cache keeps
  the user's working set.
* The systemd unit adds a second fence: CPUWeight 50, IOWeight 20,
  MemoryHigh 2G, MemoryMax 3G.
* Idle workers are reaped, which frees model RAM.

## Safety and data protection

* Read-only access to user files, enforced by the kernel (Landlock).
* Untrusted parsing happens in disposable processes.
* No network: Landlock TCP deny + `RestrictAddressFamilies=AF_UNIX` + no network
  code in the daemon.
* Crash-consistent by construction: tested with repeated SIGKILL
  (`tests/chaos.sh`).
* The index is a cache. Corruption → quarantine and rebuild; user intent is
  kept in `exclusions.json`.

## Roadmap

Phase 1 (this build) is done: scan, watch, extraction, text and image
embedding, folder context, hybrid search, CLI, GTK search window, Nautilus
extension, systemd unit, chaos test.

Phase 2, quality and coverage:
* calibrate floors and weights on a 50-query benchmark built from the owner's
  real files,
* tree-sitter chunking (function-exact `file:line`),
* `.gitignore` support (Q9),
* video metadata (QuickTime UTC creation time, keyframes through CLIP),
* HEIC/RAW decoding,
* OCR for scanned PDFs (idle-only, opt-in),
* offline reverse geocoding of EXIF GPS (GeoNames) → place names as context.

Phase 3, agents and integrations:
* MCP server for Claude Code / agents (needs Q5),
* FUSE virtual folders (Gifford-style), so every file picker can search,
* niri/fuzzel launcher binding,
* GNOME Shell search provider for GNOME users,
* a Nautilus fork with a native search provider (optional).

Phase 4, btrfs accelerators (Q6, Q7):
* read-only root helper: `BTRFS_IOC_TREE_SEARCH_V2` enumeration,
  `find-new` catch-up, fanotify whole-filesystem watch,
* FIEMAP shared-extent duplicate detection,
* snapper-backed version history ("the proposal before I changed pricing"),
* index data in its own subvolume.

Phase 5, the "intelligent filesystem":
* event log / provenance,
* "worked together" edges from usage (Q8),
* unfinished-project detection (project markers + activity curve),
* recommendations,
* ONNX int8 or a GPU backend for faster indexing,
* compaction of `.vec` files when more than 30% is dead.
