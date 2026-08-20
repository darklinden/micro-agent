//! The eight built-in tools plus their JSON-Schema definitions.
//!
//! Implementations are deliberately minimal: they take `serde_json::Value`
//! arguments and return a string result, so the turn loop stays provider-agnostic.

use std::path::PathBuf;

use serde_json::{json, Value};

use super::ToolOutput;
use crate::config::Config;
use crate::types::ToolDef;

/// Name of the bash tool (the only one gated).
pub const BASH: &str = "bash";

fn def(name: &str, description: &str, input_schema: Value) -> ToolDef {
    ToolDef {
        name: name.to_string(),
        description: description.to_string(),
        input_schema,
    }
}

/// All built-in tool definitions exposed to the model.
pub fn builtin_defs() -> Vec<ToolDef> {
    vec![
        def(
            "read_file",
            "Read the contents of a file at the given path. Returns the file text (truncated if very large).",
            json!({"type":"object","properties":{"path":{"type":"string","description":"Absolute or relative path to read"}},"required":["path"]}),
        ),
        def(
            "write_file",
            "Write `content` to the file at `path`, creating parent directories as needed. Overwrites any existing file.",
            json!({"type":"object","properties":{"path":{"type":"string"},"content":{"type":"string"}},"required":["path","content"]}),
        ),
        def(
            "edit_file",
            "Replace the first occurrence of `old_string` with `new_string` in the file at `path`.",
            json!({"type":"object","properties":{"path":{"type":"string"},"old_string":{"type":"string"},"new_string":{"type":"string"}},"required":["path","old_string","new_string"]}),
        ),
        def(
            "grep",
            "Search files for lines containing `pattern` (substring match). `path` is a file or directory to search (default: current directory); `file_glob` optionally filters file names by substring.",
            json!({"type":"object","properties":{"pattern":{"type":"string"},"path":{"type":"string"},"file_glob":{"type":"string"}},"required":["pattern"]}),
        ),
        def(
            "glob",
            "Expand a glob pattern (e.g. `**/*.rs`) against the filesystem, returning matching paths.",
            json!({"type":"object","properties":{"pattern":{"type":"string"}},"required":["pattern"]}),
        ),
        def(
            BASH,
            "Run a shell command and return its combined stdout+stderr. Use for operations that the other tools cannot express.",
            json!({"type":"object","properties":{"command":{"type":"string"}},"required":["command"]}),
        ),
        def(
            "task",
            "Record an explicit sub-task or step. The agent uses this to outline its plan before executing. Returns an acknowledgement.",
            json!({"type":"object","properties":{"task":{"type":"string","description":"Description of the sub-task / step"}},"required":["task"]}),
        ),
        def(
            "web_fetch",
            "Fetch the text content of a URL over HTTPS. Returns the response body text (truncated).",
            json!({"type":"object","properties":{"url":{"type":"string"},"max_bytes":{"type":"integer","description":"Optional cap on bytes to read"}},"required":["url"]}),
        ),
    ]
}

/// Execute a built-in tool. Returns `None` if the name is not a built-in.
pub async fn run(name: &str, args: &Value, cfg: &Config) -> Option<ToolOutput> {
    let _ = cfg;
    match name {
        "read_file" => Some(read_file(args)),
        "write_file" => Some(write_file(args)),
        "edit_file" => Some(edit_file(args)),
        "grep" => Some(grep(args)),
        "glob" => Some(glob(args)),
        BASH => Some(bash(args).await),
        "task" => Some(task(args)),
        "web_fetch" => Some(web_fetch(args).await),
        _ => None,
    }
}

fn trunc(s: &str, max: usize) -> String {
    crate::upstream::truncate(s, max)
}

fn arg<'a>(args: &'a Value, key: &str) -> &'a str {
    args.get(key).and_then(|v| v.as_str()).unwrap_or_default()
}

fn ok(content: String) -> ToolOutput {
    ToolOutput {
        content,
        is_error: false,
    }
}

fn err(content: String) -> ToolOutput {
    ToolOutput {
        content,
        is_error: true,
    }
}

fn read_file(args: &Value) -> ToolOutput {
    let path = arg(args, "path");
    let p = PathBuf::from(path);
    match std::fs::read_to_string(&p) {
        Ok(text) => ok(trunc(&text, 256 * 1024)),
        Err(e) => err(format!("read failed: {e}")),
    }
}

fn write_file(args: &Value) -> ToolOutput {
    let path = arg(args, "path");
    let content = arg(args, "content");
    let p = PathBuf::from(path);
    if let Some(parent) = p.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match std::fs::write(&p, content) {
        Ok(()) => ok(format!("wrote {} bytes to {}", content.len(), p.display())),
        Err(e) => err(format!("write failed: {e}")),
    }
}

