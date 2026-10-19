//! The landing page: pick a drive or folder to analyze.

use crate::format;
use crate::helper;
use crate::mounts::{self, Mount};
use crate::settings;
use adw::prelude::*;
use gtk::{gio, glib};
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

type ScanFn = Rc<dyn Fn(PathBuf)>;

pub struct StartPage {
    pub root: adw::ToolbarView,
    window: glib::WeakRef<adw::ApplicationWindow>,
    drives: adw::PreferencesGroup,
    drive_rows: RefCell<Vec<adw::ActionRow>>,
    on_scan: RefCell<Option<ScanFn>>,
    elevated_row: adw::SwitchRow,
    one_fs_row: adw::SwitchRow,
    mount_monitor: gio::UnixMountMonitor,
    mount_handler: RefCell<Option<glib::SignalHandlerId>>,
}

impl Drop for StartPage {
    fn drop(&mut self) {
        if let Some(handler) = self.mount_handler.take() {
            self.mount_monitor.disconnect(handler);
        }
    }
}

fn level_bar(fraction: f64) -> gtk::LevelBar {
    let bar = gtk::LevelBar::for_interval(0.0, 1.0);
    bar.add_css_class("dendro-usage");
    bar.remove_offset_value(Some(gtk::LEVEL_BAR_OFFSET_LOW));
    bar.remove_offset_value(Some(gtk::LEVEL_BAR_OFFSET_HIGH));
    bar.remove_offset_value(Some(gtk::LEVEL_BAR_OFFSET_FULL));
    bar.add_offset_value("dendro-ok", 0.85);
    bar.add_offset_value("dendro-warn", 0.95);
    bar.add_offset_value("dendro-full", 1.0);
    bar.set_value(fraction);
    bar
}

/// A usage bar coloured by how full the filesystem is.
pub fn usage_bar(fraction: f64) -> gtk::LevelBar {
    level_bar(fraction)
}

impl StartPage {
    pub fn new(window: &adw::ApplicationWindow, menu: &gio::MenuModel) -> Rc<Self> {
        let header = adw::HeaderBar::new();
        let menu_button = gtk::MenuButton::builder()
            .icon_name("open-menu-symbolic")
            .menu_model(menu)
            .tooltip_text("Main Menu")
            .primary(true)
            .build();
        header.pack_end(&menu_button);

        let content = gtk::Box::new(gtk::Orientation::Vertical, 24);
        content.set_margin_top(24);
        content.set_margin_bottom(32);
        content.set_margin_start(16);
        content.set_margin_end(16);

        // Hero
        let hero = gtk::Box::new(gtk::Orientation::Vertical, 6);
        let icon = gtk::Image::from_icon_name(super::app_icon_name());
        icon.set_pixel_size(96);
        icon.add_css_class("icon-dropshadow");
        icon.set_margin_bottom(6);
        let title = gtk::Label::new(Some(super::APP_NAME));
        title.add_css_class("title-1");
        let subtitle = gtk::Label::new(Some("Find out what is using your disk space"));
        subtitle.add_css_class("dim-label");
        subtitle.set_wrap(true);
        subtitle.set_justify(gtk::Justification::Center);
        hero.append(&icon);
        hero.append(&title);
        hero.append(&subtitle);
        content.append(&hero);

        // Quick locations
        let places = adw::PreferencesGroup::builder().title("Folders").build();
        let home = glib::home_dir();
        let home_row = adw::ActionRow::builder()
            .title("Home Folder")
            .subtitle(glib::markup_escape_text(&home.to_string_lossy()).as_str())
            .activatable(true)
            .build();
        home_row.add_prefix(&gtk::Image::from_icon_name("user-home-symbolic"));
        home_row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
        let choose_row = adw::ActionRow::builder()
            .title("Choose a Folder…")
            .subtitle("Or drop a folder onto this window")
            .activatable(true)
            .build();
        choose_row.add_prefix(&gtk::Image::from_icon_name("folder-open-symbolic"));
        choose_row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
        places.add(&home_row);
        places.add(&choose_row);
        content.append(&places);

        // Drives
        let refresh = gtk::Button::builder()
            .icon_name("view-refresh-symbolic")
            .tooltip_text("Refresh Drives")
            .valign(gtk::Align::Center)
            .build();
        refresh.add_css_class("flat");
        let drives = adw::PreferencesGroup::builder()
            .title("Drives")
            .description("Mounted filesystems")
            .header_suffix(&refresh)
            .build();
        content.append(&drives);

        // Options
        let s = settings::get();
        let options = adw::PreferencesGroup::builder().title("Options").build();
        let elevated_row = adw::SwitchRow::builder()
            .title("Scan as Administrator")
            .subtitle("Include folders your account cannot open. You will be asked for your password.")
            .active(s.elevated)
            .build();
        let root_user = rustix::process::geteuid().is_root();
        elevated_row.set_visible(!root_user && helper::pkexec_path().is_some());
        elevated_row.connect_active_notify(|row| {
            let v = row.is_active();
            if settings::get().elevated != v {
                settings::update(|s| s.elevated = v);
                super::settings_changed();
            }
        });
        let one_fs_row = adw::SwitchRow::builder()
            .title("Stay on One Filesystem")
            .subtitle("Skip other drives mounted inside the scanned folder")
            .active(s.one_filesystem)
            .build();
        one_fs_row.connect_active_notify(|row| {
            let v = row.is_active();
            if settings::get().one_filesystem != v {
                settings::update(|s| s.one_filesystem = v);
                super::settings_changed();
            }
        });
        options.add(&elevated_row);
        options.add(&one_fs_row);
        content.append(&options);

        let clamp = adw::Clamp::builder().maximum_size(720).tightening_threshold(560).child(&content).build();
        let scrolled = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .child(&clamp)
            .vexpand(true)
            .build();

        let root = adw::ToolbarView::new();
        root.add_top_bar(&header);
        root.set_content(Some(&scrolled));

        let page = Rc::new(StartPage {
            root,
            window: window.downgrade(),
            drives,
            drive_rows: RefCell::default(),
            on_scan: RefCell::default(),
            elevated_row,
            one_fs_row,
            mount_monitor: gio::UnixMountMonitor::get(),
            mount_handler: RefCell::default(),
        });

        let weak = Rc::downgrade(&page);
        home_row.connect_activated(move |_| {
            if let Some(p) = weak.upgrade() {
                p.emit_scan(glib::home_dir());
            }
        });
        let weak = Rc::downgrade(&page);
        choose_row.connect_activated(move |_| {
            if let Some(p) = weak.upgrade() {
                p.choose_folder();
            }
        });
        let weak = Rc::downgrade(&page);
        refresh.connect_clicked(move |_| {
            if let Some(p) = weak.upgrade() {
                p.refresh_drives();
            }
        });
        // Keep the list current when drives are plugged in or removed.
        let weak = Rc::downgrade(&page);
        let handler = page.mount_monitor.connect_mounts_changed(move |_| {
            if let Some(p) = weak.upgrade() {
                p.refresh_drives();
            }
        });
        page.mount_handler.replace(Some(handler));
        page.refresh_drives();
        page
    }

