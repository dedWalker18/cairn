//! `cairn launch`: a drop-in replacement for the fuzzel application launcher.
//! It shows installed applications as usual; text typed with a leading "/"
//! becomes a Cairn search, and the results open in a second fuzzel list.
//! fuzzel has no plugin API, so this drives `fuzzel --dmenu` instead.

use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use cairn_core::ipc::{self, Request, Response, WirePath};
use cairn_core::paths;

struct App {
    name: String,
    icon: Option<String>,
    file: PathBuf,
}

fn data_dirs() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Ok(h) = paths::home_dir() {
        v.push(std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).unwrap_or_else(|| h.join(".local/share")));
    }
    let sys = std::env::var("XDG_DATA_DIRS").unwrap_or_else(|_| "/usr/local/share:/usr/share".into());
    v.extend(sys.split(':').filter(|s| !s.is_empty()).map(PathBuf::from));
    v
}

fn parse_desktop(p: &Path) -> Option<App> {
    let s = std::fs::read_to_string(p).ok()?;
    let mut in_entry = false;
    let (mut name, mut icon, mut is_app) = (None, None, false);
    for line in s.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_entry = line == "[Desktop Entry]";
            continue;
        }
        if !in_entry {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else { continue };
        match k.trim() {
            "Name" => name = Some(v.trim().to_string()),
            "Icon" => icon = Some(v.trim().to_string()),
            "Type" => is_app = v.trim() == "Application",
            "NoDisplay" | "Hidden" if v.trim() == "true" => return None,
            _ => {}
        }
    }
    if !is_app {
        return None;
    }
    Some(App { name: name?, icon, file: p.to_path_buf() })
}

fn apps() -> Vec<App> {
    let mut seen_ids = HashSet::new();
    let mut out = Vec::new();
    for d in data_dirs() {
        let dir = d.join("applications");
        let mut stack = vec![dir.clone()];
        while let Some(cur) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&cur) else { continue };
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.extension().is_some_and(|x| x == "desktop") {
                    // Desktop-file ID: path below applications/ with '/' -> '-'.
                    let id = p.strip_prefix(&dir).map(|r| r.to_string_lossy().replace('/', "-")).unwrap_or_default();
                    if seen_ids.insert(id) {
                        if let Some(a) = parse_desktop(&p) {
                            out.push(a);
                        }
                    }
                }
            }
        }
    }
    out.sort_by_key(|a| a.name.to_lowercase());
    out
}

/// Run fuzzel in dmenu mode. Returns the chosen line, or the typed text when
/// nothing matched; None when cancelled.
fn fuzzel(lines: &[(String, Option<String>)], prompt: &str, extra: &[&str]) -> Result<Option<String>> {
    let mut child = Command::new("fuzzel")
        .args(["--dmenu", "--prompt", prompt])
        .args(extra)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .context("running fuzzel")?;
    {
        let mut stdin = child.stdin.take().context("stdin")?;
        let mut buf = Vec::new();
        for (text, icon) in lines {
            buf.extend_from_slice(text.replace('\n', " ").as_bytes());
            if let Some(i) = icon {
                buf.extend_from_slice(b"\0icon\x1f");
                buf.extend_from_slice(i.as_bytes());
            }
            buf.push(b'\n');
        }
        let _ = stdin.write_all(&buf);
    }
    let out = child.wait_with_output()?;
    if !out.status.success() {
        return Ok(None);
    }
    let s = String::from_utf8_lossy(&out.stdout).trim_end_matches('\n').to_string();
    Ok(if s.is_empty() { None } else { Some(s) })
}

fn icon_for(category: &str, is_dir: bool) -> &'static str {
    if is_dir {
        return "folder";
    }
    match category {
        "image" => "image-x-generic",
        "video" => "video-x-generic",
        "audio" => "audio-x-generic",
        "pdf" | "document" => "x-office-document",
        "presentation" => "x-office-presentation",
        "spreadsheet" => "x-office-spreadsheet",
        "code" => "text-x-script",
        _ => "text-x-generic",
    }
}

fn tilde(p: &str) -> String {
    match std::env::var("HOME") {
        Ok(h) if !h.is_empty() && p.starts_with(&h) => format!("~{}", &p[h.len()..]),
        _ => p.to_string(),
    }
}

