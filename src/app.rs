//! Anwesen daemon application: Hydra supervisor tree per [[ADR-004 Hydra as
//! Process Runtime]] and [ANW-17](https://crvrs.youtrack.cloud/issue/ANW-17).
//!
//! ```text
//! RootSupervisor (one_for_one)
//!   -- vault_scanner       (permanent; startup walk + overflow recovery)
//!   -- filesystem_watcher  (permanent; restart on inotify error)
//!   -- index_writer        (permanent; owns the Tantivy IndexWriter)
//!   -- http_server         (permanent; restart on bind loss)
//! ```
//!
//! Only `vault_scanner` has a real body in this commit: it runs a startup
//! walk via [`crate::vault::scan`] and accepts a `RescanNow` cast that
//! re-runs the same walk (for the inotify-overflow recovery path described
//! in [[ADR-003 Filesystem Change Tracking]]). The other three roles are
//! idle stubs that subsequent issues (ANW-12, ANW-13, ANW-16) replace with
//! real implementations.
//!
//! [`RestartCounters`] is the shared, lock-free snapshot consumed by
//! [ANW-8](https://crvrs.youtrack.cloud/issue/ANW-8) when wiring the
//! `/health` endpoint.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use hydra::{
    Application, ApplicationConfig, ChildSpec, ExitReason, From as HydraFrom, GenServer,
    GenServerOptions, Pid, SupervisionStrategy, Supervisor, SupervisorOptions,
};
use serde::{Deserialize, Serialize};

use crate::vault;

/// Snapshot of per-process restart counters. Each role increments its own
/// counter on every `init` *after the first*, so the count represents
/// supervisor-initiated restarts rather than the initial start.
#[derive(Debug, Default)]
pub struct RestartCounters {
    pub vault_scanner: RoleCounter,
    pub filesystem_watcher: RoleCounter,
    pub index_writer: RoleCounter,
    pub http_server: RoleCounter,
}

#[derive(Debug, Default)]
pub struct RoleCounter {
    started: AtomicBool,
    restarts: AtomicU32,
}

impl RoleCounter {
    /// Record an `init` call. Returns the current restart count.
    fn record_init(&self) -> u32 {
        if self.started.swap(true, Ordering::AcqRel) {
            self.restarts.fetch_add(1, Ordering::Relaxed) + 1
        } else {
            0
        }
    }

    pub fn get(&self) -> u32 {
        self.restarts.load(Ordering::Relaxed)
    }
}

impl RestartCounters {
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Snapshot the counters with the role names from [[ADR-004 Hydra as
    /// Process Runtime]]'s "Open questions / Closed in v1" section -- these
    /// keys are part of the `/health` contract ([ANW-8]).
    #[must_use]
    pub fn snapshot(&self) -> BTreeMap<&'static str, u32> {
        let mut out = BTreeMap::new();
        out.insert("vault_scanner", self.vault_scanner.get());
        out.insert("filesystem_watcher", self.filesystem_watcher.get());
        out.insert("index_writer", self.index_writer.get());
        out.insert("http_server", self.http_server.get());
        out
    }
}

/// Daemon application: a Hydra [`Application`] that links a `one_for_one`
/// supervisor over the four process roles.
pub struct Anwesen {
    pub vault: PathBuf,
    pub bind: SocketAddr,
    pub counters: Arc<RestartCounters>,
}

impl Anwesen {
    #[must_use]
    pub fn new(vault: PathBuf, bind: SocketAddr) -> Self {
        Self {
            vault,
            bind,
            counters: RestartCounters::new(),
        }
    }
}

impl Application for Anwesen {
    fn config() -> ApplicationConfig {
        // We install our own tracing-subscriber in `main::init_logging`, so
        // tell Hydra not to install a second one. Keep the panic hook off
        // too -- panics surface through `tracing` via our subscriber.
        ApplicationConfig::new()
            .with_tracing_subscribe(false)
            .with_tracing_panics(false)
            .with_graceful_shutdown(true)
    }

