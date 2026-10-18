//! cairn: command-line client for cairnd.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use cairn_core::ipc::{self, Request, Response, WirePath};
use cairn_core::paths;

mod launch;

const USAGE: &str = "usage:
  cairn                       open the Cairn app
  cairn search [--here | --in DIR] [--json] [--limit N] <query...>
                              (or say it: fire photos from downloads)
  cairn status [--json]
  cairn exclude [--notify] <path>   keep a folder out of search (removes it from the index)
  cairn include [--notify] <path>   undo exclude
  cairn pause | resume | rescan
  cairn models fetch          download pinned models (needs network, run once)
  cairn models install <dir>  install models from a directory (airgapped machines)
  cairn models list           print URLs and SHA-256 for manual download
  cairn models verify
  cairn doctor                check the environment
  cairn launch [/query]       fuzzel app launcher; text starting with / searches files";

fn main() {
    if let Err(e) = run() {
        eprintln!("cairn: {e:#}");
        std::process::exit(1);
    }
}

fn call(req: Request, timeout: u64) -> Result<Response> {
    match ipc::call(&req, Duration::from_secs(timeout))? {
        Response::Error { message } => bail!("{message}"),
        r => Ok(r),
    }
}

fn abs_path(p: &str) -> Result<Vec<u8>> {
    let path = Path::new(p);
    let abs = if path.is_absolute() { path.to_path_buf() } else { std::env::current_dir()?.join(path) };
    // Normalise "." and ".." lexically; do not resolve symlinks.
    let mut out = PathBuf::new();
    for c in abs.components() {
        match c {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            c => out.push(c),
        }
    }
    Ok(paths::to_bytes(&out))
}

fn tilde(p: &str) -> String {
    match std::env::var("HOME") {
        Ok(h) if !h.is_empty() && p.starts_with(&h) => format!("~{}", &p[h.len()..]),
        _ => p.to_string(),
    }
}

fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(cmd) = args.first() else {
        // Bare `cairn` opens the app (cairn-gui next to this binary, else PATH).
        use std::os::unix::process::CommandExt;
        let sibling = std::env::current_exe().ok().and_then(|e| e.parent().map(|d| d.join("cairn-gui"))).filter(|p| p.is_file());
        let gui = sibling.unwrap_or_else(|| PathBuf::from("cairn-gui"));
        let err = std::process::Command::new(&gui).exec();
        eprintln!("cairn: could not start {} ({err}); command-line usage:\n{USAGE}", gui.display());
        std::process::exit(1);
    };
    let rest = &args[1..];
    match cmd.as_str() {
        "search" | "s" | "find" => search(rest),
        "status" => status(rest.iter().any(|a| a == "--json")),
        "exclude" | "include" => {
            // --notify: also show a desktop notification (used by Nautilus).
            let notify = rest.iter().any(|a| a == "--notify");
            let p = rest.iter().find(|a| *a != "--notify").context("missing path")?;
            let path = WirePath::from_bytes(&abs_path(p)?);
            let req = if cmd == "exclude" { Request::Exclude { path } } else { Request::Include { path } };
            let result = call(req, 60);
            let msg = match &result {
                Ok(Response::Ok { message }) => message.clone(),
                Ok(_) => "unexpected response".to_string(),
                Err(e) => format!("failed: {e:#}"),
            };
            if notify {
                let _ = std::process::Command::new("notify-send").args(["--app-name=Cairn", "Cairn", &msg]).status();
            }
            println!("{msg}");
            result.map(|_| ())
        }
        "pause" | "resume" | "rescan" => {
            let req = match cmd.as_str() {
                "pause" => Request::Pause,
                "resume" => Request::Resume,
                _ => Request::Rescan,
            };
            if let Response::Ok { message } = call(req, 10)? {
                println!("{message}");
            }
            Ok(())
        }
        "models" => models(rest),
        "doctor" => doctor(),
        "launch" => launch::run(rest),
        "help" | "-h" | "--help" => {
            println!("{USAGE}");
            Ok(())
        }
        other => bail!("unknown command {other:?}\n{USAGE}"),
    }
}

