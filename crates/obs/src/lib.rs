// SPDX-License-Identifier: Apache-2.0

//! Process-level tracing subscriber for the executable entry points.
//!
//! Library crates emit spans and events through the `tracing` facade and
//! never install a subscriber. The two executables — `cli` and `mcp` —
//! call [`init`] once at startup to route those events: human-readable
//! lines to stderr, structured JSON lines to a rolling file under the
//! data root. Keeping the subscriber here means the heavyweight
//! `tracing-subscriber` and `tracing-appender` dependencies stay at the
//! entry points and out of every library crate.

use std::io;

use bookrack_config::{Config, LogConfig};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

/// Install the global subscriber and return its flush guard.
///
/// The console layer writes human-readable lines to **stderr**, leaving
/// stdout for command results so the two never interleave. The file layer
/// writes JSON lines to a daily-rolling `bookrack.log` under
/// [`Config::logs_dir`], created if it does not yet exist. The level
/// filter comes from [`LogConfig::directive`]; an unparseable directive is
/// dropped rather than fatal.
///
/// The returned [`WorkerGuard`] owns the non-blocking writer's background
/// thread. The caller must hold it for the program's lifetime — typically
/// `let _guard = bookrack_obs::init(&cfg, &log);` in `main` — so buffered
/// lines flush on exit.
pub fn init(cfg: &Config, log: &LogConfig) -> WorkerGuard {
    let logs_dir = cfg.logs_dir();
    // The data root is validated to exist, but its `logs/` subdirectory
    // may not; the appender does not create it, so do it here.
    let _ = std::fs::create_dir_all(&logs_dir);

    let file_appender = tracing_appender::rolling::daily(&logs_dir, "bookrack.log");
    let (file_writer, guard) = tracing_appender::non_blocking(file_appender);

    let console = fmt::layer().with_writer(io::stderr);
    let file = fmt::layer().json().with_writer(file_writer);

    tracing_subscriber::registry()
        .with(EnvFilter::new(&log.directive))
        .with(console)
        .with(file)
        .init();

    guard
}
