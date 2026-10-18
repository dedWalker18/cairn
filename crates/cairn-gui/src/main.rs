//! cairn-gui: Cairn's home. Search, index health, folders and help in one
//! warm, calm window. Started by `cairn` with no arguments, by Nautilus
//! ("Search with Cairn…", with --scope) or from the app grid.
//! Every daemon call runs off the UI thread.

mod folders;
mod garden;
mod help;
mod search;
mod util;

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use cairn_core::ipc::{Request, Response, Status};
use gtk4::prelude::*;
use gtk4::{gdk, gio, glib};

const APP_ID: &str = "dev.cairn.Cairn";

pub struct Args {
    pub scope: Option<PathBuf>,
    pub query: String,
    pub page: String,
    /// Render the window to this PNG after a delay, then exit. Used to make
    /// documentation screenshots that can only ever contain Cairn itself.
    pub snapshot: Option<PathBuf>,
}

fn parse_args() -> Args {
    let mut a = Args { scope: None, query: String::new(), page: "search".into(), snapshot: None };
    let mut it = std::env::args_os().skip(1);
    while let Some(arg) = it.next() {
        match arg.to_str() {
            Some("--scope") => a.scope = it.next().map(PathBuf::from),
            Some("--query") => a.query = it.next().map(|q| q.to_string_lossy().into_owned()).unwrap_or_default(),
            Some("--page") => a.page = it.next().map(|q| q.to_string_lossy().into_owned()).unwrap_or_default(),
            Some("--snapshot") => a.snapshot = it.next().map(PathBuf::from),
            _ => {}
        }
    }
    a
}

/// Shared app context handed to every page.
pub struct Ctx {
    pub window: gtk4::ApplicationWindow,
    pub status_listeners: RefCell<Vec<Box<dyn Fn(Option<&Status>)>>>,
    pub last_status: RefCell<Option<Status>>,
}

impl Ctx {
    pub fn on_status(&self, f: impl Fn(Option<&Status>) + 'static) {
        f(self.last_status.borrow().as_ref());
        self.status_listeners.borrow_mut().push(Box::new(f));
    }

    pub fn refresh_status(self: &Rc<Self>) {
        let ctx = self.clone();
        util::call_async(Request::Status, 5, move |r| {
            let st = match r {
                Ok(Response::Status(s)) => Some(s),
                _ => None,
            };
            *ctx.last_status.borrow_mut() = st.clone();
            for l in ctx.status_listeners.borrow().iter() {
                l(st.as_ref());
            }
        });
    }
}

// Fonts ship inside the binary (SIL OFL) and are registered for this process
// only: nothing is installed or written anywhere persistent.
const FONTS: [(&str, &[u8]); 2] = [("Nunito.ttf", include_bytes!("../fonts/Nunito.ttf")), ("Fraunces.ttf", include_bytes!("../fonts/Fraunces.ttf"))];

#[link(name = "fontconfig")]
extern "C" {
    fn FcConfigAppFontAddFile(config: *mut std::ffi::c_void, file: *const std::ffi::c_char) -> std::ffi::c_int;
}

fn load_fonts() {
    let dir = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir).join("cairn-gui-fonts");
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    for (name, bytes) in FONTS {
        let p = dir.join(name);
        if std::fs::metadata(&p).map(|m| m.len() as usize != bytes.len()).unwrap_or(true) {
            let tmp = dir.join(format!(".{name}.{}", std::process::id()));
            if std::fs::write(&tmp, bytes).is_err() || std::fs::rename(&tmp, &p).is_err() {
                continue;
            }
        }
        if let Ok(c) = std::ffi::CString::new(p.as_os_str().as_encoded_bytes()) {
            unsafe {
                FcConfigAppFontAddFile(std::ptr::null_mut(), c.as_ptr());
            }
        }
    }
}

fn brand() -> gtk4::Box {
    let b = gtk4::Box::new(gtk4::Orientation::Horizontal, 10);
    let stones = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
    stones.set_valign(gtk4::Align::Center);
    for c in ["stone-1", "stone-2", "stone-3"] {
        let s = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
        s.add_css_class("stone");
        s.add_css_class(c);
        s.set_halign(gtk4::Align::Center);
        stones.append(&s);
    }
    b.append(&stones);
    let words = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    words.append(&util::label("cairn", &["brand"]));
    words.append(&util::label("finds things by meaning", &["brand-sub"]));
    b.append(&words);
    b.set_margin_bottom(22);
    b.set_margin_start(6);
    b
}