fn search(args: &[String]) -> Result<()> {
    let mut json = false;
    let mut here = false;
    let mut in_dir: Option<Vec<u8>> = None;
    let mut limit = 20usize;
    let mut words = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--json" => json = true,
            "--here" => here = true,
            "--in" => {
                i += 1;
                in_dir = Some(abs_path(args.get(i).context("--in needs a folder")?)?);
            }
            "--limit" => {
                i += 1;
                limit = args.get(i).context("--limit needs a number")?.parse()?;
            }
            w => words.push(w.to_string()),
        }
        i += 1;
    }
    if words.is_empty() {
        bail!("empty query");
    }
    let scope = here || in_dir.is_some();
    let cwd = match in_dir {
        Some(d) => Some(WirePath::from_bytes(&d)),
        None => std::env::current_dir().ok().map(|c| WirePath::from_bytes(&paths::to_bytes(&c))),
    };
    let Response::Search(r) = call(Request::Search { query: words.join(" "), cwd, limit, scope, kinds: Vec::new() }, 30)? else { bail!("unexpected response") };
    if json {
        println!("{}", serde_json::to_string_pretty(&r)?);
        return Ok(());
    }
    let mut out = std::io::stdout().lock();
    let p = &r.parsed;
    let mut understood = Vec::new();
    if !p.kinds.is_empty() {
        understood.push(p.kinds.iter().map(|k| k.as_str()).collect::<Vec<_>>().join("/"));
    }
    if let Some(d) = &p.date {
        understood.push(d.clone());
    }
    if !p.semantic.is_empty() {
        understood.push(format!("“{}”", p.semantic));
    }
    if let Some(sc) = &p.scope {
        understood.push(format!("only in {}", tilde(sc)));
    } else if p.scope_here || scope {
        understood.push("in this folder (not indexed)".into());
    }
    writeln!(out, "{} results in {} ms · {}{}", r.hits.len(), r.took_ms, understood.join(" · "), if r.partial { " · (index still building)" } else { "" })?;
    for (i, h) in r.hits.iter().enumerate() {
        let loc = match (h.line, h.line_end) {
            (Some(a), Some(b)) if b > a => format!("{}:{a}-{b}", tilde(&h.path.path)),
            (Some(a), _) => format!("{}:{a}", tilde(&h.path.path)),
            _ => tilde(&h.path.path),
        };
        writeln!(out, "{:>3}. {:<12} {}  {}", i + 1, h.category, loc, h.date.as_deref().unwrap_or(""))?;
        if !h.why.is_empty() {
            writeln!(out, "     {:<12} {}", "", h.why.join(" · "))?;
        }
        if let Some(s) = &h.snippet {
            writeln!(out, "     {:<12} │ {}", "", s)?;
        }
    }
    Ok(())
}

fn status(json: bool) -> Result<()> {
    let Response::Status(s) = call(Request::Status, 10)? else { bail!("unexpected response") };
    if json {
        println!("{}", serde_json::to_string_pretty(&s)?);
        return Ok(());
    }
    println!("roots:      {}", s.roots.join(", "));
    println!("indexed:    {} files, {} folders, {} text chunks", s.files, s.dirs, s.chunks);
    println!("vectors:    {} text, {} image", s.text_vectors, s.image_vectors);
    let pending: i64 = s.pending.iter().map(|p| p.1).sum();
    println!("pending:    {pending} {}", s.pending.iter().map(|(k, n)| format!("{k}={n}")).collect::<Vec<_>>().join(" "));
    println!("failed:     {} files (kept out after 3 tries; retried when they change)", s.failed);
    let state = if s.paused {
        "paused".to_string()
    } else if let Some(t) = &s.throttle {
        format!("waiting ({t})")
    } else if s.scanning {
        "scanning".into()
    } else if pending > 0 {
        "indexing".into()
    } else {
        "idle".into()
    };
    println!("state:      {state}");
    println!("models:     {}", s.models.iter().map(|(m, ok)| format!("{m} {}", if *ok { "ok" } else { "MISSING" })).collect::<Vec<_>>().join(", "));
    println!("sandbox:    {}", s.sandbox);
    println!("disk use:   {:.1} MB", s.db_bytes as f64 / 1e6);
    if !s.exclusions.is_empty() {
        println!("excluded:   {}", s.exclusions.iter().map(|e| tilde(e)).collect::<Vec<_>>().join(", "));
    }
    Ok(())
}

