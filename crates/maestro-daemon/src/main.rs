//! maestro-daemon binary: a thin wrapper over [`maestro_daemon::run`], which
//! starts the socket server and blocks until a termination signal (ADR-006).
//! All logic lives in the library so integration tests can drive the server
//! in-process.

fn main() -> anyhow::Result<()> {
    maestro_daemon::run()
}
