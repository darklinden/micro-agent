//! Session log: a strict JSONL event file per launch.
//!
//! `<MA_LOG_FILE_DIR>/<yyyyMMdd-HHmmss>.log` holds one JSON object per line.
//! Session-level facts (system prompt, tool table) are written once at startup
//! (`run_start`/`system`/`tools`); every later record is an incremental event
//! (`message`, `tool_call`, `gate`, …) — never a full re-dump — so the file
//! stays compact and machine-parseable. The depth-0 `message` events are the
//! replay source for `--context`: [`load_messages`] rebuilds the conversation
//! from any session log. Nothing here ever goes to stdout (see [`crate::out`]).

use crate::types::Message;
use anyhow::{Context, Result};
use serde_json::{json, Map, Value};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Severity of a log record; also the write threshold configured by
/// `MA_LOG_LEVEL`. Ordered so `Debug < Info < Warn < Error`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Debug,
    Info,
    Warn,
    Error,
}

impl Level {
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Debug => "debug",
            Level::Info => "info",
            Level::Warn => "warn",
            Level::Error => "error",
        }
    }

    /// Parse `MA_LOG_LEVEL`; unknown values fall back to `Info`.
    fn parse(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "debug" | "trace" => Level::Debug,
            "warn" | "warning" => Level::Warn,
            "error" => Level::Error,
            _ => Level::Info,
        }
    }
}

/// One open session log file plus its write threshold.
struct SessionLog {
    writer: BufWriter<File>,
    level: Level,
}

impl SessionLog {
    /// Create `<dir>/<ts>.log` (parent dirs included).
    fn create(dir: &Path, level: Level) -> std::io::Result<(Self, PathBuf)> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join(format!("{}.log", ts()));
        let file = File::create(&path)?;
        Ok((
            SessionLog {
                writer: BufWriter::new(file),
                level,
            },
            path,
        ))
    }

    /// Append one event line if `level` passes the threshold.
    fn write(&mut self, level: Level, ev: &str, fields: Value) {
        if level < self.level {
            return;
        }
        let mut obj = Map::new();
        obj.insert("v".into(), json!(1));
        obj.insert("ts".into(), json!(chrono::Local::now().to_rfc3339()));
        obj.insert("level".into(), json!(level.as_str()));
        obj.insert("ev".into(), json!(ev));
        if let Value::Object(map) = fields {
            obj.extend(map);
        }
        // Errors are swallowed: losing a log line must never kill a run.
        let line = serde_json::to_string(&Value::Object(obj));
        let _ = line.map(|l| writeln!(self.writer, "{l}"));
        let _ = self.writer.flush();
    }
}

/// The process-wide session log; `None` until `init` succeeds and stays `None`
/// when no `MA_LOG_FILE_DIR` is configured (emit then becomes a no-op).
static LOG: Mutex<Option<SessionLog>> = Mutex::new(None);

/// Local timestamp matching plan-file naming: `yyyymmdd-hhmmss`.
fn ts() -> String {
    chrono::Local::now().format("%Y%m%d-%H%M%S").to_string()
}

/// Initialise the session log under `dir`. Returns the created file's path for
/// the stdout `[log] …` banner, or `None` when no dir is configured or the
/// file cannot be created (not fatal — logging is best-effort).
pub fn init(dir: Option<&PathBuf>, level: &str) -> Option<PathBuf> {
    let dir = dir?;
    let lv = Level::parse(level);
    match SessionLog::create(dir, lv) {
        Ok((log, path)) => {
            *LOG.lock().unwrap() = Some(log);
            Some(path)
        }
        Err(e) => {
            eprintln!("warning: failed to create session log in {}: {e}", dir.display());
            None
        }
    }
}

/// Whether records at `level` would currently be written (lets callers skip
/// building expensive debug-only payloads).
pub fn enabled(level: Level) -> bool {
    let guard = LOG.lock().unwrap();
    guard.as_ref().is_some_and(|l| level >= l.level)
}

/// Emit one structured event. `fields` should be a JSON object; non-object
/// values are dropped (the common fields still identify the record).
pub fn emit(level: Level, ev: &str, fields: Value) {
    let mut guard = LOG.lock().unwrap();
    if let Some(log) = guard.as_mut() {
        log.write(level, ev, fields);
    }
}

