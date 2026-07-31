//! CLI surface for the `anwesen` binary.
//!
//! Mirrors the contract pinned in the User Manual:
//!
//! ```text
//! anwesen serve  --vault <path> [--bind <addr:port>] [--log-level <level>]
//!                [--otlp-slow-request-ms <n>]
//! anwesen doctor --vault <path> [--log-level <level>]
//! anwesen merge  --vault <path> [--query <string>] [--log-level <level>]
//! anwesen query  --vault <path> [--query <string>] [--log-level <level>]
//! anwesen version
//! ```
//!
//! `merge` and `query` differ only in what they write -- the merged markdown
//! document or the `/query` JSON projection ([ANW-43]) -- so they share one
//! [`VaultQueryArgs`] flag set and cannot drift.
//!
//! `--bind` and `--otlp-slow-request-ms` are `serve`-only ([ANW-37]);
//! `doctor` and `merge` do not bind a port; `version` takes no flags. Each
//! flag has a matching `ANWESEN_<UPPER>` environment variable and CLI wins
//! over env per the manual.
//!
//! Telemetry export is configured through the standard `OTEL_EXPORTER_OTLP_*`
//! environment variables ([ANW-42](https://crvrs.youtrack.cloud/issue/ANW-42)).
//! With none of them set, telemetry is off and the server behaves as it did
//! before telemetry existed. The removed flags below are still parsed so an
//! upgrade fails loudly instead of dropping export config in silence.

use std::net::SocketAddr;
use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(name = "anwesen", version, about, long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run the read-only HTTP daemon.
    Serve(ServeArgs),
    /// Walk the vault once and report ingestion blockers. Read-only.
    Doctor(DoctorArgs),
    /// Walk the vault once, evaluate the query, and write the merged markdown
    /// document to stdout. One-shot; no server. Read-only.
    Merge(VaultQueryArgs),
    /// Walk the vault once, evaluate the query, and write the same JSON
    /// document `GET /query` returns to stdout. One-shot; no server.
    /// Read-only.
    Query(VaultQueryArgs),
    /// Print the version and exit.
    Version,
}

#[derive(Debug, clap::Args)]
pub struct ServeArgs {
    /// Path to the vault root.
    #[arg(long, env = "ANWESEN_VAULT")]
    pub vault: PathBuf,

    /// Listen address for the HTTP server.
    #[arg(long, env = "ANWESEN_BIND", default_value = "127.0.0.1:8080")]
    pub bind: SocketAddr,

    /// Log verbosity.
    #[arg(long, env = "ANWESEN_LOG_LEVEL", default_value = "info")]
    pub log_level: LogLevel,

    /// Removed (ANW-42): use `OTEL_EXPORTER_OTLP_ENDPOINT` plus
    /// `OTEL_EXPORTER_OTLP_HEADERS=uptrace-dsn=<dsn>`. Still accepted so
    /// startup fails with that message rather than exporting nowhere.
    #[arg(long, env = "ANWESEN_UPTRACE_DSN", hide = true)]
    pub uptrace_dsn: Option<String>,

    /// Removed (ANW-42): use `OTEL_EXPORTER_OTLP_ENDPOINT`.
    #[arg(long, env = "ANWESEN_OTLP_ENDPOINT", hide = true)]
    pub otlp_endpoint: Option<String>,

    /// Removed (ANW-42): use `OTEL_EXPORTER_OTLP_HEADERS`.
    #[arg(
        long = "otlp-header",
        env = "ANWESEN_OTLP_HEADERS",
        value_delimiter = ',',
        hide = true
    )]
    pub otlp_headers: Vec<String>,

    /// Requests at or over this duration in milliseconds, or answering a
    /// 5xx, are additionally recorded as OTLP server spans; every other
    /// request stays metrics-only.
    #[arg(long, env = "ANWESEN_OTLP_SLOW_REQUEST_MS", default_value = "500")]
    pub otlp_slow_request_ms: u64,
}

#[derive(Debug, clap::Args)]
pub struct DoctorArgs {
    /// Path to the vault root.
    #[arg(long, env = "ANWESEN_VAULT")]
    pub vault: PathBuf,

    /// Log verbosity.
    #[arg(long, env = "ANWESEN_LOG_LEVEL", default_value = "info")]
    pub log_level: LogLevel,
}

/// Flags shared by the two one-shot subcommands, `merge` and `query`.
#[derive(Debug, clap::Args)]
pub struct VaultQueryArgs {
    /// Path to the vault root.
    #[arg(long, env = "ANWESEN_VAULT")]
    pub vault: PathBuf,

    /// Query in the `/query` query-string grammar, for example
    /// `tags=anwesen&__anw-kind=skill&__anw-order=order`. The `__anw-kind`
    /// homogeneity guard and `__anw-order` fragment ordering ride inside this
    /// string -- there are no separate flags. `__anw-kind` and `__anw-order`
    /// apply to `merge` only. Empty matches every note under the vault root.
    #[arg(long, env = "ANWESEN_QUERY", default_value = "")]
    pub query: String,

    /// Log verbosity. Logs go to stderr; the document goes to stdout.
    #[arg(long, env = "ANWESEN_LOG_LEVEL", default_value = "info")]
    pub log_level: LogLevel,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
#[value(rename_all = "lower")]
pub enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl LogLevel {
    pub fn as_filter_directive(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warn => "warn",
            Self::Info => "info",
            Self::Debug => "debug",
            Self::Trace => "trace",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("anwesen").chain(args.iter().copied()))
    }

