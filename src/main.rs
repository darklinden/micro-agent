//! `ma` — a lightweight autonomous CLI agent (mini Claude Code).
//!
//! Three-mode workflow (plan → edit → run): `-p` explores and writes a
//! numbered plan, `-e`+`-c` revises an existing one, and `-r` executes one by
//! dispatching independent steps to sub-agents via the `task` tool.

mod config;
mod logger;
mod loop_;
mod mcp;
mod out;
mod persona;
mod toolchain;
mod types;
mod upstream;

use anyhow::Result;
use clap::Parser;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Lightweight autonomous CLI agent, driven entirely by environment variables.
#[derive(Parser, Debug)]
#[command(name = "ma", version, about, disable_help_subcommand = true)]
struct Cli {
    /// Task description: explore and write a numbered plan, print it, and print its path.
    #[arg(short = 'p', long = "plan")]
    plan: Option<String>,

    /// Path of an existing plan file to revise (requires --change).
    #[arg(short = 'e', long = "edit-plan")]
    edit_plan: Option<PathBuf>,

    /// Revision instruction (requires --edit-plan).
    #[arg(short = 'c', long = "change")]
    change: Option<String>,

    /// Path of the plan file to execute.
    #[arg(short = 'r', long = "run")]
    run: Option<PathBuf>,

    /// List available tools and exit.
    #[arg(long = "list-tools")]
    list_tools: bool,
}

/// Which of the three workflow modes this invocation runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Plan,
    Edit,
    Run,
}

/// How many of the three mode-selectors are set.
fn mode_count(cli: &Cli) -> usize {
    [cli.plan.is_some(), cli.edit_plan.is_some(), cli.run.is_some()]
        .into_iter()
        .filter(|b| *b)
        .count()
}

/// Derive the run mode from the CLI flags, erroring on ambiguity.
fn resolve_mode(cli: &Cli) -> Result<Mode> {
    if cli.list_tools {
        return Ok(Mode::Plan); // unused; list_tools short-circuits before running
    }
    if mode_count(cli) != 1 {
        anyhow::bail!(
            "exactly one of -p/--plan, -e/--edit-plan, or -r/--run must be given"
        );
    }
    if cli.plan.is_some() {
        Ok(Mode::Plan)
    } else if cli.edit_plan.is_some() {
        if cli.change.is_none() {
            anyhow::bail!("-e/--edit-plan requires -c/--change");
        }
        Ok(Mode::Edit)
    } else {
        Ok(Mode::Run)
    }
}

fn usage_error(msg: &str) -> ! {
    eprintln!("error: {msg}");
    std::process::exit(2);
}

/// Load `.env` files (exe-dir first, then crate root in debug builds; shell
/// vars always win, exe-dir takes precedence). Returns the paths of the files
/// that actually existed and were loaded.
fn load_env() -> Vec<PathBuf> {
    let mut loaded = Vec::new();
    // Load the executable-dir `.env` first so it takes precedence over the
    // crate root: `dotenvy::from_path` never overrides an already-set
    // variable, so whichever file loads first wins.
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let p = dir.join(".env");
        if load_from(&p) {
            loaded.push(p);
        }
    }
    if cfg!(debug_assertions) {
        let p = Path::new(env!("CARGO_MANIFEST_DIR")).join(".env");
        if load_from(&p) {
            loaded.push(p);
        }
    }
    loaded
}

/// Load a single `.env` file without overriding already-set variables.
/// Returns whether the file existed and was loaded. Missing/unreadable is not
/// fatal (returns `false`).
fn load_from(path: &Path) -> bool {
    dotenvy::from_path(path).is_ok()
}

fn main() {
    let loaded_env = load_env();
    let cli = Cli::parse();

    // Config & logger
    let cfg = match config::from_env() {
        Ok(c) => c,
        Err(e) => usage_error(&format!("invalid configuration: {e:#}")),
    };
    let _guard = logger::init(cfg.log_dir.as_ref(), &cfg.log_level);

    // Startup banner: which `.env` files were actually loaded and the
    // upstream base URL in effect (never the API key).
    let env_desc = if loaded_env.is_empty() {
        "(none)".to_string()
    } else {
        loaded_env
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    };
    out::banner(&format!("env: {env_desc} | upstream url: {}", cfg.url));
    tracing::info!(env = %env_desc, url = %cfg.url, "startup");

    let mode = match resolve_mode(&cli) {
        Ok(m) => m,
        Err(e) => usage_error(&format!("{e:#}")),
    };

    let rt = match tokio::runtime::Runtime::new() {
        Ok(r) => r,
        Err(e) => usage_error(&format!("failed to start async runtime: {e}")),
    };

    let code = match rt.block_on(run(cfg, cli, mode)) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e:#}");
            tracing::error!(error = %e, "run failed");
            2
        }
    };
    std::process::exit(code);
}

