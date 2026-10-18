//! Search page: one big friendly field, kind chips, folder scope, results
//! as cards (documents, code) or a photo grid (images).

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use cairn_core::ipc::{Hit, Request, Response, SearchResult, WirePath};
use cairn_core::paths;
use gtk4::prelude::*;
use gtk4::{gdk, gio, glib, pango};

use crate::util::{self, esc, label, markup};
use crate::{Args, Ctx};

const KINDS: [(&str, &[&str]); 7] = [
    ("All", &[]),
    ("Photos", &["image"]),
    ("Documents", &["pdf", "document", "text"]),
    ("Slides", &["presentation"]),
    ("Sheets", &["spreadsheet"]),
    ("Code", &["code"]),
    ("Folders", &["dir"]),
];

const SUGGESTIONS: [&str; 6] = [
    "photos from last december",
    "the script that runs terraform on aws",
    "sales report for last quarter",
    "pdfs in downloads",
    "notes from this week",
    "screenshots yesterday",
];

struct State {
    ctx: Rc<Ctx>,
    entry: gtk4::Entry,
    kinds: RefCell<Vec<String>>,
    scope: RefCell<Option<PathBuf>>,
    scope_btn: gtk4::Button,
    understood: gtk4::Label,
    results: gtk4::Box,
    hits: RefCell<Vec<Hit>>,
    selected: Cell<Option<usize>>,
    cards: RefCell<Vec<gtk4::Widget>>,
    generation: Cell<u64>,
    suggestions: gtk4::Box,
}

