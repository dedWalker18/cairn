//! Index page ("garden"): what Cairn knows, what it is learning, and the
//! guarantees it runs under. Refreshes every few seconds.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use cairn_core::ipc::{Request, Status};
use gtk4::prelude::*;

use crate::util::{self, label, markup};
use crate::Ctx;

const STAGES: [(&str, &str, &str); 5] = [
    ("ExtractText", "📖  Reading documents and code", "pulling text out of files"),
    ("ImageMeta", "🗓️  Reading photo dates", "when and where each photo was taken"),
    ("EmbedText", "🧠  Understanding text", "turning text into meaning"),
    ("FolderCtx", "📁  Learning folder context", "folder names and notes"),
    ("ImageEmbed", "👀  Looking at photos", "what's in each picture"),
];

/// A small rounded progress bar drawn with cairo: system themes style
/// GtkProgressBar in ways a stylesheet cannot fully undo.
struct Bar {
    area: gtk4::DrawingArea,
    state: Rc<std::cell::Cell<(f64, bool)>>,
}

impl Bar {
    fn new() -> Bar {
        let area = gtk4::DrawingArea::new();
        area.set_content_height(8);
        area.set_hexpand(true);
        let state = Rc::new(std::cell::Cell::new((0.0, false)));
        let st = state.clone();
        area.set_draw_func(move |_, cr, w, h| {
            let (frac, done) = st.get();
            let (w, h) = (w as f64, h as f64);
            let r = h / 2.0;
            let pill = |cr: &gtk4::cairo::Context, width: f64| {
                cr.new_sub_path();
                cr.arc(r, r, r, std::f64::consts::FRAC_PI_2, 3.0 * std::f64::consts::FRAC_PI_2);
                cr.arc((width - r).max(r), r, r, -std::f64::consts::FRAC_PI_2, std::f64::consts::FRAC_PI_2);
                cr.close_path();
            };
            cr.set_source_rgb(0.957, 0.918, 0.839); // sand
            pill(cr, w);
            let _ = cr.fill();
            let fw = (w * frac).max(if frac > 0.0 { h } else { 0.0 });
            if fw > 0.0 {
                if done {
                    cr.set_source_rgb(0.498, 0.682, 0.533); // sage
                } else {
                    cr.set_source_rgb(0.949, 0.710, 0.267); // honey
                }
                pill(cr, fw);
                let _ = cr.fill();
            }
        });
        Bar { area, state }
    }

    fn set(&self, frac: f64, done: bool) {
        self.state.set((if done { 1.0 } else { frac }, done));
        self.area.queue_draw();
    }
}

fn stat_card(value: &gtk4::Label, caption: &str) -> gtk4::Box {
    let b = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
    b.add_css_class("stat");
    b.set_hexpand(true);
    b.append(value);
    b.append(&label(caption, &["stat-label"]));
    b
}

