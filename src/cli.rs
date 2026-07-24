//! CLI surface for the `anwesen` binary.
//!
//! Mirrors the contract pinned in the User Manual:
//!
//! ```text
//! anwesen serve  --vault <path> [--bind <addr:port>] [--log-level <level>]
//!                [--uptrace-dsn <dsn> | --otlp-endpoint <url>]
//!                [--otlp-header <key=value>]... [--otlp-slow-request-ms <n>]
//! anwesen doctor --vault <path> [--log-level <level>]
//! anwesen merge  --vault <path> [--query <string>] [--log-level <level>]
//! anwesen version
//! ```
//!
//! `--bind` and the OTLP telemetry flags are `serve`-only ([ANW-37]);
//! `doctor` and `merge` do not bind a port; `version` takes no flags. Each
//! flag has a matching `ANWESEN_<UPPER>` environment variable and CLI wins
//! over env per the manual. With no `--otlp-endpoint`/`--uptrace-dsn`,
//! telemetry is off and the server behaves exactly as without these flags.

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
    Merge(MergeArgs),
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

    /// uptrace DSN shorthand (`https://<token>@api.uptrace.dev`), parsed
    /// into the OTLP endpoint plus an `uptrace-dsn` header. Mutually
    /// exclusive with --otlp-endpoint. When this and --otlp-endpoint are
    /// both unset, telemetry is fully off (ANW-37).
    #[arg(long, env = "ANWESEN_UPTRACE_DSN")]
    pub uptrace_dsn: Option<String>,

    /// Generic OTLP/HTTP endpoint base URL for telemetry export. The
    /// per-signal path (`/v1/metrics`, `/v1/traces`) is appended by the
    /// exporter. Mutually exclusive with --uptrace-dsn.
    #[arg(long, env = "ANWESEN_OTLP_ENDPOINT")]
    pub otlp_endpoint: Option<String>,

    /// Extra OTLP export header as `key=value`, repeatable. On the env var
    /// (`ANWESEN_OTLP_HEADERS`) pass a comma-separated `key=value` list.
    #[arg(
        long = "otlp-header",
        env = "ANWESEN_OTLP_HEADERS",
        value_delimiter = ','
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

#[derive(Debug, clap::Args)]
pub struct MergeArgs {
    /// Path to the vault root.
    #[arg(long, env = "ANWESEN_VAULT")]
    pub vault: PathBuf,

    /// Query in the `/query` query-string grammar, for example
    /// `tags=anwesen&__anw-kind=skill&__anw-order=order`. The `__anw-kind`
    /// homogeneity guard and `__anw-order` fragment ordering ride inside this
    /// string -- there are no separate flags. Empty merges every note under
    /// the vault root.
    #[arg(long, env = "ANWESEN_QUERY", default_value = "")]
    pub query: String,

    /// Log verbosity. Logs go to stderr; the merged document goes to stdout.
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
