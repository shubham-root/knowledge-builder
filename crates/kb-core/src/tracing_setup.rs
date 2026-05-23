//! Tracing / logging initialisation for Knowledge Builder.
//!
//! Configures two layers:
//! - **JSON file layer** via `tracing-appender` with daily rotation, written
//!   to `log_dir/kb.log.<date>`.
//! - **Human-readable stderr layer** enabled when `foreground = true`
//!   (i.e., `kb daemon --foreground`).
//!
//! Full implementation lives in T7; this scaffold ensures the public API
//! compiles for dependent crates.

use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

/// Initialise global tracing subscriber.
///
/// - `log_dir`:   directory for rotating JSON log files.
/// - `log_level`: minimum level string (`"info"`, `"debug"`, …).
/// - `log_format`: `"json"` or `"pretty"`.
/// - `foreground`: if `true`, also emit human-readable output on stderr.
pub fn init(
    log_dir:    impl AsRef<std::path::Path>,
    log_level:  &str,
    _log_format: &str,
    foreground: bool,
) -> crate::Result<tracing_appender::non_blocking::WorkerGuard> {
    let log_dir = log_dir.as_ref();
    std::fs::create_dir_all(log_dir)?;

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(log_level));

    // Rolling file appender (JSON).
    let file_appender = tracing_appender::rolling::daily(log_dir, "kb.log");
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

    let file_layer = fmt::layer()
        .json()
        .with_writer(non_blocking);

    if foreground {
        let stderr_layer = fmt::layer()
            .pretty()
            .with_writer(std::io::stderr);

        tracing_subscriber::registry()
            .with(filter)
            .with(file_layer)
            .with(stderr_layer)
            .init();
    } else {
        tracing_subscriber::registry()
            .with(filter)
            .with(file_layer)
            .init();
    }

    Ok(guard)
}