pub fn build(ctx: &Rc<Ctx>, args: &Args) -> gtk4::Widget {
    let page = gtk4::Box::new(gtk4::Orientation::Vertical, 14);
    page.add_css_class("page");

    page.append(&label("What are you looking for?", &["hero"]));
    page.append(&label("Describe it the way you'd tell a friend: what it is, roughly when, maybe where. Cairn reads folder names and notes too.", &["hero-sub"]));

    // Search field.
    let shell = gtk4::Box::new(gtk4::Orientation::Horizontal, 10);
    shell.add_css_class("search-shell");
    shell.set_margin_top(8);
    let glyph = label("🔍", &["search-glyph"]);
    glyph.set_valign(gtk4::Align::Center);
    shell.append(&glyph);
    let entry = gtk4::Entry::new();
    entry.set_hexpand(true);
    entry.set_placeholder_text(Some("photos of mountains from last december…"));
    entry.set_text(&args.query);
    shell.append(&entry);
    let scope_btn = util::pill_button("📍 Anywhere", &["chip"]);
    scope_btn.set_valign(gtk4::Align::Center);
    scope_btn.set_tooltip_text(Some("Only search inside a folder"));
    shell.append(&scope_btn);
    page.append(&shell);

    // Kind chips.
    let chips = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    let mut first: Option<gtk4::ToggleButton> = None;
    let chip_buttons: Vec<(gtk4::ToggleButton, Vec<String>)> = KINDS
        .iter()
        .map(|(name, kinds)| {
            let b = gtk4::ToggleButton::with_label(name);
            b.add_css_class("chip");
            match &first {
                Some(f) => b.set_group(Some(f)),
                None => {
                    b.set_active(true);
                    first = Some(b.clone());
                }
            }
            chips.append(&b);
            (b, kinds.iter().map(|k| k.to_string()).collect())
        })
        .collect();
    page.append(&chips);

    let understood = label("", &["understood"]);
    understood.set_visible(false);
    understood.set_wrap(true);
    page.append(&understood);

    // Suggestions (empty state).
    let suggestions = gtk4::Box::new(gtk4::Orientation::Vertical, 10);
    suggestions.set_margin_top(10);
    suggestions.append(&label("Try one of these", &["h2"]));
    let flow = gtk4::FlowBox::new();
    flow.set_selection_mode(gtk4::SelectionMode::None);
    flow.set_max_children_per_line(3);
    flow.set_column_spacing(8);
    flow.set_row_spacing(8);
    for s in SUGGESTIONS {
        let b = util::pill_button(s, &["chip", "suggest"]);
        let e = entry.clone();
        b.connect_clicked(move |_| {
            e.set_text(s);
            e.grab_focus();
            e.set_position(-1);
        });
        flow.insert(&b, -1);
    }
    suggestions.append(&flow);
    let tip = markup(
        "<b>Give your files context.</b>  Rename a photo dump to “<i>Manali trek Dec 25</i>”, or drop a <tt>notes.txt</tt> in a project folder. Everything inside inherits the meaning.",
        &["note"],
    );
    tip.set_wrap(true);
    tip.set_margin_top(12);
    suggestions.append(&tip);

    let results = gtk4::Box::new(gtk4::Orientation::Vertical, 10);
    let scroller = gtk4::ScrolledWindow::new();
    scroller.set_vexpand(true);
    scroller.set_hscrollbar_policy(gtk4::PolicyType::Never);
    let inner = gtk4::Box::new(gtk4::Orientation::Vertical, 10);
    inner.append(&suggestions);
    inner.append(&results);
    scroller.set_child(Some(&inner));
    page.append(&scroller);

    let st = Rc::new(State {
        ctx: ctx.clone(),
        entry: entry.clone(),
        kinds: RefCell::new(Vec::new()),
        scope: RefCell::new(args.scope.clone()),
        scope_btn: scope_btn.clone(),
        understood,
        results,
        hits: RefCell::new(Vec::new()),
        selected: Cell::new(None),
        cards: RefCell::new(Vec::new()),
        generation: Cell::new(0),
        suggestions,
    });
    update_scope_button(&st);

    for (b, kinds) in chip_buttons {
        let st = st.clone();
        b.connect_toggled(move |b| {
            if b.is_active() {
                *st.kinds.borrow_mut() = kinds.clone();
                run_search(&st);
            }
        });
    }
    {
        let st = st.clone();
        entry.connect_changed(move |_| {
            // Debounce: search 160 ms after the last keystroke.
            let g = st.generation.get() + 1;
            st.generation.set(g);
            let st2 = st.clone();
            glib::timeout_add_local_once(Duration::from_millis(160), move || {
                if st2.generation.get() == g {
                    run_search(&st2);
                }
            });
        });
    }
    {
        let st = st.clone();
        entry.connect_activate(move |_| open_selected(&st));
    }
    {
        let st = st.clone();
        scope_btn.connect_clicked(move |_| {
            if st.scope.borrow().is_some() {
                *st.scope.borrow_mut() = None;
                update_scope_button(&st);
                run_search(&st);
                return;
            }
            let dialog = gtk4::FileDialog::builder().title("Search only inside…").modal(true).build();
            let st2 = st.clone();
            dialog.select_folder(Some(&st.ctx.window), gio::Cancellable::NONE, move |res| {
                if let Ok(f) = res {
                    if let Some(p) = f.path() {
                        *st2.scope.borrow_mut() = Some(p);
                        update_scope_button(&st2);
                        run_search(&st2);
                    }
                }
            });
        });
    }
    // Keyboard: arrows move, Enter opens, Ctrl+Enter reveals, Ctrl+C copies,
    // Ctrl+K or "/" focuses the field, Esc clears.
    {
        let st = st.clone();
        let keys = gtk4::EventControllerKey::new();
        keys.set_propagation_phase(gtk4::PropagationPhase::Capture);
        keys.connect_key_pressed(move |_, key, _, mods| {
            let ctrl = mods.contains(gdk::ModifierType::CONTROL_MASK);
            match key {
                gdk::Key::Down => move_sel(&st, 1),
                gdk::Key::Up => move_sel(&st, -1),
                gdk::Key::Return | gdk::Key::KP_Enter if ctrl => {
                    if let Some(h) = selected_hit(&st) {
                        util::reveal(&util::hit_path(&h));
                    }
                }
                gdk::Key::Return | gdk::Key::KP_Enter => open_selected(&st),
                gdk::Key::k if ctrl => {
                    st.entry.grab_focus();
                }
                gdk::Key::c if ctrl && st.entry.selection_bounds().is_none() => {
                    if let Some(h) = selected_hit(&st) {
                        util::copy_text(&st.entry, &h.path.path);
                    }
                }
                gdk::Key::Escape => st.entry.set_text(""),
                _ => return glib::Propagation::Proceed,
            }
            glib::Propagation::Stop
        });
        page.add_controller(keys);
    }
    {
        let e = entry.clone();
        page.connect_map(move |_| {
            e.grab_focus();
        });
    }
    if !args.query.is_empty() {
        run_search(&st);
    }
    page.upcast()
}

