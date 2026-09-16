use std::path::Path;

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{EnvFilter, Layer, layer::SubscriberExt, util::SubscriberInitExt};

fn stderr_level_for(verbosity: u8) -> tracing::Level {
    match verbosity {
        0 => tracing::Level::WARN,
        1 => tracing::Level::INFO,
        _ => tracing::Level::DEBUG,
    }
}

/// Installs the global tracing subscriber: a verbosity/`RUST_LOG`-controlled
/// stderr layer plus a file layer pinned to DEBUG for the run log,
/// regardless of `verbosity` or `RUST_LOG`. The caller must hold the
/// returned `WorkerGuard` for the process lifetime, or buffered log lines
/// can be dropped when the guard (and its flush-on-drop) runs early.
pub fn init(verbosity: u8, run_log_path: &Path) -> anyhow::Result<WorkerGuard> {
    let stderr_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(stderr_level_for(verbosity).to_string()));
    let stderr_layer = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_filter(stderr_filter);

    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(run_log_path)?;
    let (non_blocking, guard) = tracing_appender::non_blocking(file);
    let file_layer = tracing_subscriber::fmt::layer()
        .with_writer(non_blocking)
        .with_ansi(false)
        .with_filter(EnvFilter::new("debug"));

    tracing_subscriber::registry()
        .with(stderr_layer)
        .with(file_layer)
        .try_init()
        .map_err(|error| anyhow::anyhow!("failed to install tracing subscriber: {error}"))?;

    Ok(guard)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verbosity_zero_maps_to_warn_level() {
        assert_eq!(stderr_level_for(0), tracing::Level::WARN);
    }

    #[test]
    fn verbosity_one_maps_to_info_level() {
        assert_eq!(stderr_level_for(1), tracing::Level::INFO);
    }

    #[test]
    fn verbosity_two_or_more_maps_to_debug_level() {
        assert_eq!(stderr_level_for(2), tracing::Level::DEBUG);
        assert_eq!(stderr_level_for(5), tracing::Level::DEBUG);
    }
}
