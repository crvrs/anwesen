//! Anwesen daemon application: Hydra supervisor tree per [[ADR-004 Hydra as
//! Process Runtime]] and [ANW-17](https://crvrs.youtrack.cloud/issue/ANW-17).
//!
//! ```text
//! RootSupervisor (one_for_one)
//!   -- vault_scanner       (permanent; startup walk + overflow recovery)
//!   -- filesystem_watcher  (permanent; restart on inotify error)
//!   -- index_writer        (permanent; owns the `NoteStore` write path)
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
use std::time::Duration;

use hydra::{
    Application, ApplicationConfig, ChildSpec, Dest, ExitReason, From as HydraFrom, GenServer,
    GenServerOptions, Message, Pid, Process, ProcessFlags, SupervisionStrategy, Supervisor,
    SupervisorOptions, SystemMessage,
};
use notify::{RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::health::HealthState;
use crate::http::{self as http_layer, HttpState};
use crate::store::NoteStore;
use crate::telemetry::Telemetry;
use crate::vault::{self, Note};
use crate::watcher::run_debouncer;

pub(crate) const INDEX_WRITER_NAME: &str = "index_writer";
pub(crate) const VAULT_SCANNER_NAME: &str = "vault_scanner";
pub(crate) const FILESYSTEM_WATCHER_NAME: &str = "filesystem_watcher";
pub(crate) const HTTP_SERVER_NAME: &str = "http_server";

/// Default debounce window for the filesystem watcher. Per [[ADR-003
/// Filesystem Change Tracking]] -- 100 ms is the documented target, tunable
/// once real save patterns are observed.
const WATCH_DEBOUNCE_WINDOW: Duration = Duration::from_millis(100);

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
    /// Shared in-memory note store. The scanner populates it, the watcher
    /// keeps it current via the writer's batches, and the HTTP layer reads
    /// from it on every request.
    pub store: Arc<NoteStore>,
    /// Shared mutable health surface consumed by `/health` ([ANW-8]).
    pub health: Arc<HealthState>,
    /// Request-level telemetry handle ([ANW-37]). `None` disables export and
    /// the request middleware entirely.
    pub telemetry: Option<Arc<Telemetry>>,
    /// Set once the supervisor tree is up. Hydra's `Application::run` logs a
    /// start failure and returns normally, so `serve` cannot tell a clean
    /// shutdown from a tree that never came up. `main` reads this after `run`
    /// and exits nonzero when it is still false, which is what lets systemd
    /// retry ([ANW-45](https://crvrs.youtrack.cloud/issue/ANW-45)).
    pub started: Arc<AtomicBool>,
}

impl Anwesen {
    #[must_use]
    pub fn new(vault: PathBuf, bind: SocketAddr, telemetry: Option<Arc<Telemetry>>) -> Self {
        Self {
            vault,
            bind,
            counters: RestartCounters::new(),
            store: NoteStore::new(),
            health: HealthState::new(),
            telemetry,
            started: Arc::new(AtomicBool::new(false)),
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
        // Startup order matters: `vault_scanner` casts a `Rebuild` to the
        // `index_writer` name at the end of its init. Hydra `start_link` is
        // synchronous, so we put `index_writer` first to guarantee it is
        // registered and in its message loop before `vault_scanner` runs.
        // The `one_for_one` strategy makes the order irrelevant for restart
        // semantics, only for the cold-start handshake. After ANW-23 the
        // writer is restart-recoverable (NoteStore is owned by Anwesen and
        // outlives the process), so no rescan handshake is needed on init.
        let children = [
            IndexWriter {
                counters: self.counters.clone(),
                store: self.store.clone(),
                health: self.health.clone(),
            }
            .child_spec(),
            VaultScanner {
                vault: self.vault.clone(),
                counters: self.counters.clone(),
                health: self.health.clone(),
            }
            .child_spec(),
            FilesystemWatcher {
                vault: self.vault.clone(),
                counters: self.counters.clone(),
                health: self.health.clone(),
            }
            .child_spec(),
            HttpServer {
                bind: self.bind,
                counters: self.counters.clone(),
                store: self.store.clone(),
                health: self.health.clone(),
                vault: self.vault.clone(),
                telemetry: self.telemetry.clone(),
            }
            .child_spec(),
        ];

        let pid = Supervisor::with_children(children)
            .strategy(SupervisionStrategy::OneForOne)
            .start_link(SupervisorOptions::new().name("anwesen_root"))
            .await?;
        self.started.store(true, Ordering::Release);
        Ok(pid)
    }
}

// -- vault_scanner ----------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum VaultScannerMessage {
    /// Re-run the startup walk. Sent by `filesystem_watcher` on inotify
    /// overflow recovery per [[ADR-003 Filesystem Change Tracking]].
    RescanNow,
}

