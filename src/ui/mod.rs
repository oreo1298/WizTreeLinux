//! The GTK 4 / libadwaita user interface.

mod columns;
mod objects;
mod prefs;
mod results;
mod scan_page;
mod start_page;
mod treemap_widget;
mod window;

use crate::{format, settings};
use adw::prelude::*;
use gtk::{gio, glib};
use std::ffi::OsString;
use window::Window;

pub const APP_ID: &str = "io.github.oreo1298.Dendro";
pub const APP_NAME: &str = "Dendro";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The application icon, or a stock icon when running uninstalled
/// without the icon being found.
pub fn app_icon_name() -> &'static str {
    let found = gtk::gdk::Display::default().map(|d| gtk::IconTheme::for_display(&d).has_icon(APP_ID)).unwrap_or(false);
    if found {
        APP_ID
    } else {
        "drive-harddisk-symbolic"
    }
}

pub fn apply_color_scheme() {
    let scheme = match settings::get().color_scheme {
        settings::ColorScheme::System => adw::ColorScheme::Default,
        settings::ColorScheme::Light => adw::ColorScheme::ForceLight,
        settings::ColorScheme::Dark => adw::ColorScheme::ForceDark,
    };
    adw::StyleManager::default().set_color_scheme(scheme);
}

fn load_css() {
    let provider = gtk::CssProvider::new();
    provider.load_from_string(include_str!("style.css"));
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(&display, &provider, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
        // Debug builds run from the source tree: find the icon next to the sources.
        #[cfg(debug_assertions)]
        {
            let dev_icons = concat!(env!("CARGO_MANIFEST_DIR"), "/data/icons");
            if std::path::Path::new(dev_icons).is_dir() {
                gtk::IconTheme::for_display(&display).add_search_path(dev_icons);
            }
        }
    }
}

fn show_about(parent: &gtk::Window) {
    let about = adw::AboutDialog::builder()
        .application_name(APP_NAME)
        .application_icon(app_icon_name())
        .version(VERSION)
        .developer_name("The Dendro contributors")
        .comments("A fast disk space analyzer for Linux, in the spirit of WizTree.\n\nScans any filesystem in parallel, shows every file including hidden ones, and draws a treemap of what uses your space.")
        .website("https://github.com/oreo1298/WizTreeLinux")
        .issue_url("https://github.com/oreo1298/WizTreeLinux/issues")
        .license_type(gtk::License::MitX11)
        .build();
    about.present(Some(parent));
}

const SHORTCUTS_UI: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<interface>
  <object class="GtkShortcutsWindow" id="shortcuts">
    <property name="modal">true</property>
    <child>
      <object class="GtkShortcutsSection">
        <property name="section-name">shortcuts</property>
        <child>
          <object class="GtkShortcutsGroup">
            <property name="title">Scanning</property>
            <child><object class="GtkShortcutsShortcut"><property name="title">Scan a folder</property><property name="accelerator">&lt;Primary&gt;o</property></object></child>
            <child><object class="GtkShortcutsShortcut"><property name="title">Scan again</property><property name="accelerator">F5 &lt;Primary&gt;r</property></object></child>
            <child><object class="GtkShortcutsShortcut"><property name="title">Back to locations</property><property name="accelerator">&lt;Primary&gt;n</property></object></child>
            <child><object class="GtkShortcutsShortcut"><property name="title">Search files</property><property name="accelerator">&lt;Primary&gt;f</property></object></child>
          </object>
        </child>
        <child>
          <object class="GtkShortcutsGroup">
            <property name="title">Selected item</property>
            <child><object class="GtkShortcutsShortcut"><property name="title">Open</property><property name="accelerator">&lt;Primary&gt;Return</property></object></child>
            <child><object class="GtkShortcutsShortcut"><property name="title">Open containing folder</property><property name="accelerator">&lt;Primary&gt;&lt;Shift&gt;Return</property></object></child>
            <child><object class="GtkShortcutsShortcut"><property name="title">Copy path</property><property name="accelerator">&lt;Primary&gt;c</property></object></child>
            <child><object class="GtkShortcutsShortcut"><property name="title">Move to trash</property><property name="accelerator">Delete</property></object></child>
            <child><object class="GtkShortcutsShortcut"><property name="title">Delete permanently</property><property name="accelerator">&lt;Shift&gt;Delete</property></object></child>
          </object>
        </child>
        <child>
          <object class="GtkShortcutsGroup">
            <property name="title">General</property>
            <child><object class="GtkShortcutsShortcut"><property name="title">New window</property><property name="accelerator">&lt;Primary&gt;&lt;Shift&gt;n</property></object></child>
            <child><object class="GtkShortcutsShortcut"><property name="title">Preferences</property><property name="accelerator">&lt;Primary&gt;comma</property></object></child>
            <child><object class="GtkShortcutsShortcut"><property name="title">Keyboard shortcuts</property><property name="accelerator">&lt;Primary&gt;question</property></object></child>
            <child><object class="GtkShortcutsShortcut"><property name="title">Close window</property><property name="accelerator">&lt;Primary&gt;w</property></object></child>
            <child><object class="GtkShortcutsShortcut"><property name="title">Quit</property><property name="accelerator">&lt;Primary&gt;q</property></object></child>
          </object>
        </child>
      </object>
    </child>
  </object>