    #[test]
    fn serve_requires_vault() {
        assert!(parse(&["serve"]).is_err());
    }

    #[test]
    fn serve_accepts_full_flag_set() {
        let cli = parse(&[
            "serve",
            "--vault",
            "/tmp/v",
            "--bind",
            "0.0.0.0:9000",
            "--log-level",
            "debug",
        ])
        .expect("parse");
        match cli.command {
            Command::Serve(a) => {
                assert_eq!(a.vault, PathBuf::from("/tmp/v"));
                assert_eq!(a.bind, "0.0.0.0:9000".parse::<SocketAddr>().unwrap());
                assert!(matches!(a.log_level, LogLevel::Debug));
            }
            _ => panic!("expected serve"),
        }
    }

    /// The removed telemetry flags still parse, hidden from `--help`. Clap
    /// rejecting them as unknown would say nothing about the `OTEL_`
    /// replacement; the migration error in `telemetry::TelemetryConfig`
    /// needs the values to reach it (ANW-42).
    #[test]
    fn serve_still_parses_the_removed_telemetry_flags() {
        let cli = parse(&[
            "serve",
            "--vault",
            "/tmp/v",
            "--uptrace-dsn",
            "https://tok@api.uptrace.dev",
            "--otlp-endpoint",
            "https://collector.example.com",
            "--otlp-header",
            "authorization=Bearer xyz",
        ])
        .expect("parse");
        match cli.command {
            Command::Serve(a) => {
                assert!(a.uptrace_dsn.is_some());
                assert!(a.otlp_endpoint.is_some());
                assert_eq!(a.otlp_headers, vec!["authorization=Bearer xyz".to_string()]);
            }
            _ => panic!("expected serve"),
        }
    }

    #[test]
    fn serve_rejects_malformed_bind_at_parse_time() {
        let err = parse(&["serve", "--vault", "/tmp/v", "--bind", "not-an-addr"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::ValueValidation);
    }

    #[test]
    fn doctor_rejects_bind() {
        // --bind is serve-only; doctor must not accept it.
        let err = parse(&["doctor", "--vault", "/tmp/v", "--bind", "0.0.0.0:9000"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::UnknownArgument);
    }

    #[test]
    fn doctor_accepts_vault_and_log_level() {
        let cli = parse(&["doctor", "--vault", "/tmp/v", "--log-level", "warn"]).expect("parse");
        match cli.command {
            Command::Doctor(a) => {
                assert_eq!(a.vault, PathBuf::from("/tmp/v"));
                assert!(matches!(a.log_level, LogLevel::Warn));
            }
            _ => panic!("expected doctor"),
        }
    }

    #[test]
    fn merge_requires_vault() {
        assert!(parse(&["merge"]).is_err());
    }

    #[test]
    fn merge_query_defaults_to_empty() {
        let cli = parse(&["merge", "--vault", "/tmp/v"]).expect("parse");
        match cli.command {
            Command::Merge(a) => {
                assert_eq!(a.vault, PathBuf::from("/tmp/v"));
                assert_eq!(a.query, "");
            }
            _ => panic!("expected merge"),
        }
    }

    #[test]
    fn merge_accepts_vault_query_and_log_level() {
        let cli = parse(&[
            "merge",
            "--vault",
            "/tmp/v",
            "--query",
            "tags=anwesen&__anw-order=order",
            "--log-level",
            "warn",
        ])
        .expect("parse");
        match cli.command {
            Command::Merge(a) => {
                assert_eq!(a.query, "tags=anwesen&__anw-order=order");
                assert!(matches!(a.log_level, LogLevel::Warn));
            }
            _ => panic!("expected merge"),
        }
    }

    #[test]
    fn merge_rejects_bind() {
        // --bind is serve-only; merge must not accept it.
        let err = parse(&["merge", "--vault", "/tmp/v", "--bind", "0.0.0.0:9000"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::UnknownArgument);
    }

    #[test]
    fn query_takes_the_same_flags_as_merge() {
        let cli = parse(&[
            "query",
            "--vault",
            "/tmp/v",
            "--query",
            "tags=anwesen&__anw-limit=5",
            "--log-level",
            "warn",
        ])
        .expect("parse");
        match cli.command {
            Command::Query(a) => {
                assert_eq!(a.vault, PathBuf::from("/tmp/v"));
                assert_eq!(a.query, "tags=anwesen&__anw-limit=5");
                assert!(matches!(a.log_level, LogLevel::Warn));
            }
            _ => panic!("expected query"),
        }
    }

    #[test]
    fn query_requires_vault_and_defaults_the_query() {
        assert!(parse(&["query"]).is_err());
        match parse(&["query", "--vault", "/tmp/v"])
            .expect("parse")
            .command
        {
            Command::Query(a) => assert_eq!(a.query, ""),
            _ => panic!("expected query"),
        }
    }

    #[test]
    fn query_rejects_bind() {
        // --bind is serve-only; query does not listen.
        let err = parse(&["query", "--vault", "/tmp/v", "--bind", "0.0.0.0:9000"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::UnknownArgument);
    }

    #[test]
    fn version_takes_no_flags() {
        assert!(matches!(
            parse(&["version"]).expect("parse").command,
            Command::Version
        ));
        // version subcommand must reject any flag.
        assert!(parse(&["version", "--vault", "/tmp/v"]).is_err());
    }

    #[test]
    fn unknown_subcommand_rejected() {
        assert!(parse(&["dance"]).is_err());
    }
}
