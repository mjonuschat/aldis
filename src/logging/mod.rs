use std::path::Path;

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{EnvFilter, Layer, layer::SubscriberExt, util::SubscriberInitExt};

pub mod command;

pub use command::LoggingCommandAdapter;

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
    // tracing-subscriber caches a span's formatted fields on the span itself,
    // keyed by field-formatter type; whichever fmt layer formats the span
    // first (here, stderr) would otherwise poison the file layer's cached
    // copy with ANSI codes even though the file layer sets with_ansi(false).
    let stderr_layer = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_filter(stderr_filter);

    let file = std::fs::OpenOptions::new()
        .create_new(true)
        .append(true)
        .open(run_log_path)?;
    let (non_blocking, guard) = tracing_appender::non_blocking(file);
    let file_layer = tracing_subscriber::fmt::layer()
        .with_writer(non_blocking)
        .with_ansi(false)
        .with_filter(EnvFilter::new(
            "debug,dfu_core=info,ureq=warn,ureq::run=debug",
        ));

    tracing_subscriber::registry()
        .with(stderr_layer)
        .with(file_layer)
        .try_init()
        .map_err(|error| anyhow::anyhow!("failed to install tracing subscriber: {error}"))?;

    Ok(guard)
}

/// Installs a stderr-only subscriber, for callers that cannot open a file
/// layer's log path (e.g. permission errors on a shared temp directory) but
/// still need `RUST_LOG`/verbosity-controlled diagnostics on stderr.
pub fn init_stderr_only(verbosity: u8) -> anyhow::Result<()> {
    let stderr_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(stderr_level_for(verbosity).to_string()));
    let stderr_layer = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_filter(stderr_filter);

    tracing_subscriber::registry()
        .with(stderr_layer)
        .try_init()
        .map_err(|error| anyhow::anyhow!("failed to install tracing subscriber: {error}"))
}

/// A tracing writer that forwards to the current run's log file, if one is open.
#[derive(Clone, Default)]
pub struct RunLogSink(std::sync::Arc<std::sync::Mutex<Option<std::fs::File>>>);

impl RunLogSink {
    /// Opens `path` (creating it if needed) and starts forwarding writes to it.
    pub fn start(&self, path: &Path) -> std::io::Result<()> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        *self.0.lock().unwrap() = Some(file);
        Ok(())
    }

    /// Stops forwarding writes; further writes are discarded until [`Self::start`] is called again.
    pub fn stop(&self) {
        *self.0.lock().unwrap() = None;
    }
}

pub struct RunLogWriter(std::sync::Arc<std::sync::Mutex<Option<std::fs::File>>>);

impl std::io::Write for RunLogWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        match self.0.lock().unwrap().as_mut() {
            Some(file) => std::io::Write::write(file, bytes),
            None => Ok(bytes.len()),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self.0.lock().unwrap().as_mut() {
            Some(file) => std::io::Write::flush(file),
            None => Ok(()),
        }
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for RunLogSink {
    type Writer = RunLogWriter;

    fn make_writer(&'a self) -> Self::Writer {
        RunLogWriter(std::sync::Arc::clone(&self.0))
    }
}

/// Installs the agent's subscriber: operational logs to stdout (journald), and full debug detail
/// to whichever run log is currently started on the returned sink.
pub fn init_agent(verbosity: u8) -> anyhow::Result<RunLogSink> {
    let stdout_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(stderr_level_for(verbosity.max(1)).to_string()));
    let stdout_layer = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stdout)
        .with_ansi(false)
        .with_filter(stdout_filter);
    let sink = RunLogSink::default();
    let run_layer = tracing_subscriber::fmt::layer()
        .with_writer(sink.clone())
        .with_ansi(false)
        .with_filter(EnvFilter::new(
            "debug,dfu_core=info,ureq=warn,ureq::run=debug",
        ));
    tracing_subscriber::registry()
        .with(stdout_layer)
        .with(run_layer)
        .try_init()
        .map_err(|error| anyhow::anyhow!("failed to install tracing subscriber: {error}"))?;
    Ok(sink)
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

    #[test]
    fn writes_only_while_a_run_log_is_started() {
        use std::io::Write;
        use tracing_subscriber::fmt::MakeWriter;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run.log");
        let sink = RunLogSink::default();

        sink.make_writer().write_all(b"before\n").unwrap();
        sink.start(&path).unwrap();
        sink.make_writer().write_all(b"during\n").unwrap();
        sink.stop();
        sink.make_writer().write_all(b"after\n").unwrap();

        assert_eq!(std::fs::read_to_string(path).unwrap(), "during\n");
    }
}
