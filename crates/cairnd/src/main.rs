//! cairnd: the Cairn indexing and search daemon.
//!
//! Process model (see docs/ARCHITECTURE.md):
//!   cairnd                  daemon: scanner, watcher, job pipeline, search, socket
//!   cairnd worker extract   sandboxed parser for untrusted file bytes
//!   cairnd worker embed     sandboxed embedding model runner (SCHED_IDLE)

mod daemon;
mod pipeline;
mod scanner;
mod search;
mod server;
mod shared;
mod watcher;
mod worker_main;
mod workers;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let result = match args.get(1).map(String::as_str) {
        Some("worker") => worker_main::run(args.get(2).map(String::as_str).unwrap_or("")),
        None | Some("run") => daemon::run(),
        Some("--version") => {
            println!("cairnd {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some(other) => Err(anyhow::anyhow!("unknown command {other:?}; usage: cairnd [run]")),
    };
    if let Err(e) = result {
        eprintln!("cairnd: {e:#}");
        std::process::exit(1);
    }
}
