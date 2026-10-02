//! Console logging helpers (mirrors the output format of the original tool).

use std::io::{BufRead, IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};

static DEBUG: AtomicBool = AtomicBool::new(false);

const RED: &str = "\x1b[91m";
const GREEN: &str = "\x1b[92m";
const YELLOW: &str = "\x1b[93m";
const BLUE: &str = "\x1b[94m";
const MAGENTA: &str = "\x1b[95m";
const LMAGENTA: &str = "\x1b[35m";
const RESET: &str = "\x1b[0m";
const EMPH: &str = "\x1b[33m";
const BOLD: &str = "\x1b[1m";

pub fn set_debug(on: bool) {
    DEBUG.store(on, Ordering::Relaxed);
}

pub fn is_debug() -> bool {
    DEBUG.load(Ordering::Relaxed)
}

fn color(col: &'static str) -> &'static str {
    if std::io::stdout().is_terminal() {
        col
    } else {
        ""
    }
}

fn out(s: &str) {
    let mut o = std::io::stdout().lock();
    let _ = o.write_all(s.as_bytes());
    let _ = o.flush();
}

fn errout(s: &str) {
    let mut o = std::io::stderr().lock();
    let _ = o.write_all(s.as_bytes());
    let _ = o.flush();
}

pub fn log(s: &str) {
    out(&format!("{}{}\n{}", color(BLUE), s, color(RESET)));
}

pub fn log_bold(s: &str) {
    out(&format!(
        "{}{}{}{}\n{}",
        color(BLUE),
        color(BOLD),
        s,
        color(RESET),
        color(RESET)
    ));
}

pub fn sub(s: &str) {
    out(&format!("\t{}->{} {}\n", color(BLUE), color(RESET), s));
}

pub fn sub_noend(s: &str) {
    out(&format!("\t{}->{} {}", color(BLUE), color(RESET), s));
}

pub fn emph(s: &str) {
    out(&format!("{}{}{}", color(EMPH), s, color(RESET)));
}

pub fn err(s: impl std::fmt::Display) {
    errout(&format!("{}[ERR] {} \n{}", color(RED), s, color(RESET)));
}

pub fn warn(s: impl std::fmt::Display) {
    errout(&format!("{}[WARN] {} \n{}", color(YELLOW), s, color(RESET)));
}

pub fn dry(s: &str) {
    out(&format!("{}[DRY] {} \n{}", color(GREEN), s, color(RESET)));
}

pub fn raw(s: &str) {
    out(&format!("{s}\n"));
}

pub fn dbg_at(module: &str, s: &str) {
    errout(&format!(
        "{}{}[DEBUG][{}]{}{} {}{}\n",
        color(BOLD),
        color(LMAGENTA),
        module,
        color(RESET),
        color(MAGENTA),
        s,
        color(RESET)
    ));
}

/// ask the user for confirmation (only "y" is accepted)
pub fn ask(query: &str) -> bool {
    out(&format!(
        "{}{} [y/N] ? {}",
        color(BLUE),
        query,
        color(RESET)
    ));
    let mut line = String::new();
    match std::io::stdin().lock().read_line(&mut line) {
        Ok(n) if n > 0 => line.trim_end_matches(['\n', '\r']) == "y",
        _ => false,
    }
}

/// prompt for free text input
pub fn input(prompt: &str) -> String {
    out(prompt);
    let mut line = String::new();
    let _ = std::io::stdin().lock().read_line(&mut line);
    line.trim_end_matches(['\n', '\r']).to_string()
}

/// debug log, active only when debug mode is on
#[macro_export]
macro_rules! dbg_log {
    ($($arg:tt)*) => {
        if $crate::log::is_debug() {
            $crate::log::dbg_at(module_path!(), &format!($($arg)*));
        }
    };
}
