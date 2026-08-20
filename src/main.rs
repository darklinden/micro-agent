//! `ma` — a lightweight autonomous CLI agent (mini Claude Code).

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

/// Lightweight autonomous CLI agent, driven entirely by environment variables.
#[derive(Parser, Debug)]
#[command(name = "ma", version, about, disable_help_subcommand = true)]
struct Cli {
    /// The task to run. Required (no stdin prompt semantics).
    #[arg(short = 'p', long = "prompt")]
    prompt: Option<String>,

    /// List available tools and exit.
    #[arg(long = "list-tools")]
    list_tools: bool,
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

    if !cli.list_tools && cli.prompt.is_none() {
        usage_error("missing required argument -p/--prompt");
    }

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

    let rt = match tokio::runtime::Runtime::new() {
        Ok(r) => r,
        Err(e) => usage_error(&format!("failed to start async runtime: {e}")),
    };

    let code = match rt.block_on(run(cfg, cli.prompt, cli.list_tools)) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e:#}");
            tracing::error!(error = %e, "run failed");
            2
        }
    };
    std::process::exit(code);
}

async fn run(cfg: config::Config, prompt: Option<String>, list_tools: bool) -> Result<i32> {
    // MCP servers (connects + lists tools) are set up before the agent loop.
    let mcp = mcp::McpPool::connect(&cfg).await?;

    if list_tools {
        let tools = toolchain::build_tools(&mcp);
        for t in &tools {
            println!("- {}: {}", t.name, t.description);
        }
        return Ok(0);
    }

    let system = persona::build(&cfg)?;
    tracing::debug!(system_len = system.len(), "system prompt built");

    let upstream = upstream::build(&cfg)?;
    let objective = prompt.expect("prompt validated above");

    let agent = loop_::Agent {
        cfg: &cfg,
        upstream: upstream.as_ref(),
        system,
        objective,
        mcp: &mcp,
    };

    match agent.run().await? {
        loop_::RunResult::Done => Ok(0),
        loop_::RunResult::MaxTurns => Ok(2),
    }
}