fn update_scope_button(st: &State) {
    match st.scope.borrow().as_ref() {
        Some(p) => {
            let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| p.display().to_string());
            st.scope_btn.set_label(&format!("📍 {name}  ✕"));
            st.scope_btn.add_css_class("on");
            st.scope_btn.set_tooltip_text(Some(&format!("Only in {} (click to search everywhere)", util::tilde(&p.to_string_lossy()))));
        }
        None => {
            st.scope_btn.set_label("📍 Anywhere");
            st.scope_btn.remove_css_class("on");
            st.scope_btn.set_tooltip_text(Some("Only search inside a folder"));
        }
    }
}

fn selected_hit(st: &State) -> Option<Hit> {
    st.selected.get().and_then(|i| st.hits.borrow().get(i).cloned())
}

fn open_selected(st: &State) {
    let h = selected_hit(st).or_else(|| st.hits.borrow().first().cloned());
    if let Some(h) = h {
        util::open_hit(&h);
    }
}

fn set_selected(st: &State, idx: Option<usize>) {
    let cards = st.cards.borrow();
    if let Some(old) = st.selected.get().and_then(|i| cards.get(i)) {
        old.remove_css_class("selected");
    }
    st.selected.set(idx);
    if let Some(w) = idx.and_then(|i| cards.get(i)) {
        w.add_css_class("selected");
        // Keep the selection visible inside the scroller.
        if let Some(parent) = w.ancestor(gtk4::ScrolledWindow::static_type()) {
            if let Ok(sw) = parent.downcast::<gtk4::ScrolledWindow>() {
                if let Some(p) = w.compute_point(&sw, &gtk4::graphene::Point::new(0.0, 0.0)) {
                    let adj = sw.vadjustment();
                    let y = p.y() as f64 + adj.value();
                    let h = w.height() as f64;
                    if y < adj.value() {
                        adj.set_value(y - 8.0);
                    } else if y + h > adj.value() + adj.page_size() {
                        adj.set_value(y + h - adj.page_size() + 8.0);
                    }
                }
            }
        }
    }
}

fn move_sel(st: &State, delta: i32) {
    let n = st.cards.borrow().len() as i32;
    if n == 0 {
        return;
    }
    let cur = st.selected.get().map(|i| i as i32).unwrap_or(-1);
    let next = (cur + delta).clamp(0, n - 1);
    set_selected(st, Some(next as usize));
}

fn run_search(st: &Rc<State>) {
    let text = st.entry.text().to_string();
    let g = st.generation.get() + 1;
    st.generation.set(g);
    if text.trim().is_empty() {
        util::clear(&st.results);
        st.cards.borrow_mut().clear();
        st.hits.borrow_mut().clear();
        st.selected.set(None);
        st.understood.set_visible(false);
        st.suggestions.set_visible(true);
        return;
    }
    st.suggestions.set_visible(false);
    let scope = st.scope.borrow().clone();
    let req = Request::Search {
        query: text,
        cwd: scope.as_ref().map(|p| WirePath::from_bytes(&paths::to_bytes(p))),
        limit: 80,
        scope: scope.is_some(),
        kinds: st.kinds.borrow().clone(),
    };
    let st2 = st.clone();
    util::call_async(req, 20, move |r| {
        if st2.generation.get() != g {
            return; // a newer search is in flight
        }
        match r {
            Ok(Response::Search(res)) => show_results(&st2, res),
            Ok(Response::Error { message }) => show_message(&st2, "😕", "Something went wrong", &message),
            Ok(_) => {}
            Err(e) => show_message(&st2, "😴", "Cairn isn't running", &format!("Start it with  systemctl --user start cairnd\n\n{e}")),
        }
    });
}

