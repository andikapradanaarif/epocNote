use clap::{Parser, Subcommand};
use epocnote::init::{self, Target};
use std::path::PathBuf;
use std::process::ExitCode;

// Pinned per plan.md section 1.3; used by the server in later work.
use epocnote as _;
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

#[derive(Clone, clap::ValueEnum)]
enum ScanReuse {
    Auto,
    Off,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::Init { root, target } => cmd_init(root, target),
        Command::Serve { .. } | Command::Rebuild { .. } | Command::Check { .. } => {
            eprintln!("epocnote: command not implemented yet");
            ExitCode::from(1)
        }
    }
}

fn cmd_init(root: Option<PathBuf>, target: Target) -> ExitCode {
    let cwd = match std::env::current_dir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("epocnote init: {e}");
            return ExitCode::from(1);
        }
    };
    match init::run_init(root.as_deref(), &cwd, target) {
        Ok((resolved, report)) => {
            print_report(&report);
            println!("epocnote initialized at {}", resolved.display());
            println!("launch: epocnote serve --root \"{}\"", resolved.display());
            println!(
                "register that command as a local stdio MCP server in your client configuration"
            );
            ExitCode::SUCCESS
        }
        Err(failure) => {
            print_report(&failure.report);
            eprintln!("epocnote init: {}", failure.error);
            ExitCode::from(1)
        }
    }
}

fn print_report(report: &init::InitReport) {
    for action in &report.actions {
        println!("{action}");
    }
    for warning in &report.warnings {
        let at = warning
            .path
            .as_deref()
            .map(|p| format!(" ({p})"))
            .unwrap_or_default();
        eprintln!("warning[{}]{at}: {}", warning.code, warning.message);
    }
}