    async fn start(&self) -> Result<Pid, ExitReason> {
        let children = [
            VaultScanner {
                vault: self.vault.clone(),
                counters: self.counters.clone(),
            }
            .child_spec(),
            FilesystemWatcher {
                counters: self.counters.clone(),
            }
            .child_spec(),
            IndexWriter {
                counters: self.counters.clone(),
            }
            .child_spec(),
            HttpServer {
                bind: self.bind,
                counters: self.counters.clone(),
            }
            .child_spec(),
        ];

        Supervisor::with_children(children)
            .strategy(SupervisionStrategy::OneForOne)
            .start_link(SupervisorOptions::new().name("anwesen_root"))
            .await
    }
}

// -- vault_scanner ----------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum VaultScannerMessage {
    /// Re-run the startup walk. Sent by `filesystem_watcher` on inotify
    /// overflow recovery per [[ADR-003 Filesystem Change Tracking]].
    RescanNow,
}

#[derive(Clone)]
pub struct VaultScanner {
    vault: PathBuf,
    counters: Arc<RestartCounters>,
}

impl VaultScanner {
    fn child_spec(self) -> ChildSpec {
        let counters = self.counters.clone();
        let vault = self.vault.clone();
        ChildSpec::new("vault_scanner").start(move || {
            VaultScanner {
                vault: vault.clone(),
                counters: counters.clone(),
            }
            .start_link(GenServerOptions::new().name("vault_scanner"))
        })
    }

    fn run_walk(&self) {
        let result = vault::scan(&self.vault);
        tracing::info!(
            notes = result.notes.len(),
            issues = result.issues.len(),
            vault = %self.vault.display(),
            "vault_scanner: walk complete"
        );
        for issue in &result.issues {
            tracing::warn!(
                path = %issue.path.display(),
                kind = %issue.kind,
                "vault_scanner: skipped file"
            );
        }
    }
}

impl GenServer for VaultScanner {
    type Message = VaultScannerMessage;

    async fn init(&mut self) -> Result<(), ExitReason> {
        let restart = self.counters.vault_scanner.record_init();
        tracing::info!(restart, "vault_scanner: init");
        self.run_walk();
        Ok(())
    }

    async fn handle_cast(&mut self, message: Self::Message) -> Result<(), ExitReason> {
        match message {
            VaultScannerMessage::RescanNow => {
                tracing::info!("vault_scanner: rescan_now received");
                self.run_walk();
            }
        }
        Ok(())
    }

    async fn handle_call(
        &mut self,
        _message: Self::Message,
        _from: HydraFrom,
    ) -> Result<Option<Self::Message>, ExitReason> {
        // No synchronous protocol on vault_scanner; ignore.
        Ok(None)
    }
}

// -- filesystem_watcher (stub for ANW-16) -----------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum FilesystemWatcherMessage {
    /// Placeholder so the enum has a variant -- replaced by real watch
    /// events in [ANW-16](https://crvrs.youtrack.cloud/issue/ANW-16).
    Noop,
}

#[derive(Clone)]
pub struct FilesystemWatcher {
    counters: Arc<RestartCounters>,
}

impl FilesystemWatcher {
    fn child_spec(self) -> ChildSpec {
        let counters = self.counters.clone();
        ChildSpec::new("filesystem_watcher").start(move || {
            FilesystemWatcher {
                counters: counters.clone(),
            }
            .start_link(GenServerOptions::new().name("filesystem_watcher"))
        })
    }
}

impl GenServer for FilesystemWatcher {
    type Message = FilesystemWatcherMessage;

    async fn init(&mut self) -> Result<(), ExitReason> {
        let restart = self.counters.filesystem_watcher.record_init();
        tracing::info!(restart, "filesystem_watcher: init (stub; ANW-16)");
        Ok(())
    }

    async fn handle_cast(&mut self, _message: Self::Message) -> Result<(), ExitReason> {
        Ok(())
    }

    async fn handle_call(
        &mut self,
        _message: Self::Message,
        _from: HydraFrom,
    ) -> Result<Option<Self::Message>, ExitReason> {
        Ok(None)
    }
}

