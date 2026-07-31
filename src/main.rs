//! Anwesen: read-only HTTP daemon over a markdown vault.
//!
//! This module wires the CLI to the `serve`, `doctor`, `merge`, `query`, and
//! `version` subcommands.

mod cli;

use std::sync::Arc;
use std::sync::atomic::Ordering;

use anwesen::app::Anwesen;
use anwesen::doctor;
use anwesen::oneshot;
use anwesen::telemetry::{self, OtelEnv, RawTelemetryArgs, TelemetryConfig};
use anyhow::Result;
use clap::Parser;
use hydra::Application;

use crate::cli::{Cli, Command};

// Result is retained at the binary boundary per [[ADR-001 Language and
// Foundation Libraries]] (anyhow at main); telemetry config resolution and
// exporter setup surface startup errors through it.
fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Serve(args) => {
            init_logging(args.log_level);
            // Resolve telemetry config before the supervisor starts; no
            // OTEL_EXPORTER_OTLP_* endpoint leaves it `None` (export off,
            // no middleware). A removed flag is a startup error (ANW-42).
            let telemetry = match TelemetryConfig::resolve(
                &RawTelemetryArgs {
                    uptrace_dsn: args.uptrace_dsn,
                    otlp_endpoint: args.otlp_endpoint,
                    otlp_headers: args.otlp_headers,
                    slow_request_ms: args.otlp_slow_request_ms,
                },
                OtelEnv::from_env(),
            )? {
                Some(cfg) => Some(Arc::new(telemetry::init(cfg)?)),
                None => None,
            };
            tracing::info!(
                vault = %args.vault.display(),
                bind = %args.bind,
                telemetry = telemetry.is_some(),
                "anwesen serve: starting supervisor tree"
            );
            let app = Anwesen::new(args.vault, args.bind, telemetry.clone());
            let started = app.started.clone();
            // Blocks until the supervisor exits (SIGTERM / SIGINT / crash).
            app.run();
            // Flush and shut down exporters after the server loop returns.
            if let Some(telemetry) = telemetry {
                telemetry.shutdown();
            }
            // `run` returns normally whether the tree came up or never
            // started, so a failed start would otherwise look like a clean
            // exit and systemd's `Restart=on-failure` would not retry
            // (ANW-45). Exit nonzero when the tree never came up.
            if !started.load(Ordering::Acquire) {
                tracing::error!("anwesen serve: supervisor tree failed to start");
                std::process::exit(1);
            }
        }
        Command::Doctor(args) => {
            init_logging(args.log_level);
            let (rendered, exit) = doctor::run_and_render(&args.vault);
            // Render to stdout so the report is pipe-friendly; logs go to
            // stderr via the tracing subscriber.
            print!("{rendered}");
            std::process::exit(exit);
        }
        Command::Merge(args) => {
            init_logging(args.log_level);
            match oneshot::merge(&args.vault, &args.query) {
                // `print!`, not `println!`: the merged document is byte-stable
                // and byte-identical to the HTTP merge body, which carries no
                // trailing newline. An empty match set prints nothing, exit 0.
                Ok(doc) => print!("{doc}"),
                Err(e) => {
                    eprint!("{}", e.render("merge"));
                    std::process::exit(1);
                }
            }
        }
        Command::Query(args) => {
            init_logging(args.log_level);
            match oneshot::query_json(&args.vault, &args.query) {
                // `println!` here, unlike `merge`: the JSON body carries no
                // trailing newline either, but stdout is one document per
                // line for the shells and `jq` pipelines this exists for.
                Ok(doc) => println!("{doc}"),
                Err(e) => {
                    eprint!("{}", e.render("query"));
                    std::process::exit(1);
                }
            }
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
