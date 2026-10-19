//! The main window: switches between the start, scanning and results pages
//! and runs scans.

use super::results::ResultsView;
use super::scan_page::ScanPage;
use super::start_page::StartPage;
use crate::helper;
use crate::scanner::{self, Progress, ScanError, ScanOptions};
use crate::settings;
use adw::prelude::*;
use gtk::{gdk, gio, glib};
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::{Rc, Weak};
use std::sync::Arc;

#[derive(Clone, Debug)]
pub struct ScanRequest {
    pub path: PathBuf,
    pub elevated: bool,
    pub one_filesystem: bool,
    pub threads: usize,
}

impl ScanRequest {
    /// A request using the current preferences.
    pub fn for_path(path: PathBuf) -> Self {
        let s = settings::get();
        let elevated = s.elevated && !rustix::process::geteuid().is_root() && helper::pkexec_path().is_some();
        ScanRequest { path, elevated, one_filesystem: s.one_filesystem, threads: s.threads }
    }
}

pub struct Inner {
    window: adw::ApplicationWindow,
    toasts: adw::ToastOverlay,
    stack: gtk::Stack,
    start: Rc<StartPage>,
    scanning: Rc<ScanPage>,
    results: Rc<ResultsView>,
    active: RefCell<Option<Arc<Progress>>>,
    last: RefCell<Option<ScanRequest>>,
}

thread_local! {
    static WINDOWS: RefCell<Vec<Weak<Inner>>> = const { RefCell::new(Vec::new()) };
}

/// Calls `f` for every open window.
pub fn for_each_window(f: impl Fn(&Window)) {
    let live: Vec<Rc<Inner>> = WINDOWS.with(|w| {
        let mut w = w.borrow_mut();
        w.retain(|i| i.strong_count() > 0);
        w.iter().filter_map(Weak::upgrade).collect()
    });
    for inner in live {
        f(&Window { inner });
    }
}

