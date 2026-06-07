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

use std::backtrace::Backtrace;
use std::io;
use std::panic::PanicHookInfo;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

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
/// When the logs directory cannot be created or is not writable, the file
/// layer is omitted and a notice is printed to stderr; stderr logging
/// continues. The returned guard is then `None`.
///
/// Also installs a panic hook that writes a crash report under the same
/// logs directory (see [`install_crash_hook`]).
///
/// The returned [`WorkerGuard`] owns the non-blocking writer's background
/// thread. The caller must hold it for the program's lifetime — typically
/// `let _guard = bookrack_obs::init(&cfg, &log);` in `main` — so buffered
/// lines flush on exit.
pub fn init(cfg: &Config, log: &LogConfig) -> Option<WorkerGuard> {
    let logs_dir = cfg.logs_dir();
    // The data root is validated to exist, but its `logs/` subdirectory
    // may not; the appender does not create it, so do it here.
    let logs_dir_writable = std::fs::create_dir_all(&logs_dir).is_ok() && probe_writable(&logs_dir);

    let (file_layer, guard) = if logs_dir_writable {
        let file_appender = tracing_appender::rolling::daily(&logs_dir, "bookrack.log");
        let (file_writer, worker_guard) = tracing_appender::non_blocking(file_appender);
        let file = fmt::layer().json().with_writer(file_writer);
        (Some(file), Some(worker_guard))
    } else {
        eprintln!(
            "bookrack: file logging disabled; {} is not writable",
            logs_dir.display()
        );
        (None, None)
    };

    let console = fmt::layer().with_writer(io::stderr);

    tracing_subscriber::registry()
        .with(EnvFilter::new(&log.directive))
        .with(console)
        .with(file_layer)
        .init();

    install_crash_hook(logs_dir);

    guard
}

/// Probe whether `dir` accepts file creation by writing and removing a
/// short-named sentinel file. Returns `true` on success.
fn probe_writable(dir: &Path) -> bool {
    let probe = dir.join(".bookrack-writable-probe");
    let opened = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&probe);
    match opened {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// Install a panic hook that writes a crash report to `logs_dir` before
/// chaining to the previously installed hook (so the default panic message
/// still reaches stderr).
///
/// The report carries what `std` can supply without extra dependencies:
/// the panic message and location, a backtrace, the build version, and the
/// host OS. Richer context the crash-tracking design calls for — a system
/// resource snapshot, GPU memory, the resolved config, and the failing
/// book's span fields — needs further crates and a span-capturing layer,
/// and is left for a follow-up.
fn install_crash_hook(logs_dir: PathBuf) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let message = panic_message(info);
        if is_broken_pipe_panic(&message) {
            std::process::exit(0);
        }
        let backtrace = Backtrace::force_capture().to_string();
        let location = info.location().map(|loc| loc.to_string());
        match write_crash_report(&logs_dir, &message, location.as_deref(), &backtrace) {
            Ok(path) => eprintln!("bookrack: crash report written to {}", path.display()),
            Err(e) => eprintln!("bookrack: could not write crash report: {e}"),
        }
        previous(info);
    }));
}

/// Recognise the panic raised by `std::io::stdio::_print` / `_eprint` when
/// the receiving end of stdout or stderr is gone — the typical
/// `bookrack ... | head` shape. The classifier accepts either casing of
/// `broken pipe` so it matches both the `io::Error` `Display` impl and the
/// raw OS message variants.
fn is_broken_pipe_panic(message: &str) -> bool {
    let needle = "broken pipe";
    let lower = message.to_ascii_lowercase();
    lower.contains(needle)
}

/// Extract a human-readable message from a panic payload, which is a
/// `&str` or `String` in the common cases.
fn panic_message(info: &PanicHookInfo<'_>) -> String {
    let payload = info.payload();
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_string()
    }
}

