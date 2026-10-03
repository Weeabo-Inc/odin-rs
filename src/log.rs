//! Minimal, dependency-free leveled logger for the probe.
//!
//! Verbosity comes from `ODIN_VERBOSE` (0..=4), default 3.
//! libusb's own internal log is bridged in through rusb's `log` feature-less
//! callback (`Context::set_log_level`), so everything is captured.

use std::sync::atomic::{AtomicU8, Ordering};

pub const ERROR: u8 = 1;
pub const WARN: u8 = 2;
pub const INFO: u8 = 3;
pub const DEBUG: u8 = 4;
pub const TRACE: u8 = 5;

static LEVEL: AtomicU8 = AtomicU8::new(INFO);

pub fn init_from_env() {
    let lvl = match std::env::var("ODIN_VERBOSE").ok().and_then(|v| v.parse::<u8>().ok()) {
        Some(0) => 0,
        Some(n) => n.min(TRACE),
        None => INFO,
    };
    LEVEL.store(lvl, Ordering::Relaxed);
}

pub fn level() -> u8 {
    LEVEL.load(Ordering::Relaxed)
}

pub fn enabled(l: u8) -> bool {
    level() >= l
}

pub fn banner(title: &str) {
    println!("\n========================================================================");
    println!("  {title}");
    println!("========================================================================");
}

pub fn step(n: u32, total: u32, title: &str) {
    let trail = "-".repeat(48usize.saturating_sub(title.len()));
    println!("\n---- step {n}/{total}: {title} {trail}");
}

/// Always printed (results the operator must see).
#[allow(dead_code)]
pub fn result(msg: &str) {
    println!("[result] {msg}");
}

pub fn ok(msg: &str) {
    println!("[  OK  ] {msg}");
}

pub fn fail(msg: &str) {
    println!("[ FAIL ] {msg}");
}

pub fn info(msg: &str) {
    if enabled(INFO) {
        println!("[info ] {msg}");
    }
}

pub fn warn(msg: &str) {
    if enabled(WARN) {
        eprintln!("[warn ] {msg}");
    }
}

pub fn error(msg: &str) {
    if enabled(ERROR) {
        eprintln!("[error] {msg}");
    }
}

pub fn debug(msg: &str) {
    if enabled(DEBUG) {
        println!("[debug] {msg}");
    }
}

#[allow(dead_code)]
pub fn trace(msg: &str) {
    if enabled(TRACE) {
        println!("[trace] {msg}");
    }
}

/// `12 34 ab cd  |....|`
pub fn hexdump(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len() * 4 + 16);
    for chunk in data.chunks(16) {
        for b in chunk {
            out.push_str(&format!("{b:02x} "));
        }
        for _ in chunk.len()..16 {
            out.push_str("   ");
        }
        out.push_str(" |");
        for b in chunk {
            out.push(if (0x20..0x7f).contains(b) { *b as char } else { '.' });
        }
        out.push_str("|\n");
    }
    out.trim_end().to_string()
}

pub fn hex(data: &[u8]) -> String {
    data.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" ")
}

pub fn ascii(data: &[u8]) -> String {
    data.iter().map(|b| if (0x20..0x7f).contains(b) { *b as char } else { '.' }).collect()
}
