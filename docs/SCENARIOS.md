# Scenario map (journey-first analysis)

The distinct behavioural scenarios analysed before and during the build, with
their expected outcome and how each is verified. "Corpus" means the synthetic
home built by `tests/make_corpus.sh`. "Chaos" means `tests/chaos.sh`. "Unit"
means `cargo test`.

| # | Scenario | Expected outcome | Verified by |
|---|---|---|---|
| 1 | First run on a large home; user searches before indexing finishes | Results from what is indexed so far; response flagged `partial` ("index still building") | Corpus (status/partial flag) |
| 2 | Photo dump with no context, "photos of mountains" | CLIP finds the mountain photos only | Corpus: Matterhorn and K2 returned, nothing else |
| 3 | "all photos from December last year" | Pure SQL by kind + origin date; no model needed | Corpus: exactly the Dec-2025 photo, 0 ms |
| 4 | Photo edited/copied later (mtime/btime changed) | EXIF capture time wins; otherwise min(btime, mtime) | Unit (timeutil), code review; real EXIF photos in corpus |
| 5 | User renames a folder to add context ("Manali trek Dec 25") | Folder context re-embedded (one row); children inherit at query time | Corpus + tests/behaviour.sh (rename makes deploy.sh findable by new folder name) |
| 6 | User adds notes.txt inside a folder | Folder context includes the notes; files below inherit | Corpus: Manali notes |
| 7 | File moved between folders | Same node id, vectors kept; context changes with the new parent | Unit (db move); scanner move detection |
| 8 | Folder deleted | Removed after on-disk re-verification | Chaos (final index equals clean build) |
| 9 | Duplicate copies of a file | Indexed separately (dedup by content hash deferred) | Known gap |
| 10 | "function that parses config" | Code chunk with `file:line` range | Corpus: utils.py:1-15 |
| 11 | "script that runs terraform on aws" | deploy.sh:1-7, kind=code | Corpus |
| 12 | Opaque binary (game save) | Found by name and folder ("elden ring save") | Corpus |
| 13 | Secret files (.env, *.pem, *passwords*) | Name searchable, content never read | Corpus: the SECRET marker string is never returned |
| 14 | Generated dirs (node_modules, CACHEDIR.TAG, venvs) and hidden dirs | Not indexed | Corpus |
| 15 | Machine busy / on battery / low memory | Indexing pauses; search still answers | Code (throttle_reason); `cairn status` shows the reason |
| 16 | Event storm (npm install, git checkout, unzip) | Directories marked dirty, debounced, re-listed once | Code; to stress-test |
| 17 | inotify queue overflow | Full rescan | Code |
| 18 | More directories than the watch budget | Rest covered by the 3-hourly full rescan | Code (budget) |
| 19 | External drives, other subvolumes, network mounts inside home | Not crossed (st_dev boundary): no hangs on dead mounts | Code |
| 20 | Embedding model upgraded | New vector space / re-embed (schema/version bump) | Design (D-021); not yet exercised |
| 21 | Query with no good match | Empty result instead of noise | Corpus: junk queries return 0 results |
| 22 | "that project I started but never finished" | Folder/name/notes match ("robot-arm" + plan.md) | Corpus; real detection is Phase 5 |
| 23 | Archive contents | Not indexed (name only) | Known gap |
| 24 | Multiple users on one machine | Per-user daemon, data dir and socket (0700); SO_PEERCRED | Code |
| 25 | Crash during text extraction | Job re-runs; no duplicate chunks | Chaos |
| 26 | Crash during embedding (after vector fsync, before DB commit) | Orphan tail truncated at start; job re-runs | Unit (vecstore) + Chaos |
| 27 | Crash during a DB transaction | SQLite WAL rollback | Chaos (no quarantine needed) |
| 28 | Shutdown / reboot mid-batch | SIGTERM: threads stop, WAL checkpoint, clean exit; workers die with the daemon | Chaos (orphan check) |
| 29 | Power loss | WAL + synchronous=NORMAL: last transactions may be lost, never corruption; startup quick_check | Design; same code paths as 26–27 |
| 30 | File modified while being read | Fingerprint check at commit discards the stale result; the new job runs | Code (still_same) |
| 31 | Folder excluded after it was indexed (CLI/Nautilus or `.noindex`) | Subtree removed immediately; scanner reloads exclusions mid-scan | tests/behaviour.sh |
| 32 | Excluded folder renamed/moved | Still excluded (matched by dev+inode) | tests/behaviour.sh |
| 33 | Excluded folder copied to USB | Out of scope (other fs, not indexed) | – |
| 34 | File moved into an excluded folder | Disappears from results (subtree not indexed) | Code |
| 35 | Index churn bloating btrfs snapshots | Not applicable now (`@home` not snapshotted); D-047 | – |
| 36 | GPU contention with games | No GPU use in the MVP | D-044 |
| 37 | Special files (FIFO, socket, device), symlink loops | Ignored; never opened blocking; never followed | Corpus (pipe.fifo, loop) |
| 38 | Non-UTF-8 file names | Indexed, searchable, round-trip over IPC | Corpus (`caf\xe9.txt` found by "espresso"); Unit (WirePath) |
| 39 | Unreadable directory | Left unchanged in the index, never treated as empty | Corpus (`locked/`) |
| 40 | Worker crash or hang on a hostile file | Worker killed (watchdog/pidfd), file poisoned after 3 tries, rest of batch unaffected | Code; PDEATHSIG bug found and fixed (D-008) |
| 41 | Two daemons started | Second refuses (flock) | Code |
| 42 | Client disconnects mid-reply | EPIPE handled; SIGPIPE ignored | Code |
| 43 | Index database corrupted | Moved to `quarantine-*`, rebuilt, exclusions restored | Code (open_index); to add a fault-injection test |
| 44 | Vector file shorter than DB claims | That space rebuilt (jobs re-queued) | Unit (vecstore refuses) + code |
| 45 | Models missing (airgapped, first run) | Names/dates/full text work; semantic search activates when models are installed | Code; `cairn doctor` |
| 46 | User types "/query" in the Win+D launcher (fuzzel) | Results list in fuzzel; Enter opens file (code at line) | Manual (`cairn launch`); flags checked against fuzzel 1.9.2 |
| 47 | "fire photos from downloads" / `--in DIR` / `--here` | Results only from that folder; unknown place names stay search words | Unit (query) + tests/behaviour.sh + real index |
| 48 | User opens `cairn` with a dark system theme | Warm light Cairn look regardless of theme; all pages readable | Screenshots of all four pages (docs/screenshots) |
