//! User preferences, stored as `key=value` lines in
//! `$XDG_CONFIG_HOME/dendro/settings.conf`.

use crate::tree::SizeMode;
use std::cell::RefCell;
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ColorScheme {
    #[default]
    System,
    Light,
    Dark,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum TreemapStyle {
    #[default]
    Cushion,
    Flat,
}

#[derive(Clone, Debug)]
pub struct Settings {
    pub color_scheme: ColorScheme,
    pub size_mode: SizeMode,
    pub decimal_units: bool,
    pub treemap_style: TreemapStyle,
    pub one_filesystem: bool,
    pub elevated: bool,
    pub threads: usize,
    pub window_width: i32,
    pub window_height: i32,
    pub maximized: bool,
    /// Height of the lists above the treemap, as a fraction of the window.
    pub split_vertical: f64,
    /// Width of the folder tree next to the file types, as a fraction.
    pub split_horizontal: f64,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            color_scheme: ColorScheme::System,
            size_mode: SizeMode::Allocated,
            decimal_units: false,
            treemap_style: TreemapStyle::Cushion,
            one_filesystem: true,
            elevated: false,
            threads: 0,
            window_width: 1280,
            window_height: 820,
            maximized: false,
            split_vertical: 0.48,
            split_horizontal: 0.7,
        }
    }
}

fn config_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("dendro").join("settings.conf"))
}

impl Settings {
    fn load() -> Settings {
        let mut s = Settings::default();
        let Some(text) = config_path().and_then(|p| std::fs::read_to_string(p).ok()) else {
            return s;
        };
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else { continue };
            let (k, v) = (k.trim(), v.trim());
            let flag = v == "true";
            match k {
                "color_scheme" => {
                    s.color_scheme = match v {
                        "light" => ColorScheme::Light,
                        "dark" => ColorScheme::Dark,
                        _ => ColorScheme::System,
                    }
                }
                "size_mode" => s.size_mode = if v == "logical" { SizeMode::Logical } else { SizeMode::Allocated },
                "decimal_units" => s.decimal_units = flag,
                "treemap_style" => {
                    s.treemap_style = if v == "flat" { TreemapStyle::Flat } else { TreemapStyle::Cushion }
                }
                "one_filesystem" => s.one_filesystem = flag,
                "elevated" => s.elevated = flag,
                "threads" => s.threads = v.parse().unwrap_or(0).min(256),
                "window_width" => s.window_width = v.parse().unwrap_or(s.window_width).clamp(360, 16384),
                "window_height" => s.window_height = v.parse().unwrap_or(s.window_height).clamp(360, 16384),
                "maximized" => s.maximized = flag,
                "split_vertical" => s.split_vertical = v.parse().unwrap_or(s.split_vertical).clamp(0.1, 0.9),
                "split_horizontal" => s.split_horizontal = v.parse().unwrap_or(s.split_horizontal).clamp(0.1, 0.9),
                _ => {}
            }
        }
        s
    }

    fn save(&self) {
        let Some(path) = config_path() else { return };
        let text = format!(
            "color_scheme={}\nsize_mode={}\ndecimal_units={}\ntreemap_style={}\none_filesystem={}\nelevated={}\nthreads={}\nwindow_width={}\nwindow_height={}\nmaximized={}\nsplit_vertical={:.3}\nsplit_horizontal={:.3}\n",
            match self.color_scheme {
                ColorScheme::System => "system",
                ColorScheme::Light => "light",
                ColorScheme::Dark => "dark",
            },
            match self.size_mode {
                SizeMode::Allocated => "allocated",
                SizeMode::Logical => "logical",
            },
            self.decimal_units,
            match self.treemap_style {
                TreemapStyle::Cushion => "cushion",
                TreemapStyle::Flat => "flat",
            },
            self.one_filesystem,
            self.elevated,
            self.threads,
            self.window_width,
            self.window_height,
            self.maximized,
            self.split_vertical,
            self.split_horizontal,
        );
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let tmp = path.with_extension("conf.tmp");
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
}

thread_local! {
    static SETTINGS: RefCell<Settings> = RefCell::new(Settings::load());
}

/// Current settings (UI thread only).
pub fn get() -> Settings {
    SETTINGS.with(|s| s.borrow().clone())
}

/// Changes settings and writes them to disk.
pub fn update(f: impl FnOnce(&mut Settings)) {
    SETTINGS.with(|s| {
        let mut s = s.borrow_mut();
        f(&mut s);
        s.save();
    });
}
