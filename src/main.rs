use clap::{Parser, Subcommand, ValueEnum};
use std::path::PathBuf;
use std::process::ExitCode;

mod db;

// Pinned per plan.md section 1.3; used by the server in later work.
use rmcp as _;
use rusqlite as _;
use tokio as _;

#[derive(Parser)]
#[command(
    name = "epocnote",
    version,
    about = "Markdown context memory for coding agents"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Initialize .context/ in a project
    Init {
        /// Project root directory
        #[arg(long)]
        root: Option<PathBuf>,

        /// Agent harness to install the epocnote skill for
        #[arg(long, value_enum, default_value_t = Target::None)]
        target: Target,
    },

    /// Run the stdio MCP server
    Serve {
        /// Project root directory
        #[arg(long)]
        root: Option<PathBuf>,

        /// Whether a recent directory scan may be reused instead of repeated
        #[arg(long, value_enum, default_value_t = ScanReuse::Auto)]
        scan_reuse: ScanReuse,
    },

    /// Verify the whole index against the Markdown files
    Rebuild {
        /// Project root directory
        #[arg(long)]
        root: Option<PathBuf>,
    },

    /// Report context metadata that no longer matches the working tree
    Check {
        /// Project root directory
        #[arg(long)]
        root: Option<PathBuf>,

        /// Exit unsuccessfully when any finding exists
        #[arg(long)]
        strict: bool,
    },
}

#[derive(Clone, ValueEnum)]
enum Target {
    None,
    Codex,
    ClaudeCode,
    Cursor,
}

#[derive(Clone, ValueEnum)]
enum ScanReuse {
    Auto,
    Off,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::Init { .. }
        | Command::Serve { .. }
        | Command::Rebuild { .. }
        | Command::Check { .. } => {
            eprintln!("epocnote: command not implemented yet");
            ExitCode::from(1)
        }
    }
}