    /// Shows settings that were changed elsewhere (preferences, other windows).
    pub fn sync_settings(&self) {
        let s = settings::get();
        self.elevated_row.set_active(s.elevated);
        self.one_fs_row.set_active(s.one_filesystem);
    }

    pub fn connect_scan(&self, f: impl Fn(PathBuf) + 'static) {
        self.on_scan.replace(Some(Rc::new(f)));
    }

    fn emit_scan(&self, path: PathBuf) {
        let cb = self.on_scan.borrow().clone();
        if let Some(cb) = cb {
            cb(path);
        }
    }

    pub fn choose_folder(self: &Rc<Self>) {
        let Some(window) = self.window.upgrade() else { return };
        let dialog = gtk::FileDialog::builder().title("Choose a Folder to Scan").modal(true).build();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            if let Ok(file) = dialog.select_folder_future(Some(&window)).await {
                if let (Some(path), Some(p)) = (file.path(), weak.upgrade()) {
                    p.emit_scan(path);
                }
            }
        });
    }

    /// Re-reads the mount table and filesystem usage.
    pub fn refresh_drives(self: &Rc<Self>) {
        for row in self.drive_rows.borrow_mut().drain(..) {
            self.drives.remove(&row);
        }
        let mounts = mounts::listable_mounts(&mounts::read_mounts());
        if mounts.is_empty() {
            let row = adw::ActionRow::builder().title("No drives found").build();
            self.drives.add(&row);
            self.drive_rows.borrow_mut().push(row);
            return;
        }
        for m in mounts {
            let row = self.drive_row(&m);
            self.drives.add(&row);
            self.drive_rows.borrow_mut().push(row);
        }
    }

    fn drive_row(self: &Rc<Self>, m: &Mount) -> adw::ActionRow {
        let source = m.source.strip_prefix("/dev/").unwrap_or(&m.source).to_string();
        let base_subtitle = if m.mount_point.to_string_lossy() == mounts::display_name(m) {
            format!("{source} · {}", m.fstype)
        } else {
            format!("{} · {source} · {}", m.mount_point.display(), m.fstype)
        };
        let row = adw::ActionRow::builder()
            .title(glib::markup_escape_text(&mounts::display_name(m)).as_str())
            .subtitle(glib::markup_escape_text(&base_subtitle).as_str())
            .activatable(true)
            .build();
        row.add_prefix(&gtk::Image::from_icon_name(mounts::icon_name(m)));
        let bar = usage_bar(0.0);
        bar.set_valign(gtk::Align::Center);
        bar.set_size_request(120, -1);
        bar.set_visible(false);
        row.add_suffix(&bar);
        row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));

        let path = m.mount_point.clone();
        let weak = Rc::downgrade(self);
        row.connect_activated(move |_| {
            if let Some(p) = weak.upgrade() {
                p.emit_scan(path.clone());
            }
        });

        // Filesystem usage can block on network mounts; fetch it in the background.
        let path = m.mount_point.clone();
        let row_weak = row.downgrade();
        glib::spawn_future_local(async move {
            let usage = gio::spawn_blocking(move || mounts::fs_usage(&path)).await.ok().flatten();
            let (Some(row), Some(u)) = (row_weak.upgrade(), usage) else { return };
            if u.total == 0 {
                return;
            }
            let fraction = u.used as f64 / u.total as f64;
            bar.set_value(fraction);
            bar.set_visible(true);
            bar.set_tooltip_text(Some(&format!("{} used", format::percent(fraction))));
            row.set_subtitle(&glib::markup_escape_text(&format!(
                "{base_subtitle} · {} free of {}",
                format::size(u.available),
                format::size(u.total)
            )));
        });
        row
    }
}