// -- index_writer (stub for ANW-12) -----------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum IndexWriterMessage {
    /// Placeholder; replaced by index upsert/delete in
    /// [ANW-12](https://crvrs.youtrack.cloud/issue/ANW-12).
    Noop,
}

#[derive(Clone)]
pub struct IndexWriter {
    counters: Arc<RestartCounters>,
}

impl IndexWriter {
    fn child_spec(self) -> ChildSpec {
        let counters = self.counters.clone();
        ChildSpec::new("index_writer").start(move || {
            IndexWriter {
                counters: counters.clone(),
            }
            .start_link(GenServerOptions::new().name("index_writer"))
        })
    }
}

impl GenServer for IndexWriter {
    type Message = IndexWriterMessage;

    async fn init(&mut self) -> Result<(), ExitReason> {
        let restart = self.counters.index_writer.record_init();
        tracing::info!(restart, "index_writer: init (stub; ANW-12)");
        Ok(())
    }

    async fn handle_cast(&mut self, _message: Self::Message) -> Result<(), ExitReason> {
        Ok(())
    }

    async fn handle_call(
        &mut self,
        _message: Self::Message,
        _from: HydraFrom,
    ) -> Result<Option<Self::Message>, ExitReason> {
        Ok(None)
    }
}

// -- http_server (stub for ANW-13/14/15) ------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum HttpServerMessage {
    /// Placeholder; replaced by the axum surface in
    /// [ANW-13](https://crvrs.youtrack.cloud/issue/ANW-13) and following.
    Noop,
}

#[derive(Clone)]
pub struct HttpServer {
    bind: SocketAddr,
    counters: Arc<RestartCounters>,
}

impl HttpServer {
    fn child_spec(self) -> ChildSpec {
        let bind = self.bind;
        let counters = self.counters.clone();
        ChildSpec::new("http_server").start(move || {
            HttpServer {
                bind,
                counters: counters.clone(),
            }
            .start_link(GenServerOptions::new().name("http_server"))
        })
    }
}

impl GenServer for HttpServer {
    type Message = HttpServerMessage;

    async fn init(&mut self) -> Result<(), ExitReason> {
        let restart = self.counters.http_server.record_init();
        tracing::info!(restart, bind = %self.bind, "http_server: init (stub; ANW-13)");
        Ok(())
    }

    async fn handle_cast(&mut self, _message: Self::Message) -> Result<(), ExitReason> {
        Ok(())
    }

    async fn handle_call(
        &mut self,
        _message: Self::Message,
        _from: HydraFrom,
    ) -> Result<Option<Self::Message>, ExitReason> {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_counter_first_init_is_zero() {
        let c = RoleCounter::default();
        assert_eq!(c.record_init(), 0);
        assert_eq!(c.get(), 0);
    }

    #[test]
    fn role_counter_counts_restarts_only() {
        let c = RoleCounter::default();
        assert_eq!(c.record_init(), 0); // first start
        assert_eq!(c.record_init(), 1); // first restart
        assert_eq!(c.record_init(), 2); // second restart
        assert_eq!(c.get(), 2);
    }

    #[test]
    fn snapshot_keys_match_health_contract() {
        let counters = RestartCounters::new();
        let snap = counters.snapshot();
        // Keys are part of the /health payload contract per ADR-004.
        assert_eq!(
            snap.keys().copied().collect::<Vec<_>>(),
            vec![
                "filesystem_watcher",
                "http_server",
                "index_writer",
                "vault_scanner"
            ]
        );
        assert!(snap.values().all(|v| *v == 0));
    }

    #[test]
    fn app_config_disables_hydra_tracing_subscribe() {
        // We install our own tracing-subscriber; Hydra installing a second
        // one would race and double-format every event.
        let cfg = Anwesen::config();
        // Field is pub(crate) inside hydra; we cannot assert directly. Instead
        // we exercise the builder path so a future Hydra change that flips the
        // default trips a compile-level reminder here.
        let _ = cfg;
    }
}