/// Write a crash report file under `logs_dir`, named by the wall-clock
/// time so concurrent panics do not clobber each other, and return its
/// path.
fn write_crash_report(
    logs_dir: &Path,
    message: &str,
    location: Option<&str>,
    backtrace: &str,
) -> io::Result<PathBuf> {
    std::fs::create_dir_all(logs_dir)?;
    let unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let path = logs_dir.join(format!("crash-{unix_ms}.txt"));
    std::fs::write(
        &path,
        render_crash_report(unix_ms, message, location, backtrace),
    )?;
    Ok(path)
}

/// Render the crash report body.
fn render_crash_report(
    unix_ms: u128,
    message: &str,
    location: Option<&str>,
    backtrace: &str,
) -> String {
    format!(
        "bookrack crash report\n\
         =====================\n\
         time (unix ms): {unix_ms}\n\
         version:        {}\n\
         os/arch:        {}/{}\n\
         location:       {}\n\
         panic:          {message}\n\
         \n\
         backtrace:\n{backtrace}\n",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH,
        location.unwrap_or("<unknown>"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_includes_the_key_fields() {
        let report = render_crash_report(
            1_700_000_000_000,
            "something went wrong",
            Some("src/lib.rs:42:9"),
            "  0: some::frame",
        );
        assert!(report.contains("time (unix ms): 1700000000000"));
        assert!(report.contains(env!("CARGO_PKG_VERSION")));
        assert!(report.contains(std::env::consts::OS));
        assert!(report.contains("location:       src/lib.rs:42:9"));
        assert!(report.contains("panic:          something went wrong"));
        assert!(report.contains("backtrace:\n  0: some::frame"));
    }

    #[test]
    fn probe_writable_returns_true_for_a_writable_dir() {
        let dir = std::env::temp_dir().join(format!(
            "bookrack-obs-writable-{}-{}",
            std::process::id(),
            line!()
        ));
        std::fs::create_dir_all(&dir).expect("create dir");
        assert!(probe_writable(&dir));
        assert!(!dir.join(".bookrack-writable-probe").exists());
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[cfg(unix)]
    #[test]
    fn probe_writable_returns_false_for_a_read_only_dir() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!(
            "bookrack-obs-readonly-{}-{}",
            std::process::id(),
            line!()
        ));
        std::fs::create_dir_all(&dir).expect("create dir");
        let mut perms = std::fs::metadata(&dir).expect("read perms").permissions();
        perms.set_mode(0o555);
        std::fs::set_permissions(&dir, perms).expect("chmod 555");
        let result = probe_writable(&dir);
        let mut restore = std::fs::metadata(&dir).expect("read perms").permissions();
        restore.set_mode(0o755);
        std::fs::set_permissions(&dir, restore).expect("chmod 755");
        std::fs::remove_dir_all(&dir).expect("cleanup");
        assert!(!result);
    }

    #[test]
    fn broken_pipe_classifier_matches_std_panic_text() {
        // Panic messages produced by the standard library on a closed
        // stdout / stderr come in two shapes depending on the formatter:
        // the io::Error `Display` lowercases the kind, while some
        // platform-emitted variants keep the OS message's casing. The
        // classifier accepts both so the silent-exit path covers each.
        assert!(is_broken_pipe_panic(
            "failed printing to stdout: broken pipe (os error 32)"
        ));
        assert!(is_broken_pipe_panic(
            "failed printing to stderr: Broken pipe (os error 32)"
        ));
        assert!(!is_broken_pipe_panic("unrelated assertion failure"));
        assert!(!is_broken_pipe_panic(""));
    }

    #[test]
    fn write_creates_a_crash_file_in_a_missing_dir() {
        let dir = std::env::temp_dir().join(format!("bookrack-obs-crash-{}", std::process::id()));
        // The logs directory does not exist yet; write must create it.
        let _ = std::fs::remove_dir_all(&dir);
        let path =
            write_crash_report(&dir, "boom", Some("a.rs:1:1"), "<bt>").expect("write report");
        assert!(path.exists());
        assert!(
            path.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("crash-") && n.ends_with(".txt"))
        );
        let body = std::fs::read_to_string(&path).expect("read back");
        assert!(body.contains("panic:          boom"));
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }
}