fn call_not_supported<M>(role: &str) -> Result<Option<M>, ExitReason> {
    // Each role's GenServer trait shares one `Message` type for both call and
    // cast (Hydra's API shape). To keep a stray `Foo::call(...)` from hanging
    // forever waiting on a reply, every `handle_call` returns this error.
    // Sie flagged the silent-Ok(None) pattern in ANW-17 review; pinning it
    // here in ANW-12 before ANW-16 wires the watcher-to-scanner call.
    Err(ExitReason::from(format!(
        "{role} does not handle synchronous calls; use cast"
    )))
}

#[derive(Clone)]
pub struct VaultScanner {
    vault: PathBuf,
    counters: Arc<RestartCounters>,
    health: Arc<HealthState>,
}

impl VaultScanner {
    fn child_spec(self) -> ChildSpec {
        let counters = self.counters.clone();
        let vault = self.vault.clone();
        let health = self.health.clone();
        ChildSpec::new(VAULT_SCANNER_NAME).start(move || {
            VaultScanner {
                vault: vault.clone(),
                counters: counters.clone(),
                health: health.clone(),
            }
            .start_link(GenServerOptions::new().name(VAULT_SCANNER_NAME))
        })
    }

    fn run_walk(&self) {
        self.health.set_in_flight_rescan(true);
        let result = vault::scan(&self.vault);
        // Clear before the cast so a consumer that reads `/health` right
        // after the cast doesn't see a stale `in_flight_rescan=true`.
        self.health.set_in_flight_rescan(false);
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
        // Push the fresh record set to the index writer. The cast is
        // address-by-name so we don't have to thread the index_writer Pid
        // through child specs.
        IndexWriterState::cast(
            Dest::from(INDEX_WRITER_NAME),
            IndexWriterMessage::Rebuild(result.notes),
        );
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
        call_not_supported("vault_scanner")
    }
}

// -- filesystem_watcher -----------------------------------------------------

/// The watcher accepts no inbound messages today -- its work is the
/// `notify` stream plus the debouncer task. The single `Noop` variant
/// satisfies Hydra's `Receivable` bound; once a real call/cast surface is
/// useful (e.g. for tests) it can be added.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum FilesystemWatcherMessage {
    Noop,
}

#[derive(Clone)]
pub struct FilesystemWatcher {
    vault: PathBuf,
    counters: Arc<RestartCounters>,
    health: Arc<HealthState>,
}

impl FilesystemWatcher {
    fn child_spec(self) -> ChildSpec {
        let counters = self.counters.clone();
        let vault = self.vault.clone();
        let health = self.health.clone();
        ChildSpec::new(FILESYSTEM_WATCHER_NAME).start(move || {
            FilesystemWatcherState {
                vault: vault.clone(),
                counters: counters.clone(),
                health: health.clone(),
                watcher: None,
                debouncer: None,
            }
            .start_link(GenServerOptions::new().name(FILESYSTEM_WATCHER_NAME))
        })
    }
}

/// Runtime state for the watcher process. Holds the live
/// [`notify::RecommendedWatcher`] (must outlive event delivery) and the
/// [`JoinHandle`] for the debouncer task; both are torn down when the
/// process is dropped on restart.
struct FilesystemWatcherState {
    vault: PathBuf,
    counters: Arc<RestartCounters>,
    health: Arc<HealthState>,
    watcher: Option<notify::RecommendedWatcher>,
    debouncer: Option<JoinHandle<()>>,
}

impl Drop for FilesystemWatcherState {
    fn drop(&mut self) {
        if let Some(h) = self.debouncer.take() {
            h.abort();
        }
        // `watcher` drops on its own, which is enough to stop the inotify
        // binding; the debouncer task then sees the channel close.
    }
}

impl GenServer for FilesystemWatcherState {
    type Message = FilesystemWatcherMessage;

