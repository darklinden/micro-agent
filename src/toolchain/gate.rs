//! LLM safety gate for command execution.
//!
//! Before a `bash` command runs (when `gate = true`), a *separate* LLM call
//! asks a judge model whether the command serves the current task and is not
//! destructive. Any failure — network error, unparseable answer, or an
//! explicit refusal — defaults to a denial (fail-safe).
//!
//! Commands that are provably read-only short-circuit **before** the judge: a
//! command (or `;`/`&&` chain) is split into its individual segments, and
//! when **every** segment is a whitelisted read-only command with no shell
//! operator, it runs with no LLM round trip (audited in the session log).
//! Escape handling matches the shell (a backslash outside quotes escapes the
//! next byte), so a `\`-escaped separator cannot hide a write behind a
//! read-only-looking segment. Anything uncertain goes to the judge.

use crate::types::{Message, Role};
use crate::upstream::Upstream;

/// Why a command was denied. Judge = the judge model refused it; the other
/// two are fail-safe denials where the *channel* failed, not the command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateDenyKind {
    Judge,
    Unparseable,
    UpstreamError,
}

/// Outcome of one gate check. `reason` is non-empty on denial (and is also
/// recorded on the allow path so the log stays auditable).
#[derive(Debug, Clone)]
pub struct GateVerdict {
    pub allow: bool,
    pub reason: String,
    /// `None` on allow; the denial category when denied.
    pub kind: Option<GateDenyKind>,
}

impl GateVerdict {
    pub fn allowed(reason: impl Into<String>) -> Self {
        GateVerdict {
            allow: true,
            reason: reason.into(),
            kind: None,
        }
    }
    pub fn denied(kind: GateDenyKind, reason: impl Into<String>) -> Self {
        GateVerdict {
            allow: false,
            reason: reason.into(),
            kind: Some(kind),
        }
    }
    pub fn is_allowed(&self) -> bool {
        self.allow
    }
}

pub struct Gate<'a> {
    upstream: &'a dyn Upstream,
    objective: &'a str,
    /// Nesting depth of the running agent (0 = top level). Stdout refusal
    /// notices print only at the top level; verdicts always go to the log.
    depth: u32,
}

impl<'a> Gate<'a> {
    pub fn new(upstream: &'a dyn Upstream, objective: &'a str, depth: u32) -> Self {
        Gate {
            upstream,
            objective,
            depth,
        }
    }

