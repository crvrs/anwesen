//! Anwesen: read-only HTTP daemon over a markdown vault.
//!
//! This module wires the CLI to the (still-stub) `serve` and `doctor`
//! subcommands. The full daemon arrives over [ANW-11..ANW-19].

mod cli;

use anyhow::Result;
use clap::Parser;

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
                "anwesen serve: not yet implemented (ANW-10 stub)"
            );
        }
        Command::Doctor(args) => {
            init_logging(args.log_level);
            tracing::info!(
                vault = %args.vault.display(),
                "anwesen doctor: not yet implemented (ANW-10 stub)"
            );
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