fn build(app: &gtk4::Application, args: Rc<Args>) {
    let (dw, dh) = if args.snapshot.is_some() { (1280, 860) } else { (1140, 780) };
    let window = gtk4::ApplicationWindow::builder().application(app).title("Cairn").default_width(dw).default_height(dh).build();
    window.add_css_class("cairn-window");
    let header = gtk4::HeaderBar::new();
    header.set_title_widget(Some(&gtk4::Box::new(gtk4::Orientation::Horizontal, 0)));
    window.set_titlebar(Some(&header));

    let stack = gtk4::Stack::new();
    stack.set_transition_type(gtk4::StackTransitionType::Crossfade);
    stack.set_transition_duration(140);
    stack.set_hexpand(true);

    let ctx = Rc::new(Ctx { window: window.clone(), status_listeners: RefCell::new(Vec::new()), last_status: RefCell::new(None) });

    let pages: [(&str, &str, gtk4::Widget); 4] = [
        ("search", "🔍  Search", search::build(&ctx, &args)),
        ("garden", "🌱  Index", garden::build(&ctx)),
        ("folders", "📁  Folders", folders::build(&ctx)),
        ("help", "💡  Tips & keys", help::build()),
    ];

    let sidebar = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    sidebar.add_css_class("sidebar");
    sidebar.set_size_request(230, -1);
    sidebar.append(&brand());
    let nav = gtk4::ListBox::new();
    nav.add_css_class("nav");
    nav.set_selection_mode(gtk4::SelectionMode::Single);
    for (name, title, widget) in &pages {
        stack.add_named(widget, Some(name));
        let row = gtk4::ListBoxRow::new();
        row.set_child(Some(&util::label(title, &[])));
        row.set_widget_name(name);
        nav.append(&row);
    }
    {
        let stack = stack.clone();
        nav.connect_row_selected(move |_, row| {
            if let Some(r) = row {
                stack.set_visible_child_name(r.widget_name().as_str());
            }
        });
    }
    sidebar.append(&nav);
    let spacer = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    spacer.set_vexpand(true);
    sidebar.append(&spacer);

    // Live status pill (click: go to the Index page).
    let pill = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    pill.add_css_class("status-pill");
    let dot = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    dot.add_css_class("dot");
    dot.set_valign(gtk4::Align::Center);
    let pill_text = util::label("Connecting…", &[]);
    pill_text.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    pill.append(&dot);
    pill.append(&pill_text);
    let pill_btn = gtk4::Button::new();
    pill_btn.set_child(Some(&pill));
    pill_btn.add_css_class("flat");
    {
        let nav = nav.clone();
        pill_btn.connect_clicked(move |_| {
            if let Some(r) = nav.row_at_index(1) {
                nav.select_row(Some(&r));
            }
        });
    }
    sidebar.append(&pill_btn);
    ctx.on_status(move |st| {
        for c in ["ok", "busy", "rest", "bad"] {
            dot.remove_css_class(c);
        }
        let (class, text) = match st {
            None => ("bad", "Cairn is not running".to_string()),
            Some(s) if s.paused => ("rest", "Paused".to_string()),
            Some(s) if s.throttle.is_some() => ("rest", format!("Resting · {}", s.throttle.clone().unwrap_or_default())),
            Some(s) => {
                let left: i64 = s.pending.iter().map(|p| p.1).sum();
                if s.scanning {
                    ("busy", "Looking around…".to_string())
                } else if left > 0 {
                    ("busy", format!("Learning · {} to go", util::human_count(left)))
                } else {
                    ("ok", "All caught up".to_string())
                }
            }
        };
        dot.add_css_class(class);
        pill_text.set_text(&text);
    });

    let body = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    body.append(&sidebar);
    body.append(&stack);
    window.set_child(Some(&body));

    let start = pages.iter().position(|p| p.0 == args.page).unwrap_or(0);
    if let Some(r) = nav.row_at_index(start as i32) {
        nav.select_row(Some(&r));
    }

    // Ctrl+1..4 switch pages.
    {
        let nav = nav.clone();
        let keys = gtk4::EventControllerKey::new();
        keys.connect_key_pressed(move |_, key, _, mods| {
            if mods.contains(gdk::ModifierType::CONTROL_MASK) {
                let idx = match key {
                    gdk::Key::_1 => Some(0),
                    gdk::Key::_2 => Some(1),
                    gdk::Key::_3 => Some(2),
                    gdk::Key::_4 => Some(3),
                    _ => None,
                };
                if let Some(i) = idx {
                    if let Some(r) = nav.row_at_index(i) {
                        nav.select_row(Some(&r));
                    }
                    return glib::Propagation::Stop;
                }
            }
            glib::Propagation::Proceed
        });
        window.add_controller(keys);
    }

    ctx.refresh_status();
    {
        let ctx = ctx.clone();
        glib::timeout_add_seconds_local(3, move || {
            ctx.refresh_status();
            glib::ControlFlow::Continue
        });
    }
    window.present();
    if let Some(out) = args.snapshot.clone() {
        let (win, app) = (window.clone(), app.clone());
        glib::timeout_add_local_once(std::time::Duration::from_millis(4000), move || {
            snapshot_window(&win, &out);
            app.quit();
        });
    }
}

/// Render the window's own widget tree to a PNG (no screen capture).
fn snapshot_window(win: &gtk4::ApplicationWindow, out: &std::path::Path) {
    let (w, h) = (win.width(), win.height());
    let paintable = gtk4::WidgetPaintable::new(Some(win));
    let snap = gtk4::Snapshot::new();
    paintable.snapshot(&snap, w as f64, h as f64);
    let Some(node) = snap.to_node() else { return };
    let Some(renderer) = win.native().and_then(|n| n.renderer()) else { return };
    let tex = renderer.render_texture(&node, None);
    if let Err(e) = tex.save_to_png(out) {
        eprintln!("snapshot failed: {e}");
    }
}

fn main() -> glib::ExitCode {
    load_fonts();
    let args = Rc::new(parse_args());
    let app = gtk4::Application::builder().application_id(APP_ID).flags(gio::ApplicationFlags::NON_UNIQUE).build();
    app.connect_startup(|_| {
        let provider = gtk4::CssProvider::new();
        provider.load_from_string(include_str!("style.css"));
        // Cairn has its own warm light look: ask GTK for the light variant and
        // apply our stylesheet above the system theme (USER priority).
        if let Some(settings) = gtk4::Settings::default() {
            settings.set_gtk_application_prefer_dark_theme(false);
        }
        if let Some(display) = gdk::Display::default() {
            gtk4::style_context_add_provider_for_display(&display, &provider, gtk4::STYLE_PROVIDER_PRIORITY_USER);
        }
    });
    app.connect_activate(move |app| build(app, args.clone()));
    app.run_with_args::<&str>(&[])
}