    /// Ask whether `command` should run. Never fails: upstream errors and
    /// unparseable answers fold into a fail-safe denial verdict, so callers
    /// do not need an error path of their own.
    pub async fn check(&self, command: &str) -> GateVerdict {
        // Read-only commands need no judge: allow without an LLM round trip
        // (recorded so the allowance stays auditable).
        if is_read_only(command) {
            let v = GateVerdict::allowed("read-only command, no persistent side effects");
            crate::sesslog::emit(
                crate::sesslog::Level::Info,
                "gate",
                serde_json::json!({
                    "depth": self.depth,
                    "command": command,
                    "allow": true,
                    "reason": v.reason,
                    "kind": null,
                }),
            );
            return v;
        }

        let prompt = format!(
            "You are a safety judge for an autonomous CLI coding agent. You evaluate ONE \
shell tool call in isolation, knowing the task intent. Judge only whether THIS \
command is safe to run right now.

TASK CONTEXT:
{objective}

COMMAND TO EVALUATE:
```sh
{command}
```

Rules:
- allow = true when the command is not destructive and does not clearly deviate
  from the task. Local read-only commands (git status/diff/log/rev-parse/
  ls-files/show/branch/remote/check-ignore/submodule status, ls, grep, cat,
  head, tail, less, pwd, cd, find, du, file, echo, which, type) are normally
  allowed, including several of them joined by `;` or `&&`. `cd` into any path
  is not suspicious.
- Local git write operations that serve the current task (add, commit,
  restore, clean of a task-targeted path) are allowed — committing work the
  task asked for is not harmful. Commits that would capture files unrelated to
  the task, or push to remotes, are not allowed.
- allow = false ONLY when the command itself is harmful: deletes/destroys data
  without task warrant, downloads and executes untrusted code, exfiltrates
  data, or makes unexpected network calls.
- Missing steps are not unsafe; the agent sequences its own calls. Do not
  reject a command just because it does not COMPLETE the task by itself.
- When in doubt, allow if the command is reversible or read-only; deny when
  there is concrete risk of harm.

Respond with JSON only, in this exact shape:
{{\"allow\": true/false, \"reason\": \"short justification\"}}",
            objective = self.objective
        );

        let msg = Message {
            role: Role::User,
            blocks: vec![crate::types::ContentBlock::Text(prompt)],
        };
        // Gate output is discarded — we only need the final verdict text.
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        drop(rx);
        let outcome = match self.upstream.chat("", &[msg], &[], tx).await {
            Ok(o) => o,
            Err(e) => {
                // A failing judge call denies silently downstream; record the
                // upstream error so the refusal is explainable from the log.
                crate::sesslog::emit(
                    crate::sesslog::Level::Warn,
                    "gate_error",
                    serde_json::json!({"command": command, "error": format!("{e:#}")}),
                );
                let v = GateVerdict::denied(
                    GateDenyKind::UpstreamError,
                    format!("judge call failed (denying fail-safe): {e}"),
                );
                if self.depth == 0 {
                    crate::out::gate_denied(&v.reason);
                }
                return v;
            }
        };

        // The judge's raw answer rides along on the `gate` event (bounded) so a
        // refusal whose verdict text never parsed can be inspected afterwards.
        let raw = crate::upstream::truncate(&outcome.assistant_text, 2000);
        let verdict = parse_verdict(&outcome.assistant_text);
        match verdict {
            Some((allow, reason)) => {
                // A denial is more interesting than an allowance: surface it
                // at warn so a scan of the session log finds refusals fast.
                let level = if allow {
                    crate::sesslog::Level::Info
                } else {
                    crate::sesslog::Level::Warn
                };
                let kind = (!allow).then_some(GateDenyKind::Judge);
                crate::sesslog::emit(
                    level,
                    "gate",
                    serde_json::json!({
                        "depth": self.depth,
                        "command": command,
                        "allow": allow,
                        "reason": reason,
                        "kind": kind.map(|k| format!("{k:?}")),
                        "response": raw,
                    }),
                );
                if !allow && self.depth == 0 {
                    crate::out::gate_denied(&reason);
                }
                if allow {
                    GateVerdict::allowed(reason)
                } else {
                    GateVerdict::denied(GateDenyKind::Judge, reason)
                }
            }
            None => {
                let reason = "unparseable gate answer; denying (fail-safe)";
                crate::sesslog::emit(
                    crate::sesslog::Level::Warn,
                    "gate",
                    serde_json::json!({
                        "depth": self.depth,
                        "command": command,
                        "allow": false,
                        "reason": reason,
                        "kind": "Unparseable",
                        "response": raw,
                    }),
                );
                if self.depth == 0 {
                    crate::out::gate_denied(reason);
                }
                GateVerdict::denied(GateDenyKind::Unparseable, reason)
            }
        }
    }
}

fn parse_verdict(text: &str) -> Option<(bool, String)> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    if end <= start {
        return None;
    }
    let obj: serde_json::Value = serde_json::from_str(&text[start..=end]).ok()?;
    let allow = match obj.get("allow") {
        Some(serde_json::Value::Bool(b)) => *b,
        _ => return None,
    };
    let reason = obj
        .get("reason")
        .and_then(|r| r.as_str())
        .unwrap_or("no reason given")
        .to_string();
    Some((allow, reason))
}

/// Prefix shared by every gate-denial tool result, so the turn loop can
/// recognize a denial without re-parsing the content. Both branches of
/// [`build_denied_content`] start with it; other refusal texts (deny-list,
/// unknown tool) do not.
pub const DENIAL_MARKER: &str = "The safety judge ";

/// Three-part refusal text fed back to the agent as the tool result.
/// Pure function so both `run_tool` and unit tests share it.
pub fn build_denied_content(command: &str, v: &GateVerdict) -> String {
    let command = crate::upstream::truncate(command, 600);
    match v.kind {
        Some(GateDenyKind::Judge) => format!(
            "{DENIAL_MARKER}refused this bash command.\n\
Command: {command}\n\
Judge's reason: {}\n\
The judge may have mistaken task steps for acceptance criteria — check \
whether its reason is really part of the task before acting on it. If the \
command itself is safe and the refusal looks mistaken, retry once as \
smaller, simpler steps (e.g. split add/commit, use `git -F`).",
            v.reason
        ),
        Some(GateDenyKind::Unparseable) | Some(GateDenyKind::UpstreamError) => format!(
            "{DENIAL_MARKER}could not produce a verdict for this bash command \
(fail-safe denial) — usually the judge's response was unparseable or the \
judge call failed, NOT that the command is unsafe.\n\
Command: {command}\n\
Failure detail: {}\n\
You may retry the command as-is once, or split it into smaller steps.",
            v.reason
        ),
        None => {
            // An allowed verdict must never reach the denied-content path.
            format!("command was allowed: {}", v.reason)
        }
    }
}

/// `;`- and `&&`-joined segments, split on separators that sit *outside*
/// quotes. This is not full shell parsing: quotes are only honored so that a
/// `;` inside e.g. `echo "a;b"` is not mistaken for a chain separator (the
/// real log shape `git status --porcelain; echo "exit=$?"` must split).
///
/// Escape handling matches the shell: a backslash *outside* quotes escapes
/// the next byte — including a quote or a `;`/`&` that would otherwise be a
/// separator (`echo a\; rm -f x` runs both commands). The escaped byte is
/// skipped so the chain splits exactly where the shell would.
///
/// A lone `&` (backgrounding) is not a separator: it stays inside its
/// segment and the segment then fails `has_shell_operator`, so nothing
/// backgrounded is ever pre-allowed.
fn split_chain_segments(command: &str) -> Vec<&str> {
    let bytes = command.as_bytes();
    let mut segments = Vec::new();
    let mut start = 0;
    let mut quote: Option<u8> = None; // Some(b'"') or Some(b'\'')
    let mut escaped = false;
    for (i, &b) in bytes.iter().enumerate() {
        if escaped {
            escaped = false;
            continue;
        }
        if b == b'\\' && quote.is_none() {
            escaped = true;
            continue;
        }
        match quote {
            Some(q) => {
                if b == q {
                    quote = None;
                }
            }
            None => {
                if b == b'\'' || b == b'"' {
                    quote = Some(b);
                } else if b == b';' {
                    if let Ok(s) = std::str::from_utf8(&bytes[start..i])
                        && !s.trim().is_empty()
                    {
                        segments.push(s);
                    }
                    start = i + 1;
                } else if b == b'&' && i + 1 < bytes.len() && bytes[i + 1] == b'&' {
                    if let Ok(s) = std::str::from_utf8(&bytes[start..i])
                        && !s.trim().is_empty()
                    {
                        segments.push(s);
                    }
                    start = i + 2;
                }
            }
        }
    }
    if let Ok(s) = std::str::from_utf8(&bytes[start..]) && !s.trim().is_empty() {
        segments.push(s);
    }
    segments
}

/// Pure read-only check: split the chain into its commands and require every
/// one of them to be a whitelisted read-only command with no shell operator.
/// Only ever pre-allows — never pre-denies; anything uncertain (an unknown
/// command, a redirection, a background job, ...) is left for the judge.
fn is_read_only(command: &str) -> bool {
    let segments = split_chain_segments(command);
    !segments.is_empty() && segments.iter().all(|s| read_only_segment(s))
}

/// True when the segment's first word is a whitelisted read-only command or
/// `git` with a read-only subcommand (`git -C <dir> <sub>` handled too). The
/// rest of the segment is not inspected — extra arguments to a read-only
/// command are read-only, and any shell operator hiding in the segment is
/// caught by `has_shell_operator` below.
fn read_only_segment(segment: &str) -> bool {
    let words: Vec<&str> = segment.split_whitespace().collect();
    let first = match words.first() {
        Some(w) => *w,
        None => return false,
    };
    if is_plain_read_only(first) && !has_shell_operator(segment) {
        return true;
    }
    if first == "git" && !has_shell_operator(segment) {
        // Word boundary: `git` alone is a pager, fine. The read-only
        // subcommand is the first non-option word — global options
        // (`-C <dir>`, `-c key=val`, ...) come first. A `-C` swallows its
        // value; any other `-` token (or a value that would follow a bare
        // `-C`) fails the check and goes to the judge.
        let mut rest = words.into_iter().skip(1);
        let sub = loop {
            let w = match rest.next() {
                Some(w) => w,
                None => return false, // `git` with no subcommand: judge
            };
            if w == "-C" || w == "-c" {
                // `-C <dir>` and `-c key=val` each swallow a value word; a
                // dangling one means the segment is malformed -> judge.
                if rest.next().is_none() {
                    return false;
                }
            } else if let Some(v) = w.strip_prefix('-') {
                if v.is_empty() {
                    return false;
                }
            } else {
                break w;
            }
        };
        match sub {
            "status" | "diff" | "log" | "rev-parse" | "ls-files" | "show"
            | "branch" | "remote" | "check-ignore" => true,
            "submodule" => rest.next() == Some("status"),
            _ => false, // add/commit/push/... and unknown subcommands: judge
        }
    } else {
        false
    }
}

/// True when the segment contains a shell operator that gives a plain
/// read-only command an execution/persistence surface: pipes, redirections
/// (`>`/`>>`/`<<` heredoc), command substitution (backticks/`$(`), or a
/// background `&`. Each `&&`/`;` was already split off by
/// `split_chain_segments`, so any remaining `&` is backgrounding. Such
/// segments go to the judge.
///
/// Escape handling matches `split_chain_segments` (and the shell): a
/// backslash *outside* quotes escapes the next byte, so `echo a\; rm -f x`
/// — where the shell sees `;` as a separator — is flagged here because the
/// `;` would otherwise be invisible behind the escaped quote.
fn has_shell_operator(segment: &str) -> bool {
    let b = segment.as_bytes();
    let mut quote: Option<u8> = None;
    let mut escaped = false;
    for (i, &ch) in b.iter().enumerate() {
        if escaped {
            escaped = false;
            continue;
        }
        if ch == b'\\' && quote.is_none() {
            escaped = true;
            continue;
        }
        match quote {
            Some(q) => {
                if ch == q {
                    quote = None;
                }
            }
            None => match ch {
                b'\'' | b'"' => quote = Some(ch),
                b'|' | b'>' | b'<' | b'&' | b'`' => return true,
                b'$' if b.get(i + 1) == Some(&b'(') => return true,
                _ => {}
            },
        }
    }
    false
}

/// First word is one of the whitelisted read-only commands (word boundary).
fn is_plain_read_only(word: &str) -> bool {
    matches!(
        word,
        "ls" | "grep" | "cat" | "head" | "tail" | "less" | "pwd" | "cd"
            | "find" | "du" | "file" | "echo" | "which" | "type"
    )
}

#[cfg(test)]
mod tests {
    use super::{
        build_denied_content, is_read_only, parse_verdict, GateDenyKind, GateVerdict,
    };
    use crate::types::{Message, StreamOutcome, ToolDef};
    use crate::upstream::Upstream;
    use serde_json::Value;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, OnceLock};