async fn run(mut cfg: config::Config, cli: Cli, mode: Mode) -> Result<i32> {
    // MCP servers (connects + lists tools) are set up before the agent loop.
    let mcp = mcp::McpPool::connect(&cfg).await?;

    if cli.list_tools {
        let tools = toolchain::build_tools(&mcp);
        for t in &tools {
            println!("- {}: {}", t.name, t.description);
        }
        return Ok(0);
    }

    // A shared record of this run's plan path, so Plan/Edit can print it after.
    let plan_state: Arc<Mutex<Option<PathBuf>>> = Arc::new(Mutex::new(None));

    // The mode restricts which tools the model may call. Deny-list entries run
    // before every dispatch path (ordering invariant kept), and these overlays
    // only add to — never remove — the user's MA_DENY_TOOLS.
    let base_system = persona::build(&cfg)?;
    let (system, objective): (String, String) = match mode {
        Mode::Plan => {
            cfg.deny_tools.extend(
                ["write_file", "edit_file", "task"]
                    .into_iter()
                    .map(str::to_string),
            );
            (
                format!("{base_system}\n\n{}", persona::MODE_PLAN_INSTRUCTIONS),
                format!(
                    "{}\n\nExplore first, then submit your complete numbered plan via the `plan` tool.",
                    cli.plan.as_deref().unwrap_or("")
                ),
            )
        }
        Mode::Edit => {
            cfg.deny_tools.extend(
                ["write_file", "edit_file", "task"]
                    .into_iter()
                    .map(str::to_string),
            );
            let path = cli.edit_plan.as_ref().expect("validated above");
            let content = std::fs::read_to_string(path)
                .unwrap_or_else(|e| usage_error(&format!("cannot read plan {}: {e}", path.display())));
            (
                format!("{base_system}\n\n{}", persona::MODE_EDIT_INSTRUCTIONS),
                format!(
                    "Existing plan:\n```\n{content}\n```\n\nRevision request: {}\n\nSubmit the COMPLETE revised plan via the `plan` tool.",
                    cli.change.as_deref().unwrap_or("")
                ),
            )
        }
        Mode::Run => {
            cfg.deny_tools.extend(["plan"].into_iter().map(str::to_string));
            let path = cli.run.as_ref().expect("validated above");
            let content = std::fs::read_to_string(path)
                .unwrap_or_else(|e| usage_error(&format!("cannot read plan {}: {e}", path.display())));
            (
                format!("{base_system}\n\n{}", persona::MODE_RUN_INSTRUCTIONS),
                format!(
                    "Execute the following plan:\n\n```\n{content}\n```\n\nPlan file: {}",
                    path.display()
                ),
            )
        }
    };

    let upstream = upstream::build(&cfg)?;
    let agent = loop_::Agent {
        cfg: &cfg,
        upstream: upstream.as_ref(),
        system,
        objective,
        mcp: &mcp,
        depth: 0,
        max_turns: cfg.max_turns,
        plan_path: Some(plan_state.clone()),
    };

    let outcome = agent.run().await?;

    match mode {
        Mode::Plan | Mode::Edit => {
            // The `plan` tool records the file path; output it so the caller can
            // feed it straight into `-r`. No plan submitted -> treat as failure.
            match plan_state.lock().unwrap().clone() {
                Some(path) => {
                    out::banner(&format!("[plan] {}", path.display()));
                    Ok(if outcome.result == loop_::RunResult::Done { 0 } else { 2 })
                }
                None => {
                    eprintln!("error: no plan was submitted (the model never called the `plan` tool)");
                    Ok(2)
                }
            }
        }
        Mode::Run => match outcome.result {
            loop_::RunResult::Done => Ok(0),
            loop_::RunResult::MaxTurns => Ok(2),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::{resolve_mode, Cli, Mode};

    fn cli(
        plan: Option<&str>,
        edit_plan: Option<&str>,
        change: Option<&str>,
        run: Option<&str>,
    ) -> Cli {
        Cli {
            plan: plan.map(String::from),
            edit_plan: edit_plan.map(Into::into),
            change: change.map(String::from),
            run: run.map(Into::into),
            list_tools: false,
        }
    }

    #[test]
    fn requires_exactly_one_mode() {
        assert!(resolve_mode(&cli(None, None, None, None)).is_err());
        assert!(resolve_mode(&cli(Some("p"), None, None, Some("r"))).is_err());
        assert_eq!(
            resolve_mode(&cli(Some("p"), None, None, None)).unwrap(),
            Mode::Plan
        );
        assert_eq!(
            resolve_mode(&cli(None, None, None, Some("r"))).unwrap(),
            Mode::Run
        );
    }

    #[test]
    fn edit_requires_change_and_vice_versa() {
        assert!(resolve_mode(&cli(None, Some("e"), None, None)).is_err());
        assert!(resolve_mode(&cli(None, None, Some("c"), None)).is_err());
        assert_eq!(
            resolve_mode(&cli(None, Some("e"), Some("c"), None)).unwrap(),
            Mode::Edit
        );
    }
}
