//! CLI surface for the `anwesen` binary.
//!
//! Mirrors the contract pinned in the User Manual:
//!
//! ```text
//! anwesen serve  --vault <path> [--bind <addr:port>] [--log-level <level>]
//! anwesen doctor --vault <path> [--log-level <level>]
//! anwesen version
//! ```
//!
//! `--bind` is `serve`-only; `doctor` does not bind a port; `version` takes
//! no flags. Each flag has a matching `ANWESEN_<UPPER>` environment variable
//! and CLI wins over env per the manual.

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