</interface>"#;

fn show_shortcuts(parent: &gtk::Window) {
    let builder = gtk::Builder::from_string(SHORTCUTS_UI);
    if let Some(win) = builder.object::<gtk::Window>("shortcuts") {
        win.set_transient_for(Some(parent));
        win.present();
    }
}

fn setup_actions(app: &adw::Application) {
    let add = |name: &str, f: fn(&adw::Application)| {
        let action = gio::SimpleAction::new(name, None);
        let weak = app.downgrade();
        action.connect_activate(move |_, _| {
            if let Some(app) = weak.upgrade() {
                f(&app);
            }
        });
        app.add_action(&action);
    };
    add("quit", |app| {
        for w in app.windows() {
            w.close();
        }
        app.quit();
    });
    add("new-window", |app| Window::new(app).present());
    add("preferences", |app| {
        if let Some(w) = app.active_window() {
            prefs::show(&w);
        }
    });
    add("about", |app| {
        if let Some(w) = app.active_window() {
            show_about(&w);
        }
    });
    add("shortcuts", |app| {
        if let Some(w) = app.active_window() {
            show_shortcuts(&w);
        }
    });

    for (action, accels) in [
        ("app.quit", &["<Primary>q"][..]),
        ("app.new-window", &["<Primary><Shift>n"]),
        ("app.preferences", &["<Primary>comma"]),
        ("app.shortcuts", &["<Primary>question"]),
        ("win.home", &["<Primary>n", "<Alt>Home"]),
        ("win.rescan", &["F5", "<Primary>r"]),
        ("win.open-folder", &["<Primary>o"]),
        ("win.search", &["<Primary>f"]),
        ("win.close", &["<Primary>w"]),
    ] {
        app.set_accels_for_action(action, accels);
    }
}

pub fn run(args: Vec<OsString>) -> glib::ExitCode {
    format::set_decimal_units(settings::get().decimal_units);
    let app = adw::Application::builder().application_id(APP_ID).flags(gio::ApplicationFlags::HANDLES_OPEN).build();
    app.connect_startup(|app| {
        gtk::Window::set_default_icon_name(APP_ID);
        load_css();
        apply_color_scheme();
        setup_actions(app);
    });
    app.connect_activate(|app| {
        if let Some(w) = app.active_window() {
            w.present();
        } else {
            Window::new(app).present();
        }
    });
    app.connect_open(|app, files, _| {
        for file in files {
            let w = Window::new(app);
            w.present();
            if let Some(path) = file.path() {
                w.scan_path(path);
            }
        }
    });
    let args: Vec<String> = args.iter().map(|a| a.to_string_lossy().into_owned()).collect();
    app.run_with_args(&args)
}
