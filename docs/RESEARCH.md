# Research notes: prior art, lessons, mistakes to avoid

Collected on 2026-10-18 while designing Cairn. Each lesson links to the
decision it produced (see DECISIONS.md).

## 1. DBOS: "everything is a table" (MIT/Stanford, 2020–2024)

Sources: *DBOS: A DBMS-oriented Operating System* (PVLDB 15, 2022),
CIDR 2022 progress report, *DBOS: three years later* (VLDBJ 2024),
*Data Governance in DBOS* (provenance), and the dbos-transact-py source.

DBOS stored all operating-system state (files, scheduler, messages) in tables
of a distributed transactional DBMS (VoltDB). OS services were SQL plus stored
procedures. The project became a company whose product, DBOS Transact, offers
durable workflows on Postgres.

| Finding | Number | What Cairn does |
|---|---|---|
| Analytics over files is a query, not a tree walk | "total size of files > 20K": 0.65 ms / 2 lines of SQL vs 9.9 ms / 98 lines of C++ on ext4 | All file metadata lives in SQLite tables. Kind/date-only queries are SQL (`filter_only`). |
| Full path as key made create/delete 10x faster than directory traversal | 10x on create/delete | Rejected for Cairn. We key by (parent, name) with node ids, because renames must be O(1): they *carry context* (D-026, D-028). Paths are rebuilt by walking parents. |
| DB invocation cost (~40 µs) vs a syscall (~1 µs) made DBOS reads much slower than ext4 | ~40x per call | **Never sit in the user's I/O path.** Cairn is a sidecar: user reads never touch it. |
| Batching narrowed the gap | – | Scanner commits 400 directories per transaction; jobs run in batches. |
| Provenance by change capture into a separate store | ~5% overhead | Planned event log (created/moved/renamed/tagged) kept apart from the hot index. |
| Durable execution: steps checkpointed in the DB; on recovery, completed steps return their recorded output | `operation_outputs(workflow_uuid, function_id, output, …)` in dbos-transact-py | Job result + job removal in one transaction, keyed by (node, stage, fingerprint) (D-020). |
| Polling was a significant overhead | – | Event-driven: inotify, condvars. Polling only as a fallback. |
| Block size was a single SQL update | – | Chunk size, model version and caps are data and constants, not formats. A model change is a new vector space. |

**Mistakes DBOS hit that we avoid:** putting the database on the hot path, and
assuming polling is acceptable.

## 2. Gifford et al., *Semantic File Systems* (MIT, SOSP '91)

* **Transducers:** user-programmable, per-file-type extractors that pull
  attributes out of files. Same as Cairn's extractors (`extract.rs`), and later
  user-pluggable ones.
* **Virtual directories:** a path *is* a query (`/sfs/ext:/pdf/owner:alice`).
  Existing tools work unchanged. Planned for Cairn as a FUSE mount, so every
  app's file picker can search. It needs a hang-proof design first: a hung FUSE
  daemon freezes any process that touches the mount.

## 3. LSFS: *From Commands to Prompts: LLM-based Semantic File System for AIOS* (ICLR 2025)

An LLM parses prompts into file-system API calls over a semantic index. The
paper reports better retrieval accuracy than a traditional file system. The
repository (agiresearch/AIOS-LSFS) is a research prototype that needs a full
LLM stack.

Lesson: the parsing step does not need an LLM for the common cases (dates,
kinds, scope). Cairn uses a grammar (D-018) and runs with zero generative
models.

## 4. Desktop indexers and similar tools: their issue trackers