pub fn build(ctx: &Rc<Ctx>) -> gtk4::Widget {
    let page = gtk4::Box::new(gtk4::Orientation::Vertical, 16);
    page.add_css_class("page");
    page.append(&label("Your index", &["hero"]));
    let headline = label("Checking on Cairn…", &["hero-sub"]);
    headline.set_wrap(true);
    page.append(&headline);

    let nums: Vec<gtk4::Label> = (0..4).map(|_| label("—", &["stat-num"])).collect();
    let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
    for (n, cap) in nums.iter().zip(["files", "folders", "pieces of text understood", "photos understood"]) {
        row.append(&stat_card(n, cap));
    }
    page.append(&row);

    // Stage progress. Totals are not known up front, so bars show progress
    // since this window opened (pending now vs the most pending seen).
    page.append(&label("What Cairn is doing", &["h2"]));
    let stages_box = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
    let mut bars = Vec::new();
    for (key, title, sub) in STAGES {
        let r = gtk4::Box::new(gtk4::Orientation::Vertical, 5);
        r.add_css_class("row-card");
        let top = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
        let t = label(title, &["stage-name"]);
        t.set_hexpand(true);
        top.append(&t);
        let left = label("", &["dim", "small"]);
        top.append(&left);
        r.append(&top);
        r.append(&label(sub, &["dim", "small"]));
        let bar = Bar::new();
        r.append(&bar.area);
        stages_box.append(&r);
        bars.push((key, left, bar));
    }
    page.append(&stages_box);

    // Controls.
    let controls = gtk4::Box::new(gtk4::Orientation::Horizontal, 10);
    let pause = util::pill_button("Pause learning", &["soft"]);
    let rescan = util::pill_button("Look for changes now", &["soft"]);
    controls.append(&pause);
    controls.append(&rescan);
    page.append(&controls);

    // Promises.
    let safety = markup("", &["note"]);
    safety.set_wrap(true);
    page.append(&safety);
    let throttle = markup("", &["note", "warn-note"]);
    throttle.set_wrap(true);
    throttle.set_visible(false);
    page.append(&throttle);
    let footer = label("", &["dim", "small"]);
    footer.set_wrap(true);
    page.append(&footer);

    let paused = Rc::new(std::cell::Cell::new(false));
    {
        let (ctx, paused) = (ctx.clone(), paused.clone());
        pause.connect_clicked(move |_| {
            let req = if paused.get() { Request::Resume } else { Request::Pause };
            let ctx2 = ctx.clone();
            util::call_async(req, 5, move |_| ctx2.refresh_status());
        });
    }
    {
        let ctx = ctx.clone();
        rescan.connect_clicked(move |b| {
            b.set_label("Looking… ✓");
            let ctx2 = ctx.clone();
            util::call_async(Request::Rescan, 5, move |_| ctx2.refresh_status());
        });
    }

    let max_seen: Rc<RefCell<HashMap<String, i64>>> = Rc::default();
    let scroller = gtk4::ScrolledWindow::new();
    scroller.set_hscrollbar_policy(gtk4::PolicyType::Never);
    scroller.set_child(Some(&page));

    ctx.on_status(move |st: Option<&Status>| {
        let Some(s) = st else {
            headline.set_text("Cairn's background service isn't running. Start it with  systemctl --user start cairnd");
            return;
        };
        paused.set(s.paused);
        pause.set_label(if s.paused { "Resume learning" } else { "Pause learning" });
        let vals = [s.files, s.dirs, s.text_vectors, s.image_vectors];
        for (n, v) in nums.iter().zip(vals) {
            n.set_text(&util::human_count(v));
        }
        let left: i64 = s.pending.iter().map(|p| p.1).sum();
        headline.set_text(&if s.paused {
            "Paused. Search still works with everything learned so far.".to_string()
        } else if left == 0 && !s.scanning {
            "All caught up. Cairn keeps watching quietly for changes.".to_string()
        } else {
            format!("Learning in the background, only when your computer is idle. {} steps to go.", util::human_count(left))
        });
        let mut ms = max_seen.borrow_mut();
        for (key, left_l, bar) in &bars {
            let n = s.pending.iter().find(|p| p.0 == *key).map(|p| p.1).unwrap_or(0);
            let m = ms.entry(key.to_string()).or_insert(n);
            if n > *m {
                *m = n;
            }
            let frac = if *m == 0 { 1.0 } else { 1.0 - n as f64 / *m as f64 };
            bar.set(frac.clamp(0.0, 1.0), n == 0);
            if n == 0 {
                left_l.set_text("done ✓");
            } else {
                left_l.set_text(&format!("{} left", util::human_count(n)));
            }
        }
        safety.set_markup(&format!(
            "🔒  <b>Your files are read-only to Cairn.</b> The kernel enforces it ({}).\n🌐  No network: nothing leaves this computer.\n🪶  Learning runs at idle priority and pauses on battery or when you're busy.",
            util::esc(&std::env::var("HOME").map(|h| if h.is_empty() { s.sandbox.clone() } else { s.sandbox.replace(&h, "~") }).unwrap_or_else(|_| s.sandbox.clone()))
        ));
        match &s.throttle {
            Some(t) => {
                throttle.set_markup(&format!("☕  Resting right now: <b>{}</b>. It continues by itself.", util::esc(t)));
                throttle.set_visible(true);
            }
            None => throttle.set_visible(false),
        }
        let models = s.models.iter().map(|(m, ok)| format!("{m} {}", if *ok { "✓" } else { "missing" })).collect::<Vec<_>>().join("  ·  ");
        footer.set_text(&format!(
            "Index size {:.1} GB  ·  {} files could not be read  ·  models: {}",
            s.db_bytes as f64 / 1e9,
            s.failed,
            models
        ));
    });
    scroller.upcast()
}
