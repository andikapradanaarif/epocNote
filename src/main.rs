use clap::{Parser, Subcommand};
use epocnote::check;
use epocnote::init::{self, Target};
use epocnote::root::discover_root;
use epocnote::server;
use epocnote::sync::{self, ScanReuse};
use epocnote::{Error, Warning};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

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

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::Init { root, target } => cmd_init(root, target),
        Command::Rebuild { root } => cmd_rebuild(root.as_deref()),
        Command::Check { root, strict } => cmd_check(root.as_deref(), strict),
        Command::Serve { root, scan_reuse } => cmd_serve(root.as_deref(), scan_reuse),
    }
}

fn maintenance_root(explicit: Option<&Path>) -> Result<PathBuf, Error> {
    discover_root(explicit, &std::env::current_dir()?, false)
}

fn cmd_serve(explicit: Option<&Path>, scan_reuse: ScanReuse) -> ExitCode {
    let result = maintenance_root(explicit).and_then(|root| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()?
            .block_on(server::run(root, scan_reuse))
    });
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("epocnote serve: {}: {error}", error.code());
            ExitCode::FAILURE
        }
    }
}

fn cmd_rebuild(explicit: Option<&Path>) -> ExitCode {
    let result = maintenance_root(explicit).and_then(|root| sync::rebuild(&root));
    match result {
        Ok(report) => {
            println!("indexed_count: {}", report.indexed_count);
            println!("excluded_count: {}", report.excluded_count);
            println!("failed_count: {}", report.failed_count);
            for warning in &report.warnings {
                print_warning(warning);
            }
            if report.warnings_truncated {
                eprintln!("epocnote rebuild: additional warnings omitted");
            }
            if report.failed_count == 0 {
                ExitCode::SUCCESS
            } else {
                eprintln!(
                    "epocnote rebuild: IO_ERROR: could not verify {} document(s)",
                    report.failed_count
                );
                ExitCode::FAILURE
            }
        }
        Err(e) => {
            eprintln!("epocnote rebuild: {}: {e}", e.code());
            ExitCode::FAILURE
        }
    }
}

fn cmd_check(explicit: Option<&Path>, strict: bool) -> ExitCode {
    match maintenance_root(explicit).and_then(|root| check::check(&root)) {
        Ok(findings) => {
            for finding in &findings {
                println!("{}: {}: {}", finding.path, finding.code, finding.detail);
            }
            if strict && !findings.is_empty() {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            }
        }
        Err(e) => {
            eprintln!("epocnote check: {}: {e}", e.code());
            ExitCode::FAILURE
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
            eprintln!("epocnote init: {}: {}", failure.error.code(), failure.error);
            ExitCode::from(1)
        }
    }
}

fn print_report(report: &init::InitReport) {
    for action in &report.actions {
        println!("{action}");
    }
    for warning in &report.warnings {
        print_warning(warning);
    }
}

fn print_warning(warning: &Warning) {
    let at = warning
        .path
        .as_deref()
        .map(|p| format!(" ({})", p.escape_debug()))
        .unwrap_or_default();
    eprintln!("warning[{}]{at}: {}", warning.code, warning.message);
}
