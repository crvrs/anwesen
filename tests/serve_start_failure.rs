//! `anwesen serve` must exit nonzero when the supervisor tree fails to start
//! ([ANW-45](https://crvrs.youtrack.cloud/issue/ANW-45)). Hydra's
//! `Application::run` logs the failure and returns normally, so without an
//! explicit check the process exits 0 and systemd's `Restart=on-failure`
//! never retries -- the vault stayed unreachable for 12 hours on ap.
//!
//! The forced failure is a taken bind address: `http_server` binds eagerly in
//! its child spec, so the address-in-use error fails the whole start.

use std::net::TcpListener;
use std::process::Command;

#[test]
fn serve_exits_nonzero_when_the_tree_fails_to_start() {
    let vault = tempfile::tempdir().expect("tempdir");
    // Hold the port for the lifetime of the child so its bind cannot succeed.
    let held = TcpListener::bind("127.0.0.1:0").expect("bind probe port");
    let addr = held.local_addr().expect("probe addr");

    let status = Command::new(env!("CARGO_BIN_EXE_anwesen"))
        .arg("serve")
        .arg("--vault")
        .arg(vault.path())
        .arg("--bind")
        .arg(addr.to_string())
        .status()
        .expect("spawn anwesen serve");

    assert!(
        !status.success(),
        "serve exited {status} after a failed supervisor start; systemd reads that as a clean exit"
    );
}
