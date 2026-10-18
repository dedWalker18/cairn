//! Folders page: what Cairn never looks at, and how to change that.

use std::rc::Rc;

use cairn_core::ipc::{Request, Response, Status, WirePath};
use gtk4::prelude::*;
use gtk4::gio;

use crate::util::{self, label, markup};
use crate::Ctx;

pub fn build(ctx: &Rc<Ctx>) -> gtk4::Widget {
    let page = gtk4::Box::new(gtk4::Orientation::Vertical, 14);
    page.add_css_class("page");
    page.append(&label("Folders", &["hero"]));
    let sub = label("Keep private or noisy folders out of search. Excluding removes everything Cairn learned there, and it never looks inside again.", &["hero-sub"]);
    sub.set_wrap(true);
    page.append(&sub);

    let add = util::pill_button("＋  Exclude a folder…", &["primary"]);
    add.set_halign(gtk4::Align::Start);
    page.append(&add);
    let feedback = label("", &["dim", "small"]);
    page.append(&feedback);

    page.append(&label("Excluded", &["h2"]));
    let list = gtk4::Box::new(gtk4::Orientation::Vertical, 8);
    page.append(&list);

    page.append(&label("Skipped automatically", &["h2"]));
    let auto = markup(
        "•  Hidden folders (<tt>.config</tt>, <tt>.cache</tt>, <tt>.git</tt> …) and <tt>node_modules</tt>, <tt>__pycache__</tt>, Python virtualenvs\n\
         •  Any folder containing a <tt>.noindex</tt> file (also <tt>CACHEDIR.TAG</tt>, <tt>.nomedia</tt>)\n\
         •  Other drives, mounts and btrfs subvolumes inside your home\n\
         •  Secret-looking files (<tt>.env</tt>, <tt>*.pem</tt>, <tt>*password*</tt> …): found by name, contents never read",
        &["row-card"],
    );
    auto.set_wrap(true);
    page.append(&auto);
    let tip = markup("💡  In Nautilus, right-click a folder → <b>Cairn → Exclude from search</b>.", &["note"]);
    tip.set_wrap(true);
    page.append(&tip);

    {
        let (ctx, feedback) = (ctx.clone(), feedback.clone());
        add.connect_clicked(move |_| {
            let dialog = gtk4::FileDialog::builder().title("Exclude a folder from search").modal(true).build();
            let (ctx2, fb) = (ctx.clone(), feedback.clone());
            dialog.select_folder(Some(&ctx.window), gio::Cancellable::NONE, move |res| {
                let Ok(f) = res else { return };
                let Some(p) = f.path() else { return };
                fb.set_text("Excluding… (large folders take a few seconds)");
                let path = WirePath::from_bytes(&cairn_core::paths::to_bytes(&p));
                let (ctx3, fb2) = (ctx2.clone(), fb.clone());
                util::call_async(Request::Exclude { path }, 300, move |r| {
                    fb2.set_text(&match r {
                        Ok(Response::Ok { message }) => format!("✓ {message}"),
                        Ok(Response::Error { message }) => format!("Couldn't exclude: {message}"),
                        Ok(_) => String::new(),
                        Err(e) => format!("Couldn't reach Cairn: {e}"),
                    });
                    ctx3.refresh_status();
                });
            });
        });
    }

    let last: Rc<std::cell::RefCell<Vec<String>>> = Rc::default();
    {
        let ctx = ctx.clone();
        ctx.clone().on_status(move |st: Option<&Status>| {
            let ex = st.map(|s| s.exclusions.clone()).unwrap_or_default();
            if *last.borrow() == ex && list.first_child().is_some() {
                return;
            }
            *last.borrow_mut() = ex.clone();
            util::clear(&list);
            if ex.is_empty() {
                list.append(&label("Nothing excluded. Cairn indexes your whole home (minus the automatic skips below).", &["dim"]));
            }
            for p in ex {
                let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 10);
                row.add_css_class("row-card");
                row.append(&label("🙈", &[]));
                let l = label(&util::tilde(&p), &["mono"]);
                l.set_hexpand(true);
                l.set_ellipsize(gtk4::pango::EllipsizeMode::Middle);
                row.append(&l);
                let b = util::pill_button("Include again", &["soft"]);
                let (ctx2, path) = (ctx.clone(), p.clone());
                b.connect_clicked(move |b| {
                    b.set_sensitive(false);
                    b.set_label("Including…");
                    let ctx3 = ctx2.clone();
                    util::call_async(Request::Include { path: WirePath { path: path.clone(), raw: None } }, 60, move |_| ctx3.refresh_status());
                });
                row.append(&b);
                list.append(&row);
            }
        });
    }

    let scroller = gtk4::ScrolledWindow::new();
    scroller.set_hscrollbar_policy(gtk4::PolicyType::Never);
    scroller.set_child(Some(&page));
    scroller.upcast()
}
