# Decision log

Every product and engineering decision made in Cairn, newest last. This file
is append-only: when a decision changes, add a new entry that supersedes the
old one (and mark the old one "Superseded by D-xxx") instead of editing
history. Each entry records what was decided, the alternatives, and why.

Status values: **Accepted**, **Superseded**, **Deferred** (decided not to do
yet), **Open** (needs the owner's call; see "Open questions" at the end).

---

## 2026-10-18 — Initial build

### D-001 Name: Cairn
Accepted. A cairn is a pile of stones that marks a trail; people leave
context (folder names, notes) and the search follows it. Binaries: `cairnd`
(daemon), `cairn` (CLI), `cairn-gui` (search window), `libcairn-nautilus.so`.

### D-002 License: Apache-2.0
Accepted. Permissive, with a patent grant, so open-source and commercial
paths both stay open. Third-party pieces keep their own licenses: vendored
candle CLIP code (MIT OR Apache-2.0), Nautilus extension headers
(LGPL-2.1-or-later; headers only, dynamic linking), models (MIT, see D-012/13).

### D-003 Languages: Rust everywhere, C for the Nautilus extension
Accepted. Rust gives memory safety and native speed for the daemon, the CLI and
the GUI. The Nautilus extension is C because it runs *inside* Nautilus, where a
runtime (Python) would add load time and risk; it is ~200 lines with no logic.
Rejected: Python daemon (speed, memory, packaging), C++ (memory safety).

### D-004 Do not modify the filesystem or kernel
Accepted. Cairn is a userspace sidecar index. No kernel modules, no
filesystem patches, no FUSE in the MVP. A kernel or filesystem bug on a work
machine means data loss. btrfs-specific features are accelerators layered on
later (D-043); everything works on ext4.

### D-005 Never write to user files
Accepted (answers open question 3 with "never"). Cairn opens user files
read-only (`O_RDONLY|O_NOFOLLOW|O_NONBLOCK|O_NOATIME`). Even xattrs are not
written: exclusions and future tags live in Cairn's database and are mirrored
to `exclusions.json` (D-033). Rejected: xattr tags (they write to user files
and are lost on FAT/USB/cloud copies anyway).

### D-006 Kernel-enforced sandbox (Landlock)
Accepted. The daemon restricts itself with Landlock so that all write-type
filesystem access is refused by the kernel except under its data and runtime
directories; TCP bind/connect is refused entirely. Workers restrict
themselves to *no writes at all* (except `/dev/null`) and refuse to start if
Landlock is unavailable (override: `CAIRN_ALLOW_NO_LANDLOCK=1`). A bug in
Cairn cannot modify user data because the kernel refuses, not because the
code is careful.

### D-007 Untrusted parsers run in disposable worker processes
Accepted. PDF/Office/XML/EXIF/image decoding handles untrusted bytes, so it
runs in `cairnd worker extract` processes with: Landlock (no writes),
RLIMIT_AS 1.5 GB, RLIMIT_FSIZE 0, RLIMIT_CORE 0, RLIMIT_NOFILE 256,
PR_SET_NO_NEW_PRIVS, PR_SET_PDEATHSIG(SIGKILL), SCHED_IDLE + idle I/O, a
per-request deadline enforced by a watchdog, and `catch_unwind` around each
request. A crash or hang kills a worker, never the daemon. (Lessons: sist2
#147/#325/#349 crashed the whole indexer on bad files.)

### D-008 Spawn workers only from a long-lived thread
Accepted after a bug found in testing. `PR_SET_PDEATHSIG` fires when the
*thread* that forked the child exits, not the process. Workers spawned from
short-lived scoped threads were killed mid-batch ("Broken pipe"). Workers are
now started only on the pipeline thread (`Worker::ensure_started`).

### D-009 Watchdog signals through a pidfd
Accepted. The watchdog kills a stuck worker via `pidfd_send_signal`, which
names that exact process. Plain `kill(pid)` could hit an unrelated process if
the pid was recycled. Requires Linux 5.3+.

### D-010 Batch failures never punish innocent files
Accepted. If a worker dies on one file, the rest of its batch is marked "not
attempted" and stays queued without counting as a failure. Only the file that
was being processed gets an attempt recorded.

### D-011 Embedding runs in its own idle-priority worker; query embedding in the daemon
Accepted. Bulk embedding (`cairnd worker embed`) runs at SCHED_IDLE, so it
only uses CPU nobody else wants. Query embedding runs in the daemon at normal
priority so search stays fast while the machine is busy. The two processes
load different CLIP towers (daemon: text, worker: vision), which avoids
holding the full model twice.

### D-012 Text embedding model: BAAI/bge-small-en-v1.5
Accepted. 33M parameters, 384 dimensions, MIT license. Query embedding takes about
60 ms on CPU. No generative LLM anywhere: the owner asked for "tiny"
models, and query understanding is done by a deterministic grammar (D-018).
Rejected: 1–3B instruction models (unnecessary, heavy).

### D-013 Image model: LAION CLIP ViT-B/32 (laion2B-s34B-b79K)
Accepted. MIT license, ~150M parameters, about 90 ms per image on CPU.
Rejected: Apple MobileCLIP. It is smaller, but its weights are under the Apple
ML Research license, which is research-only and so unusable on a work machine
or in a product. SigLIP-base deferred: better quality, but about 4x the compute.

### D-014 Inference library: candle (pure Rust)
Partly superseded by D-061 (text embedding moved to ONNX Runtime; CLIP stays on candle). No C++ runtime, no binary downloaded at build time, so offline builds
are simple. Revisit ONNX Runtime with int8 if indexing throughput becomes the
bottleneck.

### D-015 Vendored candle CLIP with a correctness fix
Accepted. candle 0.11 hard-codes QuickGELU in the CLIP text tower, but LAION
weights use exact GELU. Cairn vendors the CLIP module
(`crates/cairn-ml/src/clip`) with the activation made configurable and the
towers loadable separately. To upstream later.

### D-016 Time semantics
Accepted.
* Filesystem times are UTC instants. They are converted to civil time in the
  system zone only when compared against a query.
* EXIF capture times are the camera's civil time and are compared as civil
  time, without conversion. A photo taken at 23:30 on 31 Dec abroad is
  "December".
* Implausible times count as unknown: EXIF "0000:00:00", before 1990, or more
  than a day in the future.
* Videos: QuickTime times are UTC (PhotoPrism #1388). Not parsed in the MVP.

### D-017 A file's origin date
Accepted. The origin date is the EXIF capture time if present. Otherwise it is
`min(btime, mtime)`. Reason: a copy that did not preserve times has a new btime
but often an old mtime, and a file edited in place has a new mtime but its old
btime. Taking the earlier of the two is right in both cases.

### D-018 Deterministic query grammar, no LLM
Accepted. Recognised:
* months, with or without a year
* "last/this <month|year|month|week|quarter>"
* quarters: Q1–Q4, "Q4'25", "fourth quarter of 2025", "25's last quarter"
* "past N days|weeks|months", "N units ago"
* yesterday and today
* ISO dates: 2025-12, 2025-12-14, 12/2025
* bare years
* kind words: photos, slides, script…
* scope words: here, this folder, this project

Rules:
* "may" counts as a month only next to a year or date word.
* Two-digit years count only in a date context.
* A bare month means the most recent one that is not in the future.
* Seasons are not supported (hemisphere ambiguity).

### D-019 Date filters: hard for photos/videos, a boost otherwise
Accepted. "Photos from December" must exclude other months. "Sales report
for Q4 2025" must still find a file last edited in January 2026, so for
non-media files a matching date multiplies the score by 1.6 instead of
filtering. Quarter and year words are also added to keyword search.

### D-020 Durable, idempotent job pipeline (DBOS-style step checkpoints)
Accepted.
* Each job is keyed by (node, stage, fingerprint).
* A job's result and its removal commit in **one** SQLite transaction.
* A crash re-runs at most the in-flight batch, and re-running is harmless
  because results replace earlier ones.
* Before committing, the fingerprint is re-checked and stale results are
  discarded.
* After 3 failures (with 4 and 16 minute backoff) the job is dropped. Only an
  `errors` row remains, and the file is retried when it changes.

This avoids the GNOME Tracker failure where the indexer loops forever on a
failing file.

### D-021 Own append-only vector files; fsync before commit
Accepted. One file per embedding space holds int8 vectors with a per-vector
scale. Write order:
1. append the vectors to the file and fsync it,
2. commit the database rows that reference those slots,
3. publish them to search.

The database stores the committed slot count. On startup, any tail beyond that
count is truncated: it can only come from a crash between steps 1 and 2. A file
shorter than the count means corruption, and that space is rebuilt
automatically.

### D-022 No mmap of mutable files
Accepted. A mapped file that gets truncated raises SIGBUS. Vector rescoring
uses `pread`. Only immutable, hash-verified model files are mapped.

### D-023 Search index: brute-force 1-bit codes + int8 rescoring (no HNSW)
Accepted. Sign-bit codes live in RAM: 48 bytes per text vector, 64 per image
vector. The search is a Hamming scan followed by exact int8 rescoring of the
best 256+ candidates.

Rejected HNSW: it is memory-hungry, awkward under constant updates, and adds
another structure that needs crash recovery. Revisit above about 5M vectors.

### D-024 inotify within a budget, plus periodic rescans
Accepted. Cairn watches at most half of `max_user_watches` (cap 200k), so
editors and other tools keep theirs. Directories beyond the budget are caught
by a full rescan every 3 hours. `IN_Q_OVERFLOW` triggers a full rescan. Events
only mark directories dirty; one code path (the scanner diff) mutates the
index.

### D-025 File fingerprint = (size, mtime_ns, inode)
Accepted. ctime is excluded on purpose: renames update ctime on btrfs, which
would force re-extraction of every moved file.

### D-026 Rename/move keeps identity
Accepted. A new directory entry counts as a move when all of these hold:
* an indexed node has the same (dev, inode),
* its size and mtime match (for files),
* its old path no longer holds that inode.

The node id is kept, so its text and vectors survive the move. Hard links stay
separate entries.

### D-027 Deletion safety
Accepted.
* A directory that cannot be listed completely (EACCES, EIO) is never treated
  as empty, so nothing under it is deleted from the index.
* Deletions are deferred to the end of a pass and re-verified on disk.
* An interrupted full scan deletes nothing.
* A missing index root is skipped, never emptied.

Lesson from PhotoPrism #1010: deleted folders stayed in the index forever.
Cairn removes them, but only after verifying.

### D-028 Folder context and inheritance
Accepted. This is the core feature. A folder's context text is made of:
* its name (camelCase and separators split),
* the first ~1800 characters of user-written notes inside it
  (README\*, notes\*, about\*, description\*, any .txt/.md/.org/.rst),
* up to 40 child names.

It is embedded once per folder, and the embedding is skipped when the text hash
is unchanged.

At query time, files (not subfolders) inherit a matching folder's score:
* down to 3 levels,
* decaying by 0.6 per level,
* and the most specific matching folder wins (an ancestor of another matched
  folder does not pass context down).

Because inheritance is computed at query time, renaming a folder of 10,000
photos updates one row.

### D-029 Ranking: reciprocal rank fusion
Accepted. The ranked lists are:

| List | Source | Weight |
|---|---|---|
| content | bge on chunks | 1.0 |
| image | CLIP | 1.0, or 1.3 when the query asks for images |
| name | FTS on names and folder notes | 0.9 |
| text | FTS on extracted text | 0.6 |
| folder | inheritance | 0.85 |

The lists are combined with RRF (k = 60). RRF works on ranks, so the two
models' different score scales do not need calibrating against each other.

### D-030 Logging cannot grow without bound
Accepted. Logs go to stderr only (the journal under systemd, which enforces
its own limits). Cairn never writes its own log file. Per-file failures go
into the `errors` table, at most one row per (file, stage), and are logged
only at debug level. A progress summary is logged at most every 10 minutes.

### D-031 Never slow the user down
Accepted. Indexing pauses when:
* on battery,
* MemAvailable is below 1.5 GB,
* PSI memory `some avg10` > 5,
* PSI io > 25,
* PSI cpu > 40.

On memory pressure the embedding worker is killed to give RAM back. Idle
workers are reaped after 60 s (extract) or 120 s (embed). Bulk reads call
`posix_fadvise(DONTNEED)` so indexing does not evict the user's page cache.
Override for tests: `CAIRN_IGNORE_THROTTLE=1`.

### D-032 Local IPC only
Accepted.
* Unix socket in a 0700 runtime directory; the socket file is 0600.
* `SO_PEERCRED` must match the daemon's uid.
* One JSON line in, one out, with a 64 KB request cap, 5 s read and 10 s write
  timeouts, and at most 16 concurrent clients.
* SIGPIPE is ignored so a client that disconnects cannot kill the daemon.

### D-033 Index integrity and recovery
Accepted. `PRAGMA quick_check` runs at startup. On failure, or when the schema
version changes, the index is **moved** to `quarantine-<ts>/`, never deleted,
and rebuilt. Exclusions are mirrored to `exclusions.json` (atomic write) and
restored into a fresh index. GNOME Tracker's documented "move aside and
rebuild" approach validated this.

### D-034 Similarity floors
Accepted. Calibrated on the test corpus:

| Model | Relevant | Unrelated |
|---|---|---|
| bge | ≥ 0.63 | ≤ 0.57 |
| CLIP | ≥ 0.24 | ≤ 0.21 |

Floors: text 0.60 and within 0.10 of the best hit; CLIP 0.22 and within 0.06
of the best hit. Junk queries return nothing instead of noise. **Re-calibrate
on a real corpus** (open task).

### D-035 Nautilus integration: minimal C extension, separate GUI process
Accepted (answers open question 1 with "a", extension). The extension adds:
* "Search with Cairn…" on the folder background,
* a Cairn submenu on a folder: search here, exclude, include.

It only spawns `cairn-gui` / `cairn` with argv (no shell) and never does I/O in
the Nautilus process. It builds against vendored Nautilus 46.4 headers, so no
`-dev` package is needed, and links the system `libnautilus-extension.so.4`.
Nautilus loads extensions only from its system directory, so installing needs
root once.

Later: FUSE virtual folders, so every app's file picker can search (Gifford
SFS idea, needs watchdog design); a Nautilus fork with a native search
provider.

### D-036 GUI: plain GTK4 (no libadwaita)
Accepted. libadwaita is not installed on the dev machine, and plain GTK 4.8+ is
enough. The GUI:
* opens code/text hits in Zed at the line (`CAIRN_EDITOR` overrides),
  and anything else with the default app,
* Ctrl+Enter selects the file in Nautilus (`nautilus --select`),
* shows existing thumbnails only, never generating new ones.

### D-037 Models are pinned and verified; the daemon has no network code
Accepted. Model files are pinned by Hugging Face revision and SHA-256 in
`crates/cairn-ml/src/lib.rs`. The large files' hashes match the HF LFS oids,
and the small JSON files were hashed on first download (trust on first use over
HTTPS). Model files are verified before every load (cached by size + mtime).

Only `cairn models fetch` touches the network, through `curl --proto =https`.
`cairn models install <dir>` exists for airgapped machines. The systemd unit
adds `RestrictAddressFamilies=AF_UNIX`.

### D-038 SQLite configuration
Accepted:
* WAL, `synchronous=NORMAL` (a power cut loses at most the last transactions,
  never corrupts),
* `journal_size_limit` 64 MB,
* `wal_autocheckpoint=1000`, plus a TRUNCATE checkpoint every 10 minutes and at
  shutdown (prevents unbounded WAL growth),
* `mmap_size=0`, `busy_timeout` 10 s,
* short read transactions.

### D-039 Exclusion semantics
Accepted (answers open question 2). Excluded folders are fully invisible. An
exclusion is matched by path *and* (dev, inode), so a renamed excluded folder
stays excluded. The same effect comes from user-created marker files: `.noindex`,
`.cairnignore`, `CACHEDIR.TAG`, `.nomedia`, and `pyvenv.cfg` (Python
virtualenvs). Excluding an index root is refused; use `cairn pause` instead. A
running scan picks up new exclusions immediately.

### D-040 Index scope for the MVP
Accepted. Only `$HOME` is indexed (`CAIRN_ROOTS` overrides). Skipped:
* hidden directories,
* `node_modules`, `__pycache__`, `site-packages`, `bower_components`, `snap`,
  `lost+found`,
* other filesystems, mounts and btrfs subvolumes (no crossing `st_dev`),
* symlinks (never followed),
* FIFOs, sockets, devices,
* Cairn's own data directory.

System directories are out of scope until "power user mode".

### D-041 Secrets are indexed by name only
Accepted. The content of these files is never read, embedded or returned:
* key and keystore extensions: `*.pem`, `*.key`, `*.p12`, `*.kdbx`, `*.gpg`,
  `*.tfstate`,
* `*.env` and `.env*`, `id_rsa*`,
* names containing password, credential, secret, private_key, wallet, …

Example: `app-db-passwords.env`, `db-ca.pem` and `redis.secret` in `$HOME`
are findable by name, but their contents never enter the index.

### D-042 Content caps
Accepted:
* files over 64 MB: metadata only,
* extracted text: capped at 1 MB per file, at most 400 chunks per file,
* PDFs: first 60 pages,
* zip members: capped at 16 MB uncompressed (zip-bomb guard),
* images: capped at 16384 px per side and 512 MB decode allocation,
* JPEGs: decoded with DCT downscaling (4–8x faster).

### D-043 btrfs accelerators deferred
Deferred. Planned:
* `BTRFS_IOC_TREE_SEARCH_V2` enumeration and `find-new` catch-up through a
  small read-only root helper,
* FIEMAP shared-extent duplicate detection,
* the index in its own subvolume once `@home` is snapshotted.

Not needed for correctness.

### D-044 GPU deferred
Deferred. CPU-only candle for the MVP. The GTX 1650's driver state on this
hybrid-graphics laptop is unverified, and CPU indexing at idle priority is
enough. A CUDA build feature comes later.

### D-045 .gitignore not honoured in the MVP
Deferred. Hidden directories, `node_modules` and the marker files cover most
generated content. Honouring `.gitignore` is planned (it would also hide
`.env` files and build outputs automatically).

### D-046 Testing approach
Accepted.
* Development never indexes the real home. `tests/env.sh` points
  `CAIRN_DATA_DIR`, `CAIRN_RUNTIME_DIR`, `CAIRN_ROOTS` and `CAIRN_MODELS_DIR`
  at a scratch directory.
* `tests/make_corpus.sh` builds a synthetic home with the owner's scenarios and
  hostile entries: a non-UTF-8 name, a FIFO, a symlink loop, an unreadable
  directory, secrets.
* `tests/chaos.sh` SIGKILLs the daemon at random points and checks:
  (1) the corpus is bit-for-bit unchanged, including mtimes,
  (2) no corruption or quarantine,
  (3) the final index equals a clean build,
  (4) no orphaned workers.

### D-047 Data directory is a plain directory
Accepted for now. `~/.local/share/cairn` (0700). Making it a btrfs subvolume
(to keep index churn out of snapshots) is deferred until `@home` is
snapshotted, and is tracked with D-043.

### D-048 Folder context waits 20 s for children
Accepted. Folder context jobs are delayed 20 s
(`CAIRN_FOLDER_CTX_DELAY_MS`), so the notes inside a new folder are extracted
before the folder is embedded.

### D-049 Launcher integration: `cairn launch` wraps fuzzel
Superseded by D-055. The owner's Win+D launcher is fuzzel 1.9.2, which has no plugin API.
`cairn launch` lists installed applications (XDG `.desktop` files, icons
included) through `fuzzel --dmenu`. Text typed with a leading `/` becomes a
Cairn query, and its results open in a second fuzzel list (Enter opens code at
its line in Zed, anything else with `xdg-open`). Apps start via `gio launch`.

Trade-offs:
* Lost: fuzzel's own frecency ordering and desktop actions.
* No live as-you-type file results (dmenu lists are static).

Rejected: switching to a launcher with plugins (walker, anyrun, vicinae).
That is the owner's choice and would change their workflow. Also usable
directly: `cairn launch /query`. Changing the niri binding is left to the owner
(D-051).

### D-050 GUI direction
Superseded by D-060. The launcher and Nautilus are the primary
surfaces; `cairn-gui` stays as a small fallback search window. A future
settings and status app should be "cozy": warm yellow tint, playful rounded
fonts, in the soft style popular with vibe-coded apps. Exact style is an open
question (Q10).

### D-051 Never edit the owner's desktop configuration without asking
Accepted. Install steps that touch niri, Nautilus' system directory or
systemd enablement are printed for the owner to run, not done automatically.

### D-052 A marker file appearing inside a watched folder excludes that folder at once
Accepted after a failing behaviour test. When a dirty folder is re-listed, the
folder itself is first re-checked for exclusion markers. Removing a marker is
noticed on the next full rescan or `cairn rescan`, because an excluded folder
is not watched.

### D-053 Write transactions use BEGIN IMMEDIATE
Accepted after a failure on the owner's real home. With two writers (scanner
and pipeline), DEFERRED transactions that read before writing failed at once
with "database is locked": in WAL mode the read→write upgrade bypasses
busy_timeout. That killed the scanner and flooded the journal.

Fixes:
* All write transactions use `db::write_tx` (IMMEDIATE).
* Scanner transactions are capped at 250 ms or 400 directories.
* The scanner retries a failed full scan with backoff instead of exiting.
* Repeated pipeline errors are logged once per minute with a repeat count.

The synthetic corpus was too small to trigger this. Lesson: test with
realistic volume.

### D-054 Integrity check moved off the start-up path
Accepted. `PRAGMA quick_check` took minutes on the 2.5 GB index, so the socket
did not exist and every client failed ("is cairnd running?"). It now runs in
the background at idle priority, 30 s after start-up. On failure it writes a
`needs-quarantine` marker and exits non-zero; systemd restarts the daemon,
which quarantines and rebuilds. Start-up time went from minutes to about 2 s.
SQLite WAL recovery already guarantees consistency after crashes, so the check
is a defence against hardware or bit-rot corruption, not crash recovery.

### D-055 File search gets its own key; the app launcher stays untouched
Accepted, superseding D-049. Replacing fuzzel's app mode broke the owner's
Win+D workflow (different ordering and behaviour), so it was reverted
immediately.

* Win+D stays plain `fuzzel`.
* Win+/ (`Mod+Slash`) runs `cairn launch --find`: a fuzzel prompt "find files ›".
  The query's results appear in a second fuzzel list; arrows move, Enter opens
  (code at its line in Zed, anything else with xdg-open).
* "No results" says when indexing is still in progress.
* `cairn launch` without `--find` still exists (apps, or any non-app text
  searches files) but is not bound.

Bindings live in the user's niri config. Back it up before editing
(see docs/INTEGRATIONS.md).

### D-056 Folder references in queries limit the search
Accepted (owner request). Words after "from / in / inside / under / within"
(optionally "my/the/our/your …", up to 3 words, optionally "… folder") are
tried as a folder name, longest phrase first. A phrase limits the search only
when it names a folder **exactly**:
* matching ignores case, spaces and punctuation, and allows plural/singular
  ("download" → "Downloads"),
* the shallowest match wins, and a tie counts as ambiguous.

Otherwise the words stay ordinary search words, so "notes from nowhereland"
behaves as before. Dates are recognised first ("photos from december" is a
date, not a place). Partial names ("manali trek" for "Manali trek Dec 25") do
not limit the search; folder-context inheritance already ranks those files
first. The resolved folder is shown as "only in ~/Downloads" in the CLI, the
fuzzel title and the GUI.

### D-057 `cairn search --in DIR`
Accepted. Same scoping as `--here`, for any folder. Both limit results to that
folder and everything below it.

### D-058 Search and status must not scan millions of rows
Accepted after measuring on the real index (2.7M chunks, 6 GB). Semantic search
took 7.8 s and status 2.6 s. Causes:
* prefix matching (`deploy*`) on the chunk full-text index: 5.4 s,
* `count(*)` over jobs on every search: 1.7 s,
* `count(*)` over nodes and chunks on every status call.

Fixes:
* chunk full-text uses exact words, all words first and any word only as a
  fallback, ranked with FTS5 `rank` (the vectors cover word variants);
* `names_fts` got a prefix index (`prefix='2 3 4'`), added by an in-place
  migration (`m1_names_prefix`), so name prefixes stay;
* counts come from a background stats thread refreshed every 20 s;
* subtree deletion is set-based through a temp table (excluding a 298k-item SDK folder:
  14 s instead of a timeout).

Result on the real index: searches take 0.03–0.4 s. The remaining time is
mostly the two query embeddings (~70 ms) and full-text lookups. Target
< 100 ms: next steps are a query-embedding cache and a warm page cache for the
FTS indexes.

### D-059 Owner exclusions of vendored trees
Accepted. On the development machine, three large vendored source/SDK trees
were excluded at the owner's request. Everything else in `$HOME` keeps indexing.
This is user configuration, not product behaviour: undo with
`cairn include <dir>`.

### D-060 Cairn app: the cozy "warm paper" GUI, opened by `cairn`
Accepted (owner request: cozy, warm yellow, the popular vibe-coded look, Apple
design philosophy, feature-rich).

Pages:
1. **Search:** a hero question, a big pill search field, kind chips
   (All/Photos/Documents/Slides/Sheets/Code/Folders) and a folder scope picker.
   An "understood" line explains the query. Results show as cards (badge or
   thumbnail, line range, path, why-chips, code snippet, Open / Show in folder /
   Copy path) or as a 4+ column photo grid when most hits are images. Keys:
   ↑↓, Enter, Ctrl+Enter, Ctrl+C, Ctrl+K, Esc.
2. **Index:** stat tiles, per-stage progress, Pause/Resume, Rescan, the safety
   promises (read-only, no network, idle priority), resting reason, size,
   models.
3. **Folders:** exclusions with "Include again", "Exclude a folder…", and what
   is skipped automatically.
4. **Tips & keys:** example queries, how to give files context, shortcuts, CLI,
   about.

Design:
* Palette: cream paper, honey accent, cocoa ink, sage for "done".
* Fonts: Fraunces for display, Nunito for UI, JetBrains Mono for
  paths and code. Generous radii, soft shadows.
* Fonts are embedded (SIL OFL) and registered for the process only, via
  `FcConfigAppFontAddFile` and a copy in `$XDG_RUNTIME_DIR`. Nothing is
  installed.

Technical:
* The look is always the warm light one: the stylesheet is applied at USER
  priority and `prefer-dark` is off, because the owner's dark system theme
  otherwise bled through. Theme collisions found by screenshot: `.card`
  (renamed `cn-card`) and GtkProgressBar sizing (replaced by a cairo-drawn
  bar).
* Thumbnails come from the freedesktop cache, else a bounded decode (≤ 60 MB)
  on a worker thread. Only raw bytes cross threads.
* Bare `cairn` execs `cairn-gui`. The desktop entry is "Cairn".
* Search requests may carry `kinds` (chip filter); see `ipc::Request::Search`.

Not built yet: a dark variant, and settings for editor and keys (Q10 is still
open for further style direction).

### D-061 Text embedding on ONNX Runtime, int8
Accepted after measuring on the real index. candle's BERT ran at 0.8 chunks/s:
* its element-wise ops (GELU, masks, generic softmax) are single-threaded,
* and sequences were 512 tokens.

Vendoring BERT with the fused `softmax_last_dim` and 256-token sequences gave
3.6/s, still days for this home. ONNX Runtime
(`ort` 2.0.0-rc.14, statically linked, built against ORT 1.31) runs bge-small:
* fp32: 11 chunks/s, cosine 1.000 vs candle,
* int8 (transformers.js quantisation of the same weights, pinned by revision
  and SHA-256): 14 chunks/s, cosine 0.996–0.997.

Chosen: int8. Query embedding dropped from ~60 ms to ~10 ms. CLIP stays on
candle; its speed is acceptable.

A model change resets the text vector space and re-queues its jobs
(`TEXT_MODEL_TAG` in meta), so models are never mixed.

Airgap cost: `ort` downloads the ORT static library at **build** time. Offline
builds point `ORT_LIB_LOCATION` at a local copy (docs/AIRGAP.md). At runtime
nothing changes: no network, no new system dependency.

### D-062 Semantic vectors for the start of each file; keywords for all of it
Accepted. The real index had 1.85M chunks, 40% of them CSV rows. Embedding all
of them would take days even at the new speed, for little gain.

Rules:
* data files (csv/tsv/json/jsonl/xml/log/subtitles): the first chunk (header
  and first rows) gets a vector;
* every other file: its first 16 chunks.

Every chunk stays in full-text search. This means about 0.45M vectors instead
of 1.85M.

Text embedding order: documents (pdf, docx, slides) first, then notes and text,
then code and sheets, then the rest, so the most useful meaning search arrives
first.

### D-063 "Idle" only when the counts post-date the last full scan
Accepted after the crash test misread a fresh daemon as idle. With counts
cached (D-058), a just-started daemon reported "idle, 0 files" before its first
scan had been counted. Fixes:
* status reports busy until a stats snapshot that *started* after the last full
  scan finished exists;
* the stats thread is woken when the scan finishes and when the pipeline runs
  out of work, so "idle" appears within seconds instead of up to 20 s.

---

## Open questions (owner's call)

| # | Question | Default until answered |
|---|---|---|
| Q5 | What may AI agents (Claude via MCP) see: paths + snippets, paths only, or per-folder? Snippets leave the machine. | No MCP server yet; `cairn search --json` exists. Secrets never content-indexed. |
| Q6 | Enable snapper on `@home` and index old versions? | Not enabled; no version history. |
| Q7 | Allow a small read-only root helper (fanotify whole-fs, btrfs tree search)? | Unprivileged inotify + periodic rescans. |
| Q8 | Allow local usage tracking (which files you open) for ranking and "worked together" links? | Not built. |
| Q10 | Style of the cozy settings/status app (palette, fonts, toolkit)? | Not built; `cairn status` + `cairn-gui` fallback. |
| Q9 | Should `.gitignore` be honoured (hides build outputs and .env, but also data dirs you may want)? | Not honoured (D-045). |

When a question is answered, add a decision entry and remove the row.
