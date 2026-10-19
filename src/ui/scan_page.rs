//! Progress page shown while a scan runs.

use crate::format;
use crate::scanner::Progress;
use adw::prelude::*;
use gtk::glib;
use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub struct ScanPage {
    pub root: adw::ToolbarView,
    title: gtk::Label,
    path: gtk::Label,
    files: gtk::Label,
    dirs: gtk::Label,
    size: gtk::Label,
    elapsed: gtk::Label,
    current: gtk::Label,
    timer: RefCell<Option<glib::SourceId>>,
    on_cancel: RefCell<Option<Rc<dyn Fn()>>>,
}

fn stat(caption: &str) -> (gtk::Box, gtk::Label) {
    let b = gtk::Box::new(gtk::Orientation::Vertical, 2);
    b.add_css_class("card");
    b.add_css_class("dendro-stat");
    let value = gtk::Label::new(Some("0"));
    value.add_css_class("title-2");
    value.add_css_class("numeric");
    let cap = gtk::Label::new(Some(caption));
    cap.add_css_class("caption");
    cap.add_css_class("dim-label");
    b.append(&value);
    b.append(&cap);
    (b, value)
}

impl ScanPage {
    pub fn new() -> Rc<Self> {
        let header = adw::HeaderBar::new();
        header.set_title_widget(Some(&adw::WindowTitle::new("Scanning", "")));

        let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
        content.set_valign(gtk::Align::Center);
        content.set_margin_top(24);
        content.set_margin_bottom(24);
        content.set_margin_start(16);
        content.set_margin_end(16);

        let spinner = gtk::Spinner::builder().spinning(true).width_request(48).height_request(48).build();
        spinner.set_margin_bottom(12);
        let title = gtk::Label::new(Some("Scanning…"));
        title.add_css_class("title-1");
        let path = gtk::Label::builder().ellipsize(gtk::pango::EllipsizeMode::Middle).max_width_chars(60).build();
        path.add_css_class("dim-label");

        let grid = gtk::Grid::builder()
            .column_spacing(12)
            .row_spacing(12)
            .column_homogeneous(true)
            .halign(gtk::Align::Center)
            .margin_top(18)
            .margin_bottom(6)
            .build();
        let (fb, files) = stat("Files");
        let (db, dirs) = stat("Folders");
        let (sb, size) = stat("Size on Disk");
        let (eb, elapsed) = stat("Elapsed");
        grid.attach(&fb, 0, 0, 1, 1);
        grid.attach(&db, 1, 0, 1, 1);
        grid.attach(&sb, 2, 0, 1, 1);
        grid.attach(&eb, 3, 0, 1, 1);

        let current = gtk::Label::builder().ellipsize(gtk::pango::EllipsizeMode::Middle).max_width_chars(70).build();
        current.add_css_class("caption");
        current.add_css_class("dim-label");

        let cancel = gtk::Button::with_label("Cancel");
        cancel.add_css_class("pill");
        cancel.set_halign(gtk::Align::Center);
        cancel.set_margin_top(18);

        content.append(&spinner);
        content.append(&title);
        content.append(&path);
        content.append(&grid);
        content.append(&current);
        content.append(&cancel);

        let clamp = adw::Clamp::builder().maximum_size(720).child(&content).build();
        let root = adw::ToolbarView::new();
        root.add_top_bar(&header);
        root.set_content(Some(&clamp));

        let page = Rc::new(ScanPage {
            root,
            title,
            path,
            files,
            dirs,
            size,
            elapsed,
            current,
            timer: RefCell::default(),
            on_cancel: RefCell::default(),
        });
        let weak = Rc::downgrade(&page);
        cancel.connect_clicked(move |_| {
            let cb = weak.upgrade().and_then(|p| p.on_cancel.borrow().clone());
            if let Some(cb) = cb {
                cb();
            }
        });
        page
    }

    pub fn connect_cancel(&self, f: impl Fn() + 'static) {
        self.on_cancel.replace(Some(Rc::new(f)));
    }

    pub fn start(self: &Rc<Self>, path: &Path, elevated: bool, progress: Arc<Progress>) {
        self.stop();
        self.title.set_text(if elevated { "Waiting for Authentication…" } else { "Scanning…" });
        self.path.set_text(&path.to_string_lossy());
        self.current.set_text("");
        let started = Instant::now();
        let weak = Rc::downgrade(self);
        let update = move || {
            let Some(p) = weak.upgrade() else { return glib::ControlFlow::Break };
            let dirs = progress.dirs.load(Relaxed);
            if elevated && dirs > 0 {
                p.title.set_text("Scanning as Administrator…");
            }
            p.files.set_text(&format::count(progress.files.load(Relaxed)));
            p.dirs.set_text(&format::count(dirs));
            p.size.set_text(&format::size(progress.bytes.load(Relaxed)));
            p.elapsed.set_text(&format::duration(started.elapsed()));
            p.current.set_text(&progress.current().to_string_lossy());
            glib::ControlFlow::Continue
        };
        update();
        let id = glib::timeout_add_local(Duration::from_millis(100), update);
        self.timer.replace(Some(id));
    }

    pub fn stop(&self) {
        if let Some(id) = self.timer.take() {
            id.remove();
        }
    }
}