| Tool | Issue | Lesson → decision |
|---|---|---|
| sist2 | #147 scan stops with a segfault; #325 crashes on OCR image; #349 crash on ebooks | A parser crash took down the whole indexer → disposable sandboxed workers (D-007) |
| sist2 | #397 indexing never completes; fixed by adding a timeout to Elasticsearch calls | Every external call has a deadline; watchdog kills workers (D-007, D-009) |
| sist2 | #44 "Invalid UTF-8 start byte" when indexing | Linux names are bytes: stored as BLOB, round-tripped over IPC (`WirePath`) |
| sist2 | #11 files with ’ – in names not found (charset mount option) | Never transcode names; use raw bytes end to end |
| khoj | #195 first-run indexing ran out of RAM (14 GB) | Bounded batches, streaming per-directory scan, memory caps, kill the embed worker on pressure (D-031) |
| reor | #74 CPU at 100% on all cores while typing | SCHED_IDLE + idle I/O for all background work, PSI back-off (D-031) |
| reor | #119 LanceDB IO error after upgrade, "all config is lost and need to reindex" | Schema versioning with quarantine-and-rebuild; user intent (exclusions) stored outside the index (D-033) |
| reor | #24 crash while initialising the vector DB | Vector store is a simple file with startup reconciliation (D-021) |
| Immich | #4117 CLIP model download failed, search broken | Models are explicit, verified, installable offline; search degrades to names/text without them (D-037) |
| Immich | #23462 ML memory grew until the machine was exhausted | Worker RLIMIT_AS, systemd MemoryMax, reap idle workers |
| Immich | #22982 CUDA crash after an upgrade | CPU-first; GPU optional later (D-044) |
| PhotoPrism | #1010 deleted folders stay; purge limited to 10k per run | Deterministic per-directory diff with verified deletion, no batch limits (D-027) |
| PhotoPrism | #1388 video times off by the UTC offset (QuickTime stores UTC) | Explicit time-zone rules per source (D-016) |
| PhotoPrism | #2320 prefer SubSec timestamps | EXIF: DateTimeOriginal → DateTimeDigitized; sub-seconds ignored for date filters |
| KDE Baloo | bug 404057 index grew to 17.6 GB, "balooctl stop" would not stop; bug 402154 duplicates on btrfs | Bounded storage (int8/1-bit vectors, caps), clean shutdown on SIGTERM, identity by (dev, inode) checked against the path |
| KDE Nepomuk (2000s) | RDF "semantic desktop" so heavy it was replaced by Baloo (2014) | Graph as plain SQLite tables plus query-time inheritance; "never slow the user" is the first requirement |
| GNOME Tracker / LocalSearch | high CPU loops retrying deleted or failing files; restart loops | Poison-job limit with backoff (D-020); staleness check by fingerprint before work |
| GNOME Tracker | documented: corrupted DB moved aside to `~/.cache/tracker3/files.$TIMESTAMP`, then reindex | Same pattern: quarantine, never delete (D-033) |
| SemSearch (arrase) | local FTS5 + vectors desktop search, depends on Ollama | Confirms FTS5 + vectors hybrid; Cairn avoids external model servers |

## 5. Common pitfalls checked in Cairn

| Pitfall | Where handled |
|---|---|
| Time zones / DST / leap years in date math | `jiff` with the system tz database; EXIF compared as civil time; tests cover leap Feb and quarter roll-over |
| EXIF zero dates, 1970 mtimes, future mtimes | `timeutil` plausibility rules |
| Log file growing to 100 GB | no log files; journald only; bounded `errors` table (D-030) |
| SQLite WAL growing forever | periodic `wal_checkpoint(TRUNCATE)`, `journal_size_limit`, short readers (D-038) |
| `PR_SET_PDEATHSIG` is per-thread | found by a test; workers spawn from the pipeline thread (D-008) |
| PID reuse when killing children | pidfd (D-009) |
| SIGPIPE killing a daemon on client disconnect | `SIGPIPE` ignored |
| Opening a FIFO blocks forever | `O_NONBLOCK` + regular-file check |
| Symlink loops / escaping the root | symlinks never followed, `O_NOFOLLOW` |
| Non-UTF-8 file names | raw bytes everywhere |
| SIGBUS on truncated mmap | no mmap of mutable files (D-022) |
| inotify limit exhaustion hurting editors | watch budget = half (D-024) |
| inotify queue overflow silently losing events | `IN_Q_OVERFLOW` → full rescan |
| Indexer evicting the page cache | `posix_fadvise(DONTNEED)` after reads |
| Unreadable directory read as empty → mass deletion | never delete on listing errors (D-027) |
| Zip bombs | per-member uncompressed cap (D-042) |
| Running two daemons on one index | `flock` instance lock |
| "may" the verb vs May the month | grammar rule (D-018) |
| Over-broad secret detection ("token" matched tokenizer.json) | narrowed word list; unit test |
| Research-only model licenses | license checked before adopting (D-013) |
| GTK theme bleeding into an app stylesheet (dark theme, `.card`, progress bars) | stylesheet at USER priority, unique class names, custom-drawn bar (D-060) |
| `pkill -f <binary path>` matching the calling shell itself | use `pkill -x <name>` / `pgrep -n` (dev tooling lesson) |
| candle CPU element-wise ops single-threaded; generic softmax slow | measured 0.8 chunks/s; moved text embedding to ONNX Runtime int8 (D-061) |
| `cargo build -p X --examples` builds only examples, not the binaries | caught a stale deployed CLI; build binaries explicitly |
