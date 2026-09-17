//! Interactive terminal progress rendering for the update command.

use std::io::{self, IsTerminal, Write};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::time::Duration;

use aldis::coordinator::UpdateProgress;

use crate::cli::ColorMode;

pub(crate) struct UpdateUi {
    color: bool,
    interactive: bool,
    active: Option<ActiveProgress>,
}

struct ActiveProgress {
    label: String,
    spinner: Option<Spinner>,
}

struct Spinner {
    stopped: Arc<AtomicBool>,
    worker: thread::JoinHandle<()>,
}

impl UpdateUi {
    pub(crate) fn new(color_mode: ColorMode, no_progress: bool) -> Self {
        let interactive = io::stderr().is_terminal();
        Self {
            color: colors_enabled(color_mode, interactive),
            interactive: interactive && !no_progress,
            active: None,
        }
    }

    pub(crate) fn action(&self, action: &str) {
        tracing::debug!("{action}");
    }

    pub(crate) fn block(&mut self, text: &str) {
        self.clear_active();
        self.action(text.trim());
        eprint!("{text}");
        let _ = io::stderr().flush();
    }

    pub(crate) fn heading(&mut self, text: &str) {
        self.clear_active();
        self.action(text);
        eprintln!("\n{}", self.style("1", text));
    }

    pub(crate) fn prompt(&mut self, text: &str) {
        self.clear_active();
        self.action(&format!("prompt: {text}"));
        eprint!("\n{} [y/N] ", self.style("1", text));
        let _ = io::stderr().flush();
    }

    pub(crate) fn begin(&mut self, label: impl Into<String>) {
        self.clear_active();
        let label = label.into();
        self.action(&format!("starting: {label}"));
        let spinner = self
            .interactive
            .then(|| Spinner::start(label.clone(), self.color));
        if spinner.is_none() {
            eprintln!("  ..{label}");
        }
        self.active = Some(ActiveProgress { label, spinner });
    }

    pub(crate) fn finish_success(&mut self, message: impl AsRef<str>) {
        if let Some(active) = self.active.take() {
            if let Some(spinner) = active.spinner {
                spinner.stop();
            }
            self.clear_spinner();
            self.action(&format!("completed: {}", message.as_ref()));
            eprintln!("{}", self.success_line(message.as_ref()));
        }
    }

    pub(crate) fn finish_failure(&mut self) {
        let Some(active) = self.active.take() else {
            return;
        };
        if let Some(spinner) = active.spinner {
            spinner.stop();
            self.clear_spinner();
        }
        self.action(&format!("failed: {}", active.label));
        eprintln!("{}", self.failure_line(&format!("{} failed", active.label)));
    }

    fn clear_active(&mut self) {
        if let Some(active) = self.active.take()
            && let Some(spinner) = active.spinner
        {
            spinner.stop();
            self.clear_spinner();
        }
    }

    fn clear_spinner(&self) {
        if self.interactive {
            eprint!("\r\x1b[2K");
            let _ = io::stderr().flush();
        }
    }

    fn success_line(&self, message: &str) -> String {
        if self.interactive {
            format!("  {} {message}", self.style("32", "✓"))
        } else {
            let marker = if self.color {
                self.style("32", "[ok]")
            } else {
                "[ok]".to_owned()
            };
            if self.color {
                format!("  {marker} {message}")
            } else {
                plain_success_line(message)
            }
        }
    }

    fn failure_line(&self, message: &str) -> String {
        let marker = if self.interactive { "error" } else { "[error]" };
        format!("  {} {message}", self.style("31", marker))
    }

    fn style(&self, code: &str, text: &str) -> String {
        if self.color {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_owned()
        }
    }

    pub(crate) fn progress(&mut self, progress: UpdateProgress) {
        match progress {
            UpdateProgress::StoppingKlipper => self.begin("stopping Klipper"),
            UpdateProgress::ConfiguringFirmware => {
                self.finish_success("stopped Klipper");
                self.begin("configuring firmware");
            }
            UpdateProgress::CompilingFirmware => {
                self.finish_success("configured firmware");
                self.begin("compiling firmware");
            }
            UpdateProgress::EnteringBootloader => {
                self.finish_success("compiled firmware");
                self.begin("entering bootloader");
            }
            UpdateProgress::BootloaderReady => self.finish_success("bootloader ready"),
            UpdateProgress::StartingFlash => self.begin("flashing firmware"),
        }
    }
}

impl Drop for UpdateUi {
    fn drop(&mut self) {
        self.clear_active();
    }
}

impl Spinner {
    fn start(label: String, color: bool) -> Self {
        let stopped = Arc::new(AtomicBool::new(false));
        let worker_stopped = Arc::clone(&stopped);
        let worker = thread::spawn(move || {
            const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
            let mut frame = 0;
            while !worker_stopped.load(Ordering::Relaxed) {
                let glyph = if color {
                    format!("\x1b[36m{}\x1b[0m", FRAMES[frame])
                } else {
                    FRAMES[frame].to_owned()
                };
                eprint!("\r\x1b[2K  {glyph} {label}");
                let _ = io::stderr().flush();
                frame = (frame + 1) % FRAMES.len();
                thread::sleep(Duration::from_millis(80));
            }
        });
        Self { stopped, worker }
    }

    fn stop(self) {
        self.stopped.store(true, Ordering::Relaxed);
        let _ = self.worker.join();
    }
}

pub(crate) fn colors_enabled(mode: ColorMode, is_terminal: bool) -> bool {
    match mode {
        ColorMode::Auto => is_terminal && std::env::var_os("NO_COLOR").is_none(),
        ColorMode::Always => true,
        ColorMode::Never => false,
    }
}

pub(crate) fn plain_success_line(message: &str) -> String {
    format!("  [ok] {message}")
}

/// Reads one line from stdin, trimmed and lowercased.
pub(crate) fn read_confirmation() -> Result<String, io::Error> {
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    Ok(input.trim().to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::{colors_enabled, plain_success_line};
    use crate::cli::ColorMode;

    #[test]
    fn honors_color_mode_without_using_terminal_escape_codes_in_plain_output() {
        assert!(colors_enabled(ColorMode::Always, false));
        assert!(!colors_enabled(ColorMode::Never, true));
        assert!(colors_enabled(ColorMode::Auto, true));
        assert!(!colors_enabled(ColorMode::Auto, false));
        assert_eq!(
            plain_success_line("compiled firmware"),
            "  [ok] compiled firmware"
        );
    }
}