type WindowAction = (&'static str, fn(&Window));

#[derive(Clone)]
pub struct Window {
    inner: Rc<Inner>,
}

fn main_menu() -> gio::Menu {
    let menu = gio::Menu::new();
    let scan = gio::Menu::new();
    scan.append(Some("_New Window"), Some("app.new-window"));
    scan.append(Some("_Scan Folder…"), Some("win.open-folder"));
    menu.append_section(None, &scan);
    let rescan = gio::Menu::new();
    rescan.append(Some("Scan _Again"), Some("win.rescan"));
    rescan.append(Some("Scan Again as _Administrator"), Some("win.rescan-admin"));
    menu.append_section(None, &rescan);
    let app = gio::Menu::new();
    app.append(Some("_Preferences"), Some("app.preferences"));
    app.append(Some("_Keyboard Shortcuts"), Some("app.shortcuts"));
    app.append(Some(&format!("_About {}", super::APP_NAME)), Some("app.about"));
    menu.append_section(None, &app);
    menu
}

impl Window {
    pub fn new(app: &adw::Application) -> Window {
        let s = settings::get();
        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title(super::APP_NAME)
            .default_width(s.window_width)
            .default_height(s.window_height)
            .width_request(360)
            .height_request(400)
            .build();
        if s.maximized {
            window.maximize();
        }
        let menu: gio::MenuModel = main_menu().upcast();
        let toasts = adw::ToastOverlay::new();
        let stack =
            gtk::Stack::builder().transition_type(gtk::StackTransitionType::Crossfade).transition_duration(150).build();
        let start = StartPage::new(&window, &menu);
        let scanning = ScanPage::new();
        let results = ResultsView::new(&window, &toasts, &menu);
        stack.add_named(&start.root, Some("start"));
        stack.add_named(&scanning.root, Some("scanning"));
        stack.add_named(&results.root, Some("results"));
        toasts.set_child(Some(&stack));
        window.set_content(Some(&toasts));

        let inner = Rc::new(Inner {
            window: window.clone(),
            toasts,
            stack,
            start,
            scanning,
            results,
            active: RefCell::default(),
            last: RefCell::default(),
        });
        WINDOWS.with(|w| w.borrow_mut().push(Rc::downgrade(&inner)));
        let win = Window { inner };
        win.setup();
        win
    }

    fn setup(&self) {
        let inner = &self.inner;
        let weak = Rc::downgrade(inner);
        let with = move |f: fn(&Window)| {
            let weak = weak.clone();
            move || {
                if let Some(inner) = weak.upgrade() {
                    f(&Window { inner });
                }
            }
        };

        {
            let weak = Rc::downgrade(inner);
            inner.start.connect_scan(move |path| {
                if let Some(inner) = weak.upgrade() {
                    Window { inner }.scan(ScanRequest::for_path(path));
                }
            });
        }
        {
            let cancel = with(|w| w.cancel_scan());
            inner.scanning.connect_cancel(cancel);
        }
        {
            let weak = Rc::downgrade(inner);
            inner.results.connect_rescan(move |elevated| {
                if let Some(inner) = weak.upgrade() {
                    Window { inner }.rescan(elevated);
                }
            });
        }

        let actions: [WindowAction; 6] = [
            ("home", |w| w.go_home()),
            ("rescan", |w| w.rescan(false)),
            ("rescan-admin", |w| w.rescan(true)),
            ("open-folder", |w| w.inner.start.choose_folder()),
            ("search", |w| {
                if w.inner.results.tree().is_some() && w.page() != "scanning" {
                    w.inner.stack.set_visible_child_name("results");
                    w.inner.results.focus_search();
                }
            }),
            ("close", |w| w.inner.window.close()),
        ];
        for (name, f) in actions {
            let action = gio::SimpleAction::new(name, None);
            let run = with(f);
            action.connect_activate(move |_, _| run());
            inner.window.add_action(&action);
        }
        self.update_actions();

        // Drop a folder anywhere on the window to scan it.
        let drop = gtk::DropTarget::new(gdk::FileList::static_type(), gdk::DragAction::COPY);
        let weak = Rc::downgrade(inner);
        drop.connect_drop(move |_, value, _, _| {
            let Some(inner) = weak.upgrade() else { return false };
            let Ok(list) = value.get::<gdk::FileList>() else { return false };
            let Some(path) = list.files().first().and_then(|f| f.path()) else { return false };
            let path = if path.is_dir() { path } else { path.parent().map(PathBuf::from).unwrap_or(path) };
            Window { inner }.scan(ScanRequest::for_path(path));
            true
        });
        inner.toasts.add_controller(drop);

        // The window owns `inner`; this handler keeps it alive until closing.
        let keep = inner.clone();
        inner.window.connect_close_request(move |w| {
            if let Some(p) = keep.active.take() {
                p.cancel();
            }
            let (width, height) = w.default_size();
            let maximized = w.is_maximized();
            let panes = keep.results.pane_fractions();
            settings::update(|s| {
                if !maximized && width > 0 && height > 0 {
                    s.window_width = width;
                    s.window_height = height;
                }
                s.maximized = maximized;
                if let Some((v, h)) = panes {
                    s.split_vertical = v;
                    s.split_horizontal = h;
                }
            });
            keep.results.clear();
            glib::Propagation::Proceed
        });
    }

    pub fn present(&self) {
        self.inner.window.present();
    }

    fn page(&self) -> String {
        self.inner.stack.visible_child_name().map(|s| s.to_string()).unwrap_or_default()
    }

    fn show_page(&self, name: &str) {
        self.inner.stack.set_visible_child_name(name);
        if name != "results" {
            self.inner.window.set_title(Some(super::APP_NAME));
        }
        self.update_actions();
    }

    fn update_actions(&self) {
        let has_results = self.inner.results.tree().is_some();
        let scanning = self.inner.active.borrow().is_some();
        let has_last = self.inner.last.borrow().is_some();
        let can_elevate = !rustix::process::geteuid().is_root() && helper::pkexec_path().is_some();
        let set = |name: &str, enabled: bool| {
            if let Some(a) = self.inner.window.lookup_action(name).and_downcast::<gio::SimpleAction>() {
                a.set_enabled(enabled);
            }
        };
        set("rescan", has_last && !scanning);
        set("rescan-admin", has_last && !scanning && can_elevate);
        set("search", has_results && !scanning);
        set("home", !scanning);
    }

    pub fn scan_path(&self, path: PathBuf) {
        self.scan(ScanRequest::for_path(path));
    }

    fn rescan(&self, elevated: bool) {
        let last = self.inner.last.borrow().clone();
        if let Some(mut req) = last {
            if elevated {
                req.elevated = true;
            }
            self.scan(req);
        }
    }

    fn go_home(&self) {
        if self.inner.active.borrow().is_some() {
            return;
        }
        // Free the memory held by the previous results.
        self.inner.results.clear();
        self.inner.start.refresh_drives();
        self.show_page("start");
    }

    pub fn scan(&self, req: ScanRequest) {
        let inner = &self.inner;
        if let Some(p) = inner.active.take() {
            p.cancel();
        }
        let progress = Arc::new(Progress::default());
        inner.active.replace(Some(progress.clone()));
        inner.last.replace(Some(req.clone()));
        inner.scanning.start(&req.path, req.elevated, progress.clone());
        self.show_page("scanning");

        let weak = Rc::downgrade(inner);
        glib::spawn_future_local(async move {
            let p = progress.clone();
            let r = req.clone();
            let result = gio::spawn_blocking(move || {
                let opts = ScanOptions { one_filesystem: r.one_filesystem, threads: r.threads };
                if r.elevated {
                    helper::scan_elevated(&r.path, opts, &p)
                } else {
                    scanner::scan(&r.path, opts, &p)
                }
            })
            .await;
            let Some(inner) = weak.upgrade() else { return };
            let current = inner.active.borrow().as_ref().is_some_and(|a| Arc::ptr_eq(a, &progress));
            if !current {
                return; // superseded or cancelled
            }
            inner.active.replace(None);
            inner.scanning.stop();
            let win = Window { inner };
            match result {
                Ok(Ok(tree)) => {
                    win.inner.results.set_tree(Arc::new(tree));
                    win.show_page("results");
                }
                Ok(Err(ScanError::Cancelled)) => win.after_scan_ended(),
                Ok(Err(ScanError::Failed(msg))) => {
                    win.after_scan_ended();
                    win.error("Scan Failed", &msg);
                }
                Err(_) => {
                    win.after_scan_ended();
                    win.error("Scan Failed", "The scanner stopped unexpectedly.");
                }
            }
        });
    }

    fn cancel_scan(&self) {
        if let Some(p) = self.inner.active.take() {
            p.cancel();
        }
        self.inner.scanning.stop();
        self.after_scan_ended();
    }

    fn after_scan_ended(&self) {
        if self.inner.results.tree().is_some() {
            self.show_page("results");
        } else {
            self.show_page("start");
        }
    }

    fn error(&self, title: &str, msg: &str) {
        let dialog = adw::AlertDialog::new(Some(title), Some(msg));
        dialog.add_response("close", "_Close");
        dialog.present(Some(&self.inner.window));
    }

    /// Re-applies preferences after they changed.
    pub fn apply_settings(&self) {
        self.inner.results.apply_settings();
    }
}