fn show_message(st: &State, emoji: &str, title: &str, body: &str) {
    util::clear(&st.results);
    st.cards.borrow_mut().clear();
    st.hits.borrow_mut().clear();
    st.understood.set_visible(false);
    let b = gtk4::Box::new(gtk4::Orientation::Vertical, 8);
    b.add_css_class("empty");
    b.set_halign(gtk4::Align::Center);
    let e = label(emoji, &["empty-emoji"]);
    e.set_xalign(0.5);
    b.append(&e);
    let t = label(title, &["empty-title"]);
    t.set_xalign(0.5);
    b.append(&t);
    let m = label(body, &["dim"]);
    m.set_xalign(0.5);
    m.set_justify(gtk4::Justification::Center);
    m.set_wrap(true);
    b.append(&m);
    st.results.append(&b);
}

fn understood_text(r: &SearchResult) -> String {
    let p = &r.parsed;
    let mut parts = Vec::new();
    if !p.kinds.is_empty() {
        parts.push(p.kinds.iter().map(|k| k.as_str()).collect::<Vec<_>>().join(" / "));
    }
    if let Some(d) = &p.date {
        parts.push(format!("📅 {d}"));
    }
    if !p.semantic.is_empty() {
        parts.push(format!("“{}”", p.semantic));
    }
    if let Some(sc) = &p.scope {
        parts.push(format!("📍 {}", util::tilde(sc)));
    }
    let mut s = format!("✨ {} found in {} ms", r.hits.len(), r.took_ms);
    if !parts.is_empty() {
        s.push_str(&format!("  ·  understood: {}", parts.join("  ·  ")));
    }
    if r.partial {
        s.push_str("  ·  still learning your files, more to come");
    }
    s
}

fn show_results(st: &Rc<State>, r: SearchResult) {
    util::clear(&st.results);
    st.cards.borrow_mut().clear();
    st.selected.set(None);
    st.understood.set_text(&understood_text(&r));
    st.understood.set_visible(true);
    if r.hits.is_empty() {
        let body = if r.partial {
            "Cairn is still learning your files (see the Index page). Try fewer words, or a different way to say it."
        } else {
            "Try fewer words, a different way to say it, or remove the folder filter."
        };
        show_message(st, "🌱", "Nothing here yet", body);
        st.understood.set_visible(true);
        return;
    }
    let images = r.hits.iter().filter(|h| h.category == "image").count();
    let as_grid = images * 10 >= r.hits.len() * 6;
    if as_grid {
        let grid = gtk4::FlowBox::new();
        grid.set_selection_mode(gtk4::SelectionMode::None);
        grid.set_homogeneous(true);
        grid.set_max_children_per_line(8);
        grid.set_min_children_per_line(3);
        grid.set_column_spacing(12);
        grid.set_row_spacing(12);
        for (i, h) in r.hits.iter().enumerate() {
            let tile = photo_tile(st, h, i);
            grid.insert(&tile, -1);
            st.cards.borrow_mut().push(tile.upcast());
        }
        st.results.append(&grid);
    } else {
        for (i, h) in r.hits.iter().enumerate() {
            let c = card(st, h, i);
            st.results.append(&c);
            st.cards.borrow_mut().push(c.upcast());
        }
    }
    *st.hits.borrow_mut() = r.hits;
    set_selected(st, Some(0));
}

fn click_to_open(widget: &impl IsA<gtk4::Widget>, st: &Rc<State>, idx: usize) {
    let g = gtk4::GestureClick::new();
    let st = st.clone();
    g.connect_released(move |_, n, _, _| {
        set_selected(&st, Some(idx));
        if n >= 1 {
            if let Some(h) = st.hits.borrow().get(idx) {
                util::open_hit(h);
            }
        }
    });
    widget.as_ref().add_controller(g);
}

fn photo_tile(st: &Rc<State>, h: &Hit, idx: usize) -> gtk4::Box {
    let tile = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
    tile.add_css_class("photo");
    let ph = gtk4::Picture::new();
    ph.set_size_request(190, 142);
    ph.set_content_fit(gtk4::ContentFit::Cover);
    ph.set_can_shrink(true);
    ph.add_css_class("ph");
    tile.append(&ph);
    let name = h.path.path.rsplit('/').next().unwrap_or("").to_string();
    let cap = label(&name, &["photo-cap"]);
    cap.set_ellipsize(pango::EllipsizeMode::Middle);
    cap.set_width_chars(1);
    cap.set_max_width_chars(20);
    tile.append(&cap);
    let sub = label(&format!("{}{}", h.date.clone().unwrap_or_default(), h.why.first().map(|w| format!(" · {w}")).unwrap_or_default()), &["dim", "small"]);
    sub.set_ellipsize(pango::EllipsizeMode::End);
    sub.set_width_chars(1);
    sub.set_max_width_chars(20);
    tile.append(&sub);
    tile.set_tooltip_text(Some(&util::tilde(&h.path.path)));
    let ph2 = ph.clone();
    util::load_thumbnail(util::hit_path(h), 320, move |t| {
        if let Some(t) = t {
            ph2.set_paintable(Some(&t));
        }
    });
    click_to_open(&tile, st, idx);
    tile
}

