//! Shared helpers: daemon calls off the UI thread, opening files,
//! thumbnails, small widget builders.

use std::path::{Path, PathBuf};
use std::time::Duration;

use cairn_core::ipc::{self, Hit, Request, Response};
use cairn_core::paths;
use gtk4::prelude::*;
use gtk4::{gdk, gio, glib};

/// Call the daemon on a worker thread and hand the result to `done` on the
/// UI thread.
pub fn call_async<F: FnOnce(Result<Response, String>) + 'static>(req: Request, timeout_s: u64, done: F) {
    glib::spawn_future_local(async move {
        let r = gio::spawn_blocking(move || ipc::call(&req, Duration::from_secs(timeout_s)).map_err(|e| format!("{e:#}"))).await;
        done(r.unwrap_or_else(|_| Err("internal error".into())));
    });
}

pub fn tilde(p: &str) -> String {
    match std::env::var("HOME") {
        Ok(h) if !h.is_empty() && p.starts_with(&h) => format!("~{}", &p[h.len()..]),
        _ => p.to_string(),
    }
}

fn which(cmd: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|p| std::env::split_paths(&p).map(|d| d.join(cmd)).find(|c| c.is_file()))
}

pub fn hit_path(hit: &Hit) -> PathBuf {
    paths::from_bytes(&hit.path.bytes())
}

/// Code/text at its line in Zed ($CAIRN_EDITOR), everything else with the
/// default application.
pub fn open_hit(hit: &Hit) {
    let path = hit_path(hit);
    if matches!(hit.category.as_str(), "code" | "text") && hit.line.is_some() {
        let editor = std::env::var("CAIRN_EDITOR").ok().map(PathBuf::from).or_else(|| which("zed"));
        if let Some(ed) = editor {
            let mut target = path.clone().into_os_string();
            target.push(format!(":{}", hit.line.unwrap_or(1)));
            if std::process::Command::new(ed).arg(target).spawn().is_ok() {
                return;
            }
        }
    }
    open_path(&path);
}

pub fn open_path(path: &Path) {
    let file = gio::File::for_path(path);
    let _ = gio::AppInfo::launch_default_for_uri(&file.uri(), None::<&gio::AppLaunchContext>);
}

pub fn reveal(path: &Path) {
    let _ = std::process::Command::new("nautilus").arg("--select").arg(path).spawn();
}

pub fn copy_text(widget: &impl IsA<gtk4::Widget>, text: &str) {
    widget.as_ref().clipboard().set_text(text);
}

pub fn kind_emoji(category: &str, is_dir: bool) -> &'static str {
    if is_dir {
        return "📁";
    }
    match category {
        "image" => "🖼️",
        "video" => "🎞️",
        "audio" => "🎵",
        "pdf" => "📕",
        "document" => "📄",
        "presentation" => "📊",
        "spreadsheet" => "📈",
        "code" => "⌨️",
        "text" => "📝",
        "archive" => "📦",
        "cad" => "📐",
        "design" => "🎨",
        _ => "📎",
    }
}

/// A thumbnail texture: the freedesktop cache if Nautilus made one, else a
/// downscaled decode of the image (bounded size, off the UI thread).
pub fn load_thumbnail<F: FnOnce(Option<gdk::Texture>) + 'static>(path: PathBuf, size: i32, done: F) {
    // Decode on a worker thread; only plain bytes cross back (Pixbuf is not
    // Send). Image files are bounded to 60 MB.
    glib::spawn_future_local(async move {
        let raw = gio::spawn_blocking(move || -> Option<(i32, i32, usize, bool, Vec<u8>)> {
            let f = gio::File::for_path(&path);
            let decode = |p: &Path| gtk4::gdk_pixbuf::Pixbuf::from_file_at_scale(p, size, size, true).ok();
            let mut pb = None;
            if let Ok(info) = f.query_info("thumbnail::path,standard::size", gio::FileQueryInfoFlags::NONE, gio::Cancellable::NONE) {
                if let Some(t) = info.attribute_byte_string("thumbnail::path") {
                    pb = decode(Path::new(t.as_str()));
                }
                if pb.is_none() && info.size() > 60 << 20 {
                    return None;
                }
            }
            let pb = pb.or_else(|| decode(&path))?;
            let bytes = pb.read_pixel_bytes().to_vec();
            Some((pb.width(), pb.height(), pb.rowstride() as usize, pb.has_alpha(), bytes))
        })
        .await
        .ok()
        .flatten();
        let tex = raw.map(|(w, h, stride, alpha, bytes)| {
            let fmt = if alpha { gdk::MemoryFormat::R8g8b8a8 } else { gdk::MemoryFormat::R8g8b8 };
            gdk::MemoryTexture::new(w, h, fmt, &glib::Bytes::from_owned(bytes), stride).upcast::<gdk::Texture>()
        });
        done(tex);
    });
}

pub fn label(text: &str, classes: &[&str]) -> gtk4::Label {
    let l = gtk4::Label::new(Some(text));
    l.set_xalign(0.0);
    for c in classes {
        l.add_css_class(c);
    }
    l
}

pub fn markup(m: &str, classes: &[&str]) -> gtk4::Label {
    let l = gtk4::Label::new(None);
    l.set_markup(m);
    l.set_xalign(0.0);
    for c in classes {
        l.add_css_class(c);
    }
    l
}

pub fn esc(s: &str) -> String {
    glib::markup_escape_text(s).to_string()
}

pub fn pill_button(text: &str, classes: &[&str]) -> gtk4::Button {
    let b = gtk4::Button::with_label(text);
    for c in classes {
        b.add_css_class(c);
    }
    b
}

pub fn human_count(n: i64) -> String {
    match n {
        n if n >= 1_000_000 => format!("{:.1}M", n as f64 / 1e6),
        n if n >= 10_000 => format!("{}k", n / 1000),
        n if n >= 1000 => format!("{:.1}k", n as f64 / 1e3),
        n => n.to_string(),
    }
}

pub fn clear(container: &gtk4::Box) {
    while let Some(c) = container.first_child() {
        container.remove(&c);
    }
}
