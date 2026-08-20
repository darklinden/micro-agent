//! Stdout output helpers.
//!
//! Stdout is the *user-facing* channel and stays a readable event transcript:
//! model stream is labelled per turn block (`[user #NNN]`,
//! `[assistant #NNN think|answer]`, `[assistant #NNN run] ( tool args )`) so a
//! multi-turn run is easy to follow. All full request/response detail lives in
//! the log file (see [`crate::logger`]) — this only summarizes.

use crate::types::StreamEvent;
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

/// Write raw bytes straight to stdout and flush.
fn raw(s: &str) {
    let mut w = writer();
    if let Some(w) = w.as_mut() {
        let _ = w.write_all(s.as_bytes());
        let _ = w.flush();
    }
}

/// Serialize tool arguments to a compact JSON string, truncated with `…` if it
/// exceeds 200 characters.
fn compact_args(args: &serde_json::Value) -> String {
    let text = args.to_string();
    if text.len() > 200 {
        let mut s = text[..200].to_string();
        s.push('…');
        s
    } else {
        text
    }
}

/// The `#NNN` turn index, zero-padded to three digits.
pub fn turn_tag(n: u32) -> String {
    format!("{n:03}")
}

/// Print the user block that opens a run: the objective (and any later
/// user-role content). Header on its own line, content on the next.
pub fn user_block(n: u32, text: &str) {
    raw(&format!("\n[user #{}]\n{}\n", turn_tag(n), text));
}

/// Existing streaming primitive: append a chunk straight to stdout (no label).
/// The turn printer routes Think/Answer text here after printing its header.
pub fn text(s: &str) {
    raw(s);
}

/// Print a tool-run marker on its own line, e.g.
/// `[assistant #000 run] ( bash {"command":"..."} )`.
pub fn run_marker(n: u32, name: &str, args: &serde_json::Value) {
    raw(&format!(
        "\n[assistant #{} run] ( {} {} )\n",
        turn_tag(n),
        name,
        compact_args(args)
    ));
}

/// Which assistant block is currently open in the stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Think,
    Answer,
}

/// Label management for one assistant turn's streaming blocks. Opens a block
/// header the first time a token of that kind arrives, then streams subsequent
/// tokens of the same kind onto the same open line; switches blocks on a kind
/// change.
pub struct TurnPrinter {
    turn: u32,
    last: Option<Kind>,
}

impl TurnPrinter {
    pub fn new(turn: u32) -> Self {
        TurnPrinter { turn, last: None }
    }

    /// Print each streamed token behind the right block header.
    pub fn event(&mut self, ev: StreamEvent) {
        match ev {
            StreamEvent::Think(t) => self.open(Kind::Think, &t),
            StreamEvent::Answer(t) => self.open(Kind::Answer, &t),
        }
    }

    fn open(&mut self, kind: Kind, text: &str) {
        if text.is_empty() {
            return;
        }
        if self.last != Some(kind) {
            self.last = Some(kind);
            let label = match kind {
                Kind::Think => "think",
                Kind::Answer => "answer",
            };
            raw(&format!("\n[assistant #{} {}]\n", turn_tag(self.turn), label));
        }
        raw(text);
    }

    /// Close any still-open line so the turn ends cleanly.
    pub fn close(&mut self) {
        if self.last.is_some() {
            self.last = None;
            raw("\n");
        }
    }
}

/// Print a short banner line (e.g. error summary) to stdout.
pub fn banner(s: &str) {
    raw(&format!("{s}\n"));
}
