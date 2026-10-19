//! Human-readable numbers.

use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::time::Duration;

static DECIMAL_UNITS: AtomicBool = AtomicBool::new(false);

/// Switches between binary (KiB, MiB, ...) and decimal (kB, MB, ...) units.
pub fn set_decimal_units(decimal: bool) {
    DECIMAL_UNITS.store(decimal, Relaxed);
}

pub fn size(bytes: u64) -> String {
    let decimal = DECIMAL_UNITS.load(Relaxed);
    let (base, units): (f64, [&str; 7]) = if decimal {
        (1000.0, ["B", "kB", "MB", "GB", "TB", "PB", "EB"])
    } else {
        (1024.0, ["B", "KiB", "MiB", "GiB", "TiB", "PiB", "EiB"])
    };
    if (bytes as f64) < base {
        return format!("{bytes} B");
    }
    let mut v = bytes as f64;
    let mut unit = 0;
    while v >= base && unit < units.len() - 1 {
        v /= base;
        unit += 1;
    }
    // Three significant digits, like most file managers.
    if v < 9.995 {
        format!("{v:.2} {}", units[unit])
    } else if v < 99.95 {
        format!("{v:.1} {}", units[unit])
    } else {
        format!("{v:.0} {}", units[unit])
    }
}

/// Groups digits with thin spaces: `1 234 567`.
pub fn count(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3 * 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push('\u{202F}');
        }
        out.push(c);
    }
    out
}

pub fn percent(fraction: f64) -> String {
    let p = (fraction * 100.0).clamp(0.0, 100.0);
    if p > 0.0 && p < 0.1 {
        "<0.1 %".to_string()
    } else {
        format!("{p:.1} %")
    }
}

pub fn duration(d: Duration) -> String {
    let s = d.as_secs_f64();
    if s < 10.0 {
        format!("{s:.2} s")
    } else if s < 60.0 {
        format!("{s:.1} s")
    } else {
        let total = s.round() as u64;
        format!("{} min {} s", total / 60, total % 60)
    }
}

pub fn plural<'a>(n: u64, one: &'a str, many: &'a str) -> String {
    format!("{} {}", count(n), if n == 1 { one } else { many })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        set_decimal_units(false);
        assert_eq!(size(0), "0 B");
        assert_eq!(size(1023), "1023 B");
        assert_eq!(size(1024), "1.00 KiB");
        assert_eq!(size(1536 * 1024), "1.50 MiB");
        assert_eq!(size(45 * 1024 * 1024 * 1024), "45.0 GiB");
        assert_eq!(size(512 * 1024 * 1024 * 1024), "512 GiB");
        set_decimal_units(true);
        assert_eq!(size(1_500_000_000), "1.50 GB");
        set_decimal_units(false);
    }

    #[test]
    fn counts() {
        assert_eq!(count(0), "0");
        assert_eq!(count(999), "999");
        assert_eq!(count(1000), "1\u{202F}000");
        assert_eq!(count(1234567), "1\u{202F}234\u{202F}567");
        assert_eq!(percent(0.4567), "45.7 %");
        assert_eq!(percent(0.0001), "<0.1 %");
        assert_eq!(plural(1, "file", "files"), "1 file");
    }
}