fn edit_file(args: &Value) -> ToolOutput {
    let path = arg(args, "path");
    let old = arg(args, "old_string");
    let new = arg(args, "new_string");
    let p = PathBuf::from(path);
    let text = match std::fs::read_to_string(&p) {
        Ok(t) => t,
        Err(e) => return err(format!("read failed: {e}")),
    };
    if old.is_empty() {
        return err("old_string must not be empty".into());
    }
    match text.find(old) {
        Some(idx) => {
            let mut out = text.clone();
            out.replace_range(idx..idx + old.len(), new);
            match std::fs::write(&p, &out) {
                Ok(()) => ok(format!("edited {}", p.display())),
                Err(e) => err(format!("write failed: {e}")),
            }
        }
        None => err(format!("old_string not found in {}", p.display())),
    }
}

fn grep(args: &Value) -> ToolOutput {
    use std::io::BufRead;
    let pattern = arg(args, "pattern");
    if pattern.is_empty() {
        return err("pattern must not be empty".into());
    }
    let base = if arg(args, "path").is_empty() {
        PathBuf::from(".")
    } else {
        PathBuf::from(arg(args, "path"))
    };
    let file_glob = arg(args, "file_glob");
    let mut hits: Vec<String> = Vec::new();
    let max = 200usize;

    let mut stack = vec![base];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_dir() {
                stack.push(path);
            } else if ft.is_file() {
                // Allow symlink-pointed or fifo? Only regular files.
                if !file_glob.is_empty() && !path.to_string_lossy().contains(file_glob) {
                    continue;
                }
                let Ok(file) = std::fs::File::open(&path) else {
                    continue;
                };
                let reader = std::io::BufReader::new(file);
                for (i, line) in reader.lines().enumerate() {
                    let Ok(line) = line else { break };
                    if line.contains(pattern) {
                        hits.push(format!("{}:{}:{}", path.display(), i + 1, trunc(&line, 300)));
                        if hits.len() >= max {
                            return ok(hits.join("\n"));
                        }
                    }
                }
            }
        }
    }
    if hits.is_empty() {
        ok(format!("no matches for {pattern:?}"))
    } else {
        ok(hits.join("\n"))
    }
}

fn glob(args: &Value) -> ToolOutput {
    let pattern = arg(args, "pattern");
    if pattern.is_empty() {
        return err("pattern must not be empty".into());
    }
    let mut matches: Vec<String> = Vec::new();
    match glob::glob(pattern) {
        Ok(paths) => {
            for p in paths.flatten() {
                matches.push(p.display().to_string());
            }
        }
        Err(e) => return err(format!("invalid pattern: {e}")),
    }
    if matches.is_empty() {
        ok(format!("no matches for {pattern:?}"))
    } else {
        ok(matches.join("\n"))
    }
}

async fn bash(args: &Value) -> ToolOutput {
    let command = arg(args, "command");
    if command.is_empty() {
        return err("command must not be empty".into());
    }
    let start = std::time::Instant::now();
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(120),
        tokio::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(command)
            .output(),
    )
    .await;

    match result {
        Err(_) => err(format!("command timed out after 120s:\n{command}")),
        Ok(Err(e)) => err(format!("failed to spawn: {e}")),
        Ok(Ok(output)) => {
            let mut text = String::new();
            if !output.stdout.is_empty() {
                text.push_str(&String::from_utf8_lossy(&output.stdout));
            }
            if !output.stderr.is_empty() {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(&String::from_utf8_lossy(&output.stderr));
            }
            if text.is_empty() {
                text = "(no output)".into();
            }
            if !output.status.success() {
                text.push_str(&format!(
                    "\n[exit code {}]",
                    output.status.code().unwrap_or(-1)
                ));
            }
            let elapsed = start.elapsed().as_secs_f64();
            ok(format!("[{elapsed:.1}s]\n{}", trunc(&text, 256 * 1024)))
        }
    }
}

fn task(args: &Value) -> ToolOutput {
    let t = arg(args, "task");
    if t.is_empty() {
        return err("task must not be empty".into());
    }
    ok(format!("recorded task: {t}"))
}

async fn web_fetch(args: &Value) -> ToolOutput {
    let url = arg(args, "url");
    if url.is_empty() {
        return err("url must not be empty".into());
    }
    let max = args
        .get("max_bytes")
        .and_then(|v| v.as_u64())
        .unwrap_or(256 * 1024) as usize;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build();
    let client = match client {
        Ok(c) => c,
        Err(e) => return err(format!("failed to build http client: {e}")),
    };
    match client.get(url).send().await {
        Ok(resp) => {
            let status = resp.status();
            match resp.bytes().await {
                Ok(bytes) => {
                    let txt = String::from_utf8_lossy(&bytes);
                    ok(trunc(&txt, max))
                }
                Err(e) => err(format!("read failed ({status}): {e}")),
            }
        }
        Err(e) => err(format!("fetch failed: {e}")),
    }
}