/// Rebuild the top-level conversation from a session log: the `message`
/// events with `depth == 0`, in file order. Sub-agent (depth > 0) messages are
/// skipped — they live inside the parent's tool results already.
pub fn load_messages(path: &Path) -> Result<Vec<Message>> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let v: Value = serde_json::from_str(line)
            .with_context(|| format!("{}:{}: not valid JSON", path.display(), i + 1))?;
        if v.get("ev").and_then(|e| e.as_str()) != Some("message") {
            continue;
        }
        if v.get("depth").and_then(|d| d.as_u64()).unwrap_or(0) != 0 {
            continue;
        }
        let msg: Message = serde_json::from_value(v.get("msg").cloned().unwrap_or(Value::Null))
            .with_context(|| format!("{}:{}: bad message record", path.display(), i + 1))?;
        out.push(msg);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::{Level, SessionLog};
    use crate::types::{ContentBlock, Message, Role};
    use serde_json::{json, Value};
    use std::path::PathBuf;

    fn tmp_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("ma-sesslog-{}-{tag}", std::process::id()))
    }

    /// A message equal to what `emit_message` would serialize.
    fn sample_messages() -> Vec<Message> {
        vec![
            Message {
                role: Role::User,
                blocks: vec![ContentBlock::Text("hello".into())],
            },
            Message {
                role: Role::Assistant,
                blocks: vec![
                    ContentBlock::Thinking {
                        thinking: "hmm".into(),
                        signature: Some("sig==".into()),
                    },
                    ContentBlock::ToolUse {
                        id: "t1".into(),
                        name: "bash".into(),
                        input: json!({"command": "ls"}),
                    },
                ],
            },
            Message {
                role: Role::User,
                blocks: vec![ContentBlock::ToolResult {
                    tool_use_id: "t1".into(),
                    content: "out".into(),
                    is_error: false,
                }],
            },
        ]
    }

    #[test]
    fn round_trip_replays_depth_zero_messages_in_order() {
        let dir = tmp_dir("roundtrip");
        let (mut log, path) = SessionLog::create(&dir, Level::Debug).unwrap();

        let msgs = sample_messages();
        for m in &msgs {
            log.write(
                Level::Info,
                "message",
                json!({"depth": 0u32, "msg": m}),
            );
        }
        // A sub-agent message must be filtered out on replay.
        log.write(
            Level::Info,
            "message",
            json!({"depth": 1u32, "msg": Message::user_text("nested")}),
        );
        // Non-message events are ignored too.
        log.write(Level::Info, "gate", json!({"command": "ls", "allow": true}));
        log.write(Level::Warn, "warn", json!({"message": "boom"}));
        drop(log); // flush + close before reading back

        let replayed = super::load_messages(&path).unwrap();
        assert_eq!(replayed.len(), msgs.len());
        for (got, want) in replayed.iter().zip(&msgs) {
            let g = serde_json::to_value(got).unwrap();
            let w = serde_json::to_value(want).unwrap();
            assert_eq!(g, w);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn level_threshold_filters_records() {
        let dir = tmp_dir("threshold");
        let (mut log, path) = SessionLog::create(&dir, Level::Warn).unwrap();
        log.write(Level::Debug, "request", json!({"n_msgs": 1})); // filtered
        log.write(Level::Info, "turn", json!({"status": "start"})); // filtered
        log.write(Level::Warn, "warn", json!({"message": "kept"}));
        log.write(Level::Error, "error", json!({"message": "kept"}));
        drop(log);

        let text = std::fs::read_to_string(&path).unwrap();
        let evs: Vec<String> = text
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter_map(|v| v["ev"].as_str().map(str::to_string))
            .collect();
        assert_eq!(evs, vec!["warn".to_string(), "error".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn every_line_is_a_complete_json_object_with_common_fields() {
        let dir = tmp_dir("shape");
        let (mut log, path) = SessionLog::create(&dir, Level::Debug).unwrap();
        log.write(Level::Info, "run_start", json!({"mode": "plan"}));
        drop(log);

        let text = std::fs::read_to_string(&path).unwrap();
        for line in text.lines() {
            let v: Value = serde_json::from_str(line).expect("each line parses");
            assert_eq!(v["v"], json!(1));
            assert!(v["ts"].is_string());
            assert_eq!(v["level"], json!("info"));
            assert_eq!(v["ev"], json!("run_start"));
            assert_eq!(v["mode"], json!("plan"));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_reports_bad_lines_with_line_number() {
        let dir = tmp_dir("badline");
        let path = dir.join("broken.log");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            &path,
            "{\"ev\":\"other\"}\nnot json at all\n",
        )
        .unwrap();
        let err = format!("{}", super::load_messages(&path).unwrap_err());
        assert!(err.contains("2"), "line number expected in: {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_log_loads_to_no_messages() {
        let dir = tmp_dir("empty");
        let path = dir.join("empty.log");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, "").unwrap();
        assert!(super::load_messages(&path).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_file_is_a_read_error() {
        let p = tmp_dir("missing").join("nope.log");
        assert!(super::load_messages(&p).is_err());
    }

    #[test]
    fn level_parses_known_names_and_defaults_to_info() {
        assert_eq!(Level::parse("debug"), Level::Debug);
        assert_eq!(Level::parse("TRACE"), Level::Debug);
        assert_eq!(Level::parse("warning"), Level::Warn);
        assert_eq!(Level::parse("error"), Level::Error);
        assert_eq!(Level::parse("info"), Level::Info);
        assert_eq!(Level::parse("nonsense"), Level::Info);
        assert!(Level::Debug < Level::Info && Level::Info < Level::Warn);
    }
}
