//! Anwesen: read-only HTTP daemon over a markdown vault.
//!
//! This module wires the CLI to the (still-stub) `serve` and `doctor`
//! subcommands. The full daemon arrives over [ANW-11..ANW-19].

mod cli;

use anwesen::app::Anwesen;
use anwesen::doctor;
use anyhow::Result;
use clap::Parser;
use hydra::Application;

use crate::cli::{Cli, Command};

// Result is retained at the binary boundary per [[ADR-001 Language and
// Foundation Libraries]] (anyhow at main); current stubs do not yet
// surface errors.
#[allow(clippy::unnecessary_wraps)]
fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Serve(args) => {
            init_logging(args.log_level);
            tracing::info!(
                vault = %args.vault.display(),
                bind = %args.bind,
                "anwesen serve: starting supervisor tree"
            );
            // Blocks until the supervisor exits (SIGTERM / SIGINT / crash).
            Anwesen::new(args.vault, args.bind).run();
        }
        Command::Doctor(args) => {
            init_logging(args.log_level);
            let (rendered, exit) = doctor::run_and_render(&args.vault);
            // Render to stdout so the report is pipe-friendly; logs go to
            // stderr via the tracing subscriber.
            print!("{rendered}");
            std::process::exit(exit);
        }
        Command::Version => {
            println!("{}", env!("CARGO_PKG_VERSION"));
        }
    }
    Ok(())
}

fn init_logging(level: cli::LogLevel) {
    // The User Manual lists --log-level / ANWESEN_LOG_LEVEL as the only knobs
    // for verbosity, and pins "CLI flags win over environment variables".
    // Resolution happens in clap; this function only builds the filter from
    // the already-resolved level.
    let filter = tracing_subscriber::EnvFilter::new(level.as_filter_directive());
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}