fn search_and_open(query: &str) -> Result<()> {
    let cwd = paths::home_dir().ok().map(|h| WirePath::from_bytes(&paths::to_bytes(&h)));
    let resp = ipc::call(&Request::Search { query: query.to_string(), cwd, limit: 40, scope: false, kinds: Vec::new() }, Duration::from_secs(20));
    let r = match resp {
        Ok(Response::Search(r)) => r,
        Ok(Response::Error { message }) => bail!("{message}"),
        Ok(_) => bail!("unexpected response"),
        Err(e) => {
            let _ = fuzzel(&[(format!("Cairn is not running: {e:#}"), Some("dialog-error".into()))], "cairn ", &[]);
            return Ok(());
        }
    };
    if r.hits.is_empty() {
        let msg = if r.partial {
            format!("No results for “{query}” yet. Cairn is still indexing (see `cairn status`)")
        } else {
            format!("No results for “{query}”")
        };
        let _ = fuzzel(&[(msg, Some("edit-find".into()))], "cairn ", &["--width", "80"]);
        return Ok(());
    }
    let lines: Vec<(String, Option<String>)> = r
        .hits
        .iter()
        .map(|h| {
            let loc = match h.line {
                Some(l) => format!("{}:{l}", tilde(&h.path.path)),
                None => tilde(&h.path.path),
            };
            let why = h.why.first().map(|w| format!("   · {w}")).unwrap_or_default();
            (format!("{loc}{why}"), Some(icon_for(&h.category, h.is_dir).to_string()))
        })
        .collect();
    let title = match &r.parsed.scope {
        Some(sc) => format!("{query} · in {} › ", tilde(sc)),
        None => format!("{query} › "),
    };
    let Some(choice) = fuzzel(&lines, &title, &["--index", "--width", "90"])? else { return Ok(()) };
    let Ok(i) = choice.trim().parse::<usize>() else { return Ok(()) };
    let Some(h) = r.hits.get(i) else { return Ok(()) };
    let path = paths::from_bytes(&h.path.bytes());
    if matches!(h.category.as_str(), "code" | "text") && h.line.is_some() {
        let editor = std::env::var("CAIRN_EDITOR").ok().unwrap_or_else(|| "zed".into());
        let mut target = path.clone().into_os_string();
        target.push(format!(":{}", h.line.unwrap_or(1)));
        if Command::new(&editor).arg(target).spawn().is_ok() {
            return Ok(());
        }
    }
    Command::new("xdg-open").arg(&path).spawn().context("xdg-open")?;
    Ok(())
}

pub fn run(args: &[String]) -> Result<()> {
    // `cairn launch --find`: file search only (bound to Win+/). An empty
    // fuzzel list with a prompt; whatever is typed becomes the query.
    if args.first().map(String::as_str) == Some("--find") {
        let Some(q) = fuzzel(&[], "find files › ", &["--width", "60"])? else { return Ok(()) };
        let q = q.trim().trim_start_matches('/').trim().to_string();
        if q.is_empty() {
            return Ok(());
        }
        return search_and_open(&q);
    }
    // `cairn launch /query` searches directly (bindable to its own key).
    if let Some(q) = args.first().and_then(|a| a.strip_prefix('/')) {
        let q = std::iter::once(q.to_string()).chain(args[1..].iter().cloned()).collect::<Vec<_>>().join(" ");
        return search_and_open(q.trim());
    }
    let apps = apps();
    let lines: Vec<(String, Option<String>)> = apps.iter().map(|a| (a.name.clone(), a.icon.clone())).collect();
    let Some(choice) = fuzzel(&lines, "apps · /search › ", &[])? else { return Ok(()) };
    if let Some(q) = choice.strip_prefix('/') {
        if !q.trim().is_empty() {
            return search_and_open(q.trim());
        }
        return Ok(());
    }
    match apps.iter().find(|a| a.name == choice) {
        Some(app) => {
            Command::new("gio").arg("launch").arg(&app.file).spawn().context("gio launch")?;
        }
        // Not an app name: treat the typed text as a file search.
        None => return search_and_open(choice.trim()),
    }
    Ok(())
}