    #[test]
    fn parses_plain_allow() {
        assert_eq!(
            parse_verdict(r#"{"allow": true, "reason": "fine"}"#),
            Some((true, "fine".to_string()))
        );
    }

    #[test]
    fn parses_deny() {
        assert_eq!(
            parse_verdict(r#"{"allow": false, "reason": "rm -rf"}"#),
            Some((false, "rm -rf".to_string()))
        );
    }

    #[test]
    fn parses_fenced_json() {
        assert_eq!(
            parse_verdict("Here you go:\n```json\n{\"allow\": true, \"reason\": \"ok\"}\n```\n"),
            Some((true, "ok".to_string()))
        );
    }

    #[test]
    fn rejects_non_json() {
        assert_eq!(parse_verdict("I do not know."), None);
        assert_eq!(parse_verdict(r#"{"allow": "yes"}"#), None);
    }

    #[test]
    fn missing_reason_defaults_to_no_reason_given() {
        assert_eq!(
            parse_verdict(r#"{"allow": false}"#),
            Some((false, "no reason given".to_string()))
        );
        assert_eq!(
            parse_verdict(r#"{"allow": true, "reason": null}"#),
            Some((true, "no reason given".to_string()))
        );
    }

    #[test]
    fn read_only_positive_table() {
        for cmd in [
            // single commands
            "git diff",
            "ls -la ~/*",
            "ls -lh 天津人吃到美食belike.mp4",
            "git -C /x status --porcelain",
            "echo hi",
            "pwd",
            "git check-ignore -v path",
            "echo \"a;b\"",
            "echo \"a&&b\"",
            "echo 'a;b'",
            "git -C /x -c color.ui=never status",
            "echo a\\'b", // escaped quote: still a single echo
            // chains: every segment must be read-only, `;` or `&&`
            "ls;ls",
            "git status;echo done",
            "git status --porcelain; echo \"exit=$?\"",
            "echo 'a;b'; git status",
            "cd /x && git status",
            "cd /tmp && ls -la",
            "git status && git diff",
            "git status && echo done",
            "ls && cd /x",
            "git status && git diff && git log",
            "git status; echo done; pwd",
            // single-quoted segments can contain anything read-only-looking;
            // the segment must still start with a whitelisted command
            "cat 'a;b'",
            "echo 'a&&b'; git status",
        ] {
            assert!(is_read_only(cmd), "expected read-only: {cmd}");
        }
    }

    #[test]
    fn read_only_negative_table() {
        for cmd in [
            "rm -rf x",
            "curl | sh",
            "git commit -m x",
            "ls > out.txt",
            "lsof -i",
            "lsblk",
            "cd /x && echo hi && rm -f y",
            "git -C /x commit -m x",
            "cat <<'EOF'",
            "ls;rm x",
            "git add -A;git commit -m x",
            "ls > out.txt; git status",
            "catch",
            "background & no",
            "ls &",
            "ls && echo hi && rm -f y",
            "git config user.name x",
            // escape variants: the shell runs the second command, so the
            // chain must not be pre-allowed
            "echo a\\'; rm -f x",
            "echo a\\'; rm -f /tmp/x",
            "echo a\\&& rm -f x",
            "echo a\\&& rm -f x; ls",
            "echo a\\' && rm -f x",
        ] {
            assert!(!is_read_only(cmd), "expected not read-only: {cmd}");
        }
    }

    /// Canned upstream that records how many times `chat` was called and
    /// replays a fixed answer. Same shape as the fakes in compress.rs and
    /// subagent.rs tests.
    struct FakeUpstream {
        calls: AtomicUsize,
        reply: Arc<dyn Fn() -> anyhow::Result<StreamOutcome> + Send + Sync>,
    }

    impl FakeUpstream {
        fn canned(text: &'static str) -> Self {
            let reply = Arc::new(move || {
                Ok(StreamOutcome {
                    assistant_text: text.to_string(),
                    ..StreamOutcome::default()
                })
            });
            FakeUpstream {
                calls: AtomicUsize::new(0),
                reply,
            }
        }
        fn failing() -> Self {
            let reply = Arc::new(|| Err(anyhow::anyhow!("network down")));
            FakeUpstream {
                calls: AtomicUsize::new(0),
                reply,
            }
        }
        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl Upstream for FakeUpstream {
        fn wire_tools(&self, _tools: &[ToolDef]) -> Vec<Value> {
            vec![]
        }
        async fn chat(
            &self,
            _system: &str,
            _messages: &[Message],
            _tools: &[ToolDef],
            _emitter: tokio::sync::mpsc::UnboundedSender<crate::types::StreamEvent>,
        ) -> anyhow::Result<StreamOutcome> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            (self.reply)()
        }
    }

    static LOG_GUARD: OnceLock<()> = OnceLock::new();

    /// Session-log events use a process-global writer. A live log (any
    /// directory, Debug threshold) makes every emit a real write; without
    /// one, emits are no-ops and the code under test still runs fine — this
    /// only guarantees the writer exists so no path panics.
    fn init_sesslog() {
        LOG_GUARD.get_or_init(|| {
            let dir: PathBuf =
                std::env::temp_dir().join(format!("ma-gate-test-{}", std::process::id()));
            let _ = crate::sesslog::init(Some(&dir), "debug");
        });
    }

    #[tokio::test]
    async fn check_read_only_short_circuits_before_judge() {
        init_sesslog();
        let fake = FakeUpstream::canned("should not be called");
        let gate = super::Gate::new(&fake, "objective", 0);
        let v = gate.check("ls -lh some file.mp4").await;
        assert!(v.is_allowed());
        assert!(v.kind.is_none());
        assert_eq!(fake.calls(), 0, "read-only command must not call the judge");
    }

    #[tokio::test]
    async fn check_allows_on_canned_allow() {
        init_sesslog();
        let fake = FakeUpstream::canned(r#"{"allow": true, "reason": "fine"}"#);
        let gate = super::Gate::new(&fake, "objective", 0);
        let v = gate.check("git add -A && git commit -m x").await;
        assert!(v.is_allowed());
        assert!(v.kind.is_none());
        assert_eq!(fake.calls(), 1);
    }

    #[tokio::test]
    async fn check_denies_on_canned_deny() {
        init_sesslog();
        let fake = FakeUpstream::canned(r#"{"allow": false, "reason": "rm -rf"}"#);
        let gate = super::Gate::new(&fake, "objective", 0);
        let v = gate.check("rm -rf x").await;
        assert!(!v.is_allowed());
        assert_eq!(v.kind, Some(GateDenyKind::Judge));
        assert_eq!(v.reason, "rm -rf");
    }

    #[tokio::test]
    async fn check_denies_on_unparseable() {
        init_sesslog();
        let fake = FakeUpstream::canned("I do not know.");
        let gate = super::Gate::new(&fake, "objective", 0);
        let v = gate.check("some command").await;
        assert!(!v.is_allowed());
        assert_eq!(v.kind, Some(GateDenyKind::Unparseable));
        assert!(v.reason.contains("unparseable"));
    }

    #[tokio::test]
    async fn check_denies_on_upstream_error() {
        init_sesslog();
        let fake = FakeUpstream::failing();
        let gate = super::Gate::new(&fake, "objective", 0);
        let v = gate.check("some command").await;
        assert!(!v.is_allowed());
        assert_eq!(v.kind, Some(GateDenyKind::UpstreamError));
        assert!(v.reason.contains("judge call failed"));
    }

    #[test]
    fn denied_content_judge_branch_mentions_refusal_and_reason() {
        let v = GateVerdict::denied(GateDenyKind::Judge, "rm -rf is destructive");
        let s = build_denied_content("rm -rf x", &v);
        assert!(s.starts_with(super::DENIAL_MARKER), "denial must carry the shared marker");
        assert!(s.contains("The safety judge refused this bash command."));
        assert!(s.contains("rm -rf is destructive"));
        assert!(s.contains("split add/commit"));
    }

    #[test]
    fn denied_content_channel_branch_absolves_command() {
        let v = GateVerdict::denied(GateDenyKind::Unparseable, "unparseable gate answer");
        let s = build_denied_content("git rev-parse --show-toplevel", &v);
        assert!(s.starts_with(super::DENIAL_MARKER), "denial must carry the shared marker");
        assert!(s.contains("could not produce a verdict"));
        assert!(s.contains("NOT that the command is unsafe"));
        assert!(s.contains("retry the command as-is once"));
        let v = GateVerdict::denied(GateDenyKind::UpstreamError, "judge call failed: e");
        let s = build_denied_content("git rev-parse --show-toplevel", &v);
        assert!(s.starts_with(super::DENIAL_MARKER), "denial must carry the shared marker");
        assert!(s.contains("could not produce a verdict"));
        assert!(s.contains("NOT that the command is unsafe"));
    }

    #[test]
    fn denied_content_truncates_long_commands() {
        let v = GateVerdict::denied(GateDenyKind::Judge, "r");
        let long = format!("echo {}", "x".repeat(2000));
        let s = build_denied_content(&long, &v);
        assert!(s.len() < long.len());
        assert!(s.contains("Command:"));
    }
}