fn card(st: &Rc<State>, h: &Hit, idx: usize) -> gtk4::Box {
    let card = gtk4::Box::new(gtk4::Orientation::Horizontal, 14);
    card.add_css_class("cn-card");

    // Left: thumbnail for images, kind badge otherwise.
    if h.category == "image" {
        let pic = gtk4::Picture::new();
        pic.set_size_request(64, 64);
        pic.set_content_fit(gtk4::ContentFit::Cover);
        pic.set_valign(gtk4::Align::Start);
        let p2 = pic.clone();
        util::load_thumbnail(util::hit_path(h), 128, move |t| {
            if let Some(t) = t {
                p2.set_paintable(Some(&t));
            }
        });
        card.append(&pic);
    } else {
        let badge = label(util::kind_emoji(&h.category, h.is_dir), &["kind-badge"]);
        badge.set_xalign(0.5);
        badge.set_valign(gtk4::Align::Start);
        card.append(&badge);
    }

    let mid = gtk4::Box::new(gtk4::Orientation::Vertical, 5);
    mid.set_hexpand(true);
    let full = &h.path.path;
    let name = full.rsplit('/').next().unwrap_or(full);
    let title = match (h.line, h.line_end) {
        (Some(a), Some(b)) if b > a => format!("{}  <span alpha='55%' weight='600'>lines {a}–{b}</span>", esc(name)),
        (Some(a), _) => format!("{}  <span alpha='55%' weight='600'>line {a}</span>", esc(name)),
        _ => esc(name),
    };
    let t = markup(&title, &["cn-card-title"]);
    t.set_ellipsize(pango::EllipsizeMode::End);
    mid.append(&t);
    let dir = full.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
    let p = label(&format!("{}{}", util::tilde(dir), h.date.as_ref().map(|d| format!("   ·   {d}")).unwrap_or_default()), &["mono", "dim"]);
    p.set_ellipsize(pango::EllipsizeMode::Middle);
    mid.append(&p);
    if !h.why.is_empty() {
        let whys = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
        for w in h.why.iter().take(3) {
            let l = label(w, &["why"]);
            l.set_ellipsize(pango::EllipsizeMode::End);
            l.set_max_width_chars(36);
            whys.append(&l);
        }
        mid.append(&whys);
    }
    if let Some(sn) = &h.snippet {
        let s = label(sn, &["snippet", "mono"]);
        s.set_wrap(true);
        s.set_wrap_mode(pango::WrapMode::WordChar);
        s.set_lines(3);
        s.set_ellipsize(pango::EllipsizeMode::End);
        mid.append(&s);
    }
    card.append(&mid);

    let actions = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
    actions.set_valign(gtk4::Align::Center);
    let open = util::pill_button("Open", &["icon-btn"]);
    let reveal = util::pill_button("Show in folder", &["icon-btn"]);
    let copy = util::pill_button("Copy path", &["icon-btn"]);
    {
        let h = h.clone();
        open.connect_clicked(move |_| util::open_hit(&h));
    }
    {
        let path = util::hit_path(h);
        reveal.connect_clicked(move |_| util::reveal(&path));
    }
    {
        let text = h.path.path.clone();
        copy.connect_clicked(move |b| {
            util::copy_text(b, &text);
            b.set_label("Copied ✓");
            let b2 = b.clone();
            glib::timeout_add_local_once(Duration::from_millis(1200), move || b2.set_label("Copy path"));
        });
    }
    actions.append(&open);
    actions.append(&reveal);
    actions.append(&copy);
    card.append(&actions);
    click_to_open(&mid, st, idx);
    card
}
