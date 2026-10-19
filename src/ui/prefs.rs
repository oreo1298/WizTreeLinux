//! Preferences dialog.

use crate::format;
use crate::scanner;
use crate::settings::{self, ColorScheme, TreemapStyle};
use crate::tree::SizeMode;
use adw::prelude::*;

fn combo(title: &str, subtitle: &str, items: &[&str], selected: u32) -> adw::ComboRow {
    let row = adw::ComboRow::builder().title(title).model(&gtk::StringList::new(items)).selected(selected).build();
    if !subtitle.is_empty() {
        row.set_subtitle(subtitle);
    }
    row
}

fn changed() {
    let s = settings::get();
    format::set_decimal_units(s.decimal_units);
    super::apply_color_scheme();
    super::window::for_each_window(|w| w.apply_settings());
}

pub fn show(parent: &impl IsA<gtk::Widget>) {
    let s = settings::get();
    let page = adw::PreferencesPage::builder().title("General").icon_name("preferences-system-symbolic").build();

    let appearance = adw::PreferencesGroup::builder().title("Appearance").build();
    let style = combo(
        "Style",
        "",
        &["Follow System", "Light", "Dark"],
        match s.color_scheme {
            ColorScheme::System => 0,
            ColorScheme::Light => 1,
            ColorScheme::Dark => 2,
        },
    );
    style.connect_selected_notify(|row| {
        let scheme = match row.selected() {
            1 => ColorScheme::Light,
            2 => ColorScheme::Dark,
            _ => ColorScheme::System,
        };
        settings::update(|s| s.color_scheme = scheme);
        changed();
    });
    let tm_style = combo(
        "Treemap Style",
        "",
        &["Cushions", "Flat"],
        match s.treemap_style {
            TreemapStyle::Cushion => 0,
            TreemapStyle::Flat => 1,
        },
    );
    tm_style.connect_selected_notify(|row| {
        let style = if row.selected() == 1 { TreemapStyle::Flat } else { TreemapStyle::Cushion };
        settings::update(|s| s.treemap_style = style);
        changed();
    });
    appearance.add(&style);
    appearance.add(&tm_style);

    let sizes = adw::PreferencesGroup::builder().title("Sizes").build();
    let measure = combo(
        "Measure By",
        "Used for sorting, percentages and the treemap",
        &["Space on Disk", "File Size"],
        match s.size_mode {
            SizeMode::Allocated => 0,
            SizeMode::Logical => 1,
        },
    );
    measure.connect_selected_notify(|row| {
        let mode = if row.selected() == 1 { SizeMode::Logical } else { SizeMode::Allocated };
        settings::update(|s| s.size_mode = mode);
        changed();
    });
    let units = combo("Units", "", &["Binary (KiB, MiB, GiB)", "Decimal (kB, MB, GB)"], u32::from(s.decimal_units));
    units.connect_selected_notify(|row| {
        let decimal = row.selected() == 1;
        settings::update(|s| s.decimal_units = decimal);
        changed();
    });
    sizes.add(&measure);
    sizes.add(&units);

    let scanning = adw::PreferencesGroup::builder().title("Scanning").build();
    let one_fs = adw::SwitchRow::builder()
        .title("Stay on One Filesystem")
        .subtitle("Skip other drives mounted inside the scanned folder")
        .active(s.one_filesystem)
        .build();
    one_fs.connect_active_notify(|row| {
        let v = row.is_active();
        settings::update(|s| s.one_filesystem = v);
        changed();
    });
    let threads = adw::SpinRow::with_range(0.0, 256.0, 1.0);
    threads.set_title("Worker Threads");
    threads.set_subtitle(&format!("0 chooses automatically ({} on this computer)", scanner::default_threads()));
    threads.set_value(s.threads as f64);
    threads.connect_value_notify(|row| {
        let v = row.value().round().max(0.0) as usize;
        settings::update(|s| s.threads = v);
    });
    scanning.add(&one_fs);
    scanning.add(&threads);

    page.add(&appearance);
    page.add(&sizes);
    page.add(&scanning);

    let dialog = adw::PreferencesDialog::new();
    dialog.add(&page);
    dialog.present(Some(parent));
}