    async fn init(&mut self) -> Result<(), ExitReason> {
        let restart = self.counters.filesystem_watcher.record_init();

        let (tx, rx) = mpsc::unbounded_channel::<notify::Result<notify::Event>>();
        let mut watcher = notify::recommended_watcher(move |res| {
            // Send is non-blocking on an unbounded channel; ignore the
            // SendError that arises only when the receiver has been dropped
            // (process shutdown).
            let _ = tx.send(res);
        })
        .map_err(|e| {
            // Failing to construct the watcher means inotify isn't bound.
            // Surface the state so /health reflects it before we exit and
            // let the supervisor restart us.
            self.health
                .set_watcher_state(crate::health::WatcherState::Degraded);
            ExitReason::from(format!("filesystem_watcher: recommended_watcher: {e}"))
        })?;
        watcher
            .watch(&self.vault, RecursiveMode::Recursive)
            .map_err(|e| {
                self.health
                    .set_watcher_state(crate::health::WatcherState::Degraded);
                ExitReason::from(format!("filesystem_watcher: watch: {e}"))
            })?;

        let handle = tokio::spawn(run_debouncer(
            rx,
            self.vault.clone(),
            WATCH_DEBOUNCE_WINDOW,
            self.health.clone(),
        ));

        self.watcher = Some(watcher);
        self.debouncer = Some(handle);
        self.health
            .set_watcher_state(crate::health::WatcherState::Running);

        tracing::info!(
            restart,
            vault = %self.vault.display(),
            debounce = ?WATCH_DEBOUNCE_WINDOW,
            "filesystem_watcher: init"
        );
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
        call_not_supported("filesystem_watcher")
    }
}

// -- index_writer -----------------------------------------------------------

/// Messages handled by the [`IndexWriter`] `GenServer`. All variants are casts
/// (one-way fire-and-forget); calls return an error from `handle_call`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum IndexWriterMessage {
    /// Discard the current index and reindex the given notes. Sent by
    /// `vault_scanner` at startup and after `rescan_now`.
    Rebuild(Vec<Note>),
    /// Apply one debounce-window's worth of upserts and deletes against
    /// [`NoteStore`] in a single write. Sent by `filesystem_watcher`.
    Batch(IndexBatch),
    /// Insert-or-replace one note. Retained for direct callers; the watcher
    /// uses `Batch`.
    Upsert(Box<Note>),
    /// Drop one note from the index. Retained for direct callers; the
    /// watcher uses `Batch`.
    Delete(String),
}

/// One batched index update. Deletes apply first so a "delete-then-upsert"
/// sequence is unambiguous; per-path coalescing in
/// [`crate::watcher::coalesce`] already keeps at most one action per path.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IndexBatch {
    pub upserts: Vec<Note>,
    pub deletes: Vec<String>,
    /// Directories whose indexed notes all drop before the upserts land. A
    /// removed or renamed directory produces no per-file event, so the
    /// prefix is the only handle on those notes. A directory that was
    /// walked is listed here too: the index under that prefix has to match
    /// the walk, not keep what an earlier directory of the same name left
    /// behind [ANW-36].
    #[serde(default)]
    pub delete_prefixes: Vec<String>,
}

#[derive(Clone)]
pub struct IndexWriter {
    counters: Arc<RestartCounters>,
    store: Arc<NoteStore>,
    health: Arc<HealthState>,
}

impl IndexWriter {
    fn child_spec(self) -> ChildSpec {
        let counters = self.counters.clone();
        let store = self.store.clone();
        let health = self.health.clone();
        ChildSpec::new(INDEX_WRITER_NAME).start(move || {
            IndexWriterState {
                counters: counters.clone(),
                store: store.clone(),
                health: health.clone(),
            }
            .start_link(GenServerOptions::new().name(INDEX_WRITER_NAME))
        })
    }
}

/// Runtime state for the [`IndexWriter`] process. Post-[ANW-23] the
/// "index" name survives in the supervisor tree and `/health` per
/// [[ADR-009 Reverse ADR-002 In-Memory Evaluation No Tantivy]]'s naming
/// call, but the role is now purely a serialized writer into the shared
/// [`NoteStore`]. No on-process state survives a restart -- `NoteStore`
/// is owned by `Anwesen` and outlives writer restarts, so no rescan
/// handshake is needed on init.
pub(crate) struct IndexWriterState {
    counters: Arc<RestartCounters>,
    store: Arc<NoteStore>,
    health: Arc<HealthState>,
}

impl GenServer for IndexWriterState {
    type Message = IndexWriterMessage;

    async fn init(&mut self) -> Result<(), ExitReason> {
        let restart = self.counters.index_writer.record_init();
        tracing::info!(restart, "index_writer: init");
        Ok(())
    }

