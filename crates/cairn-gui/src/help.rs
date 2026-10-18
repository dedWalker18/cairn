//! Tips & keys page.

use gtk4::prelude::*;

use crate::util::{label, markup};

fn key_row(keys: &[&str], what: &str) -> gtk4::Box {
    let r = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
    r.add_css_class("row-card");
    for k in keys {
        r.append(&label(k, &["kbd"]));
    }
    let w = label(what, &[]);
    w.set_margin_start(8);
    r.append(&w);
    r
}

fn section(title: &str) -> gtk4::Label {
    let l = label(title, &["h2"]);
    l.set_margin_top(10);
    l
}

pub fn build() -> gtk4::Widget {
    let page = gtk4::Box::new(gtk4::Orientation::Vertical, 10);
    page.add_css_class("page");
    page.append(&label("Tips & keys", &["hero"]));
    page.append(&label("A few things that make Cairn feel like magic.", &["hero-sub"]));

    page.append(&section("Ask naturally"));
    for (q, why) in [
        ("photos of mountains from last december", "kind + what's in the picture + date"),
        ("fire photos from downloads", "“from <folder>” searches only that folder"),
        ("the script that runs terraform on aws", "code hits open in Zed at the right line"),
        ("sales report for 25's last quarter", "Q4 2025: quarters, months, “last week”, “2 days ago”…"),
        ("that SOP the client gave for this project", "“this project / here” means the current folder"),
    ] {
        page.append(&markup(&format!("<b>{}</b>\n<span alpha='60%'>{}</span>", crate::util::esc(q), crate::util::esc(why)), &["row-card"]));
    }

    page.append(&section("Give your files context"));
    let ctx = markup(
        "Folder names and notes are Cairn's favourite clues. Rename <tt>IMG_dump</tt> to <tt>Manali trek Dec 25</tt>, or drop a <tt>notes.txt</tt> into a project folder (“client SOPs for Acme, 2026”), and every file inside inherits that meaning. No tagging needed.",
        &["note"],
    );
    ctx.set_wrap(true);
    page.append(&ctx);

    page.append(&section("Keyboard"));
    page.append(&key_row(&["Super", "/"], "Search from anywhere (fuzzel)"));
    page.append(&key_row(&["↑", "↓"], "Move through results"));
    page.append(&key_row(&["Enter"], "Open the selected result"));
    page.append(&key_row(&["Ctrl", "Enter"], "Show it in Nautilus"));
    page.append(&key_row(&["Ctrl", "C"], "Copy its path"));
    page.append(&key_row(&["Ctrl", "K"], "Jump to the search field"));
    page.append(&key_row(&["Esc"], "Clear the search"));
    page.append(&key_row(&["Ctrl", "1–4"], "Switch pages"));

    page.append(&section("From the terminal"));
    let cli = label(
        "cairn                              open this window\ncairn search fire photos from downloads\ncairn search --here config parser   only the current folder\ncairn search --in ~/Zed deploy      only that folder\ncairn search --json …               for scripts and AI agents\ncairn status · pause · resume · exclude <dir> · include <dir> · doctor",
        &["snippet", "mono"],
    );
    page.append(&cli);

    page.append(&section("About"));
    let about = markup(
        &format!(
            "Cairn {} · Apache-2.0 · runs entirely on this computer.\nModels: bge-small-en-v1.5 (text) and CLIP ViT-B/32 LAION (images), both MIT.\nFonts: Fraunces and Nunito (SIL Open Font License).",
            env!("CARGO_PKG_VERSION")
        ),
        &["dim", "small"],
    );
    about.set_wrap(true);
    page.append(&about);

    let scroller = gtk4::ScrolledWindow::new();
    scroller.set_hscrollbar_policy(gtk4::PolicyType::Never);
    scroller.set_child(Some(&page));
    scroller.upcast()
}