fn models(args: &[String]) -> Result<()> {
    let root = paths::models_dir()?;
    match args.first().map(String::as_str) {
        Some("list") => {
            for m in cairn_ml::ALL_MODELS {
                println!("{} ({}, license {})", m.id, m.repo, m.license);
                for f in m.files {
                    println!("  {}\n    sha256 {}  size {}", m.url(f.name), f.sha256, f.size);
                }
                println!("  -> place files in {}", cairn_ml::model_dir(&root, m).display());
            }
            Ok(())
        }
        Some("verify") | None => {
            let mut bad = false;
            for m in cairn_ml::ALL_MODELS {
                match cairn_ml::verify(&root, m) {
                    Ok(()) => println!("{}: ok", m.id),
                    Err(e) => {
                        bad = true;
                        println!("{}: {e:#}", m.id);
                    }
                }
            }
            if bad {
                bail!("some models are missing or corrupt");
            }
            Ok(())
        }
        Some("fetch") => {
            for m in cairn_ml::ALL_MODELS {
                let dir = cairn_ml::model_dir(&root, m);
                paths::ensure_private_dir(&dir)?;
                for f in m.files {
                    let dest = dir.join(f.name);
                    if dest.exists() && cairn_ml::sha256_file(&dest)? == f.sha256 {
                        continue;
                    }
                    let part = dir.join(format!("{}.part", f.name));
                    println!("fetching {} / {} ({:.0} MB)", m.id, f.name, f.size as f64 / 1e6);
                    let st = std::process::Command::new("curl")
                        .args(["-fL", "--retry", "3", "--proto", "=https", "--tlsv1.2", "-o"])
                        .arg(&part)
                        .arg(m.url(f.name))
                        .status()
                        .context("running curl")?;
                    if !st.success() {
                        bail!("download failed: {}", m.url(f.name));
                    }
                    let h = cairn_ml::sha256_file(&part)?;
                    if h != f.sha256 {
                        let _ = std::fs::remove_file(&part);
                        bail!("{}: sha256 mismatch (got {h}); refusing to install", f.name);
                    }
                    std::fs::rename(&part, &dest)?;
                }
                cairn_ml::verify(&root, m)?;
                println!("{}: ok", m.id);
            }
            println!("restart cairnd (systemctl --user restart cairnd) to load new models");
            Ok(())
        }
        Some("install") => {
            let src = PathBuf::from(args.get(1).context("usage: cairn models install <dir>")?);
            for m in cairn_ml::ALL_MODELS {
                let dir = cairn_ml::model_dir(&root, m);
                paths::ensure_private_dir(&dir)?;
                for f in m.files {
                    // Accept either <src>/<model-id>/<file> or <src>/<file>.
                    let cand = [src.join(m.id).join(f.name), src.join(f.name)];
                    let Some(from) = cand.iter().find(|p| p.exists()) else { bail!("{} not found under {}", f.name, src.display()) };
                    let h = cairn_ml::sha256_file(from)?;
                    if h != f.sha256 {
                        bail!("{}: sha256 mismatch (got {h})", from.display());
                    }
                    let part = dir.join(format!("{}.part", f.name));
                    std::fs::copy(from, &part)?;
                    std::fs::rename(&part, dir.join(f.name))?;
                }
                cairn_ml::verify(&root, m)?;
                println!("{}: installed", m.id);
            }
            Ok(())
        }
        Some(o) => bail!("unknown models command {o:?}"),
    }
}

fn doctor() -> Result<()> {
    let mut ok = true;
    let mut check = |name: &str, good: bool, detail: String| {
        println!("[{}] {name}: {detail}", if good { " ok " } else { "FAIL" });
        ok &= good;
    };
    let abi = cairn_core::sandbox::landlock_abi();
    check("landlock", abi > 0, if abi > 0 { format!("ABI v{abi}") } else { "unavailable: workers refuse to run unsandboxed".into() });
    let watches = std::fs::read_to_string("/proc/sys/fs/inotify/max_user_watches").unwrap_or_default();
    check("inotify watches", true, format!("{} (Cairn uses at most half)", watches.trim()));
    let pdf = std::process::Command::new("pdftotext").arg("-v").output().is_ok();
    check("pdftotext", pdf, if pdf { "found".into() } else { "missing: install poppler-utils for PDF text".into() });
    for m in cairn_ml::ALL_MODELS {
        let r = cairn_ml::verify(&paths::models_dir()?, m);
        check(&format!("model {}", m.id), r.is_ok(), r.map(|_| "ok".to_string()).unwrap_or_else(|e| format!("{e:#}")));
    }
    let d = call(Request::Status, 5);
    check("daemon", d.is_ok(), d.map(|_| "running".to_string()).unwrap_or_else(|e| format!("{e:#}")));
    let psi = cairn_core::sandbox::psi_avg10("cpu").is_some();
    check("pressure stall info", psi, if psi { "available".into() } else { "unavailable: throttling uses memory checks only".into() });
    if !ok {
        bail!("some checks failed");
    }
    Ok(())
}