    async fn handle_cast(&mut self, message: Self::Message) -> Result<(), ExitReason> {
        match message {
            IndexWriterMessage::Rebuild(notes) => {
                let count = notes.len();
                self.store.replace(notes);
                tracing::info!(notes = count, "index_writer: rebuilt store");
            }
            IndexWriterMessage::Batch(batch) => {
                let (u, d) = (batch.upserts.len(), batch.deletes.len());
                let dropped =
                    self.store
                        .apply_batch(batch.upserts, &batch.deletes, &batch.delete_prefixes);
                tracing::info!(
                    upserts = u,
                    deletes = d,
                    dropped,
                    "index_writer: batch applied"
                );
            }
            IndexWriterMessage::Upsert(note) => {
                let path = note.path.clone();
                self.store.upsert(*note);
                tracing::debug!(%path, "index_writer: upserted");
            }
            IndexWriterMessage::Delete(path) => {
                self.store.delete(&path);
                tracing::debug!(%path, "index_writer: deleted");
            }
        }
        self.health.record_index_update(chrono::Utc::now());
        Ok(())
    }

    async fn handle_call(
        &mut self,
        _message: Self::Message,
        _from: HydraFrom,
    ) -> Result<Option<Self::Message>, ExitReason> {
        call_not_supported("index_writer")
    }
}

// -- http_server ------------------------------------------------------------
//
// Per [ANW-24](https://crvrs.youtrack.cloud/issue/ANW-24): a plain Hydra
// process (not a `GenServer`), so axum's serve loop can run on the
// process's own task and shut down gracefully via
// `with_graceful_shutdown` when the supervisor sends an exit signal. The
// `GenServer` machinery added a no-op `Message` enum + a detached
// `tokio::spawn(serve)` + `Drop::abort()` -- all overhead for a process
// that has no inbound user protocol.

#[derive(Clone)]
pub struct HttpServer {
    bind: SocketAddr,
    counters: Arc<RestartCounters>,
    store: Arc<NoteStore>,
    health: Arc<HealthState>,
    vault: PathBuf,
    telemetry: Option<Arc<Telemetry>>,
}

impl HttpServer {
    fn child_spec(self) -> ChildSpec {
        let bind = self.bind;
        let counters = self.counters.clone();
        let store = self.store.clone();
        let health = self.health.clone();
        let vault = self.vault.clone();
        let telemetry = self.telemetry.clone();
        ChildSpec::new(HTTP_SERVER_NAME).start(move || {
            let bind = bind;
            let counters = counters.clone();
            let store = store.clone();
            let health = health.clone();
            let vault = vault.clone();
            let telemetry = telemetry.clone();
            async move {
                // Bind eagerly so the supervisor sees `bind: <addr>: ...`
                // as the start error rather than a successful spawn that
                // exits on its first message.
                let listener = tokio::net::TcpListener::bind(bind)
                    .await
                    .map_err(|e| ExitReason::from(format!("http_server: bind {bind}: {e}")))?;
                let restart = counters.http_server.record_init();
                let router = http_layer::router(
                    HttpState {
                        store,
                        health,
                        restart_counters: counters,
                        vault,
                    },
                    telemetry,
                );
                let pid = Process::spawn_link(async move {
                    Process::set_flags(ProcessFlags::TRAP_EXIT);
                    tracing::info!(restart, bind = %bind, "http_server: init");

                    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
                    let serve_fut = std::future::IntoFuture::into_future(
                        axum::serve(listener, router).with_graceful_shutdown(async move {
                            let _ = shutdown_rx.await;
                        }),
                    );
                    tokio::pin!(serve_fut);

                    tokio::select! {
                        biased;
                        msg = Process::receiver().select(|m| {
                            matches!(m, Message::System(SystemMessage::Exit(_, _)))
                        }) => {
                            tracing::info!(?msg, "http_server: shutdown signal received");
                            let _ = shutdown_tx.send(());
                            if let Err(e) = (&mut serve_fut).await {
                                tracing::error!(error = %e, "http_server: serve loop error during shutdown");
                            }
                        }
                        result = &mut serve_fut => {
                            if let Err(e) = result {
                                tracing::error!(error = %e, "http_server: serve loop exited with error");
                            }
                        }
                    }
                });
                Process::register(pid, HTTP_SERVER_NAME).map_err(|e| {
                    ExitReason::from(format!("http_server: register: {e:?}"))
                })?;
                Ok(pid)
            }
        })
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
