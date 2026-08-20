//! Stdout output helpers.
//!
//! Stdout is the *user-facing* channel and stays clean: it carries only the
//! model's streamed text plus compact tool marks. All full request/response
//! detail lives in the log file (see [`crate::logger`]).

use std::io::{BufWriter, Write};
use std::sync::Mutex;

static OUT: Mutex<Option<BufWriter<std::io::Stdout>>> = Mutex::new(None);

/// Lazily obtain the shared, mutex-guarded stdout writer.
fn writer() -> std::sync::MutexGuard<'static, Option<BufWriter<std::io::Stdout>>> {
    let mut guard = OUT.lock().unwrap();
    if guard.is_none() {
        *guard = Some(BufWriter::new(std::io::stdout()));
    }
    guard
}

/// Flush a streamed text chunk straight to stdout. Used for model text.
pub fn text(s: &str) {
    let mut w = writer();
    if let Some(w) = w.as_mut() {
        let _ = w.write_all(s.as_bytes());
        let _ = w.flush();
    }
}

/// Print a compact tool mark on its own line, e.g.
/// `⧗ read_file ["foo.rs"]`.
pub fn tool_mark(name: &str, args: &serde_json::Value) {
    let mut w = writer();
    if let Some(w) = w.as_mut() {
        let arg_text = args.to_string();
        let trimmed = if arg_text.len() > 200 {
            let mut s = arg_text[..200].to_string();
            s.push('…');
            s
        } else {
            arg_text
        };
        let _ = writeln!(w, "\n⧗ {name} {trimmed}");
        let _ = w.flush();
    }
}

/// Print a short banner line (e.g. error summary) to stdout.
pub fn banner(s: &str) {
    let mut w = writer();
    if let Some(w) = w.as_mut() {
        let _ = writeln!(w, "{s}");
        let _ = w.flush();
    }
}
