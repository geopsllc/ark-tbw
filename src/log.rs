//! Minimal logging. Under systemd (JOURNAL_STREAM set) lines carry sd-daemon priority
//! prefixes and no timestamp, since journald adds its own.

use std::sync::OnceLock;

static JOURNAL: OnceLock<bool> = OnceLock::new();

pub fn write(level: u8, tag: &str, msg: std::fmt::Arguments) {
    let journal = *JOURNAL.get_or_init(|| std::env::var_os("JOURNAL_STREAM").is_some());
    if journal {
        eprintln!("<{level}>{tag}{msg}");
    } else {
        let now = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S");
        eprintln!("{now} {tag}{msg}");
    }
}

#[macro_export]
macro_rules! info {
    ($($t:tt)*) => { $crate::log::write(6, "", format_args!($($t)*)) };
}
#[macro_export]
macro_rules! warn {
    ($($t:tt)*) => { $crate::log::write(4, "WARN ", format_args!($($t)*)) };
}
#[macro_export]
macro_rules! error {
    ($($t:tt)*) => { $crate::log::write(3, "ERROR ", format_args!($($t)*)) };
}
