//! A `CommandPort` decorator that traces every command and its complete captured output.

use crate::build::{BuildCommand, CommandError, CommandOutput, CommandPort};

/// A command runner that traces every command and its complete captured output as a single
/// `tracing` event per finished command (not streamed line-by-line).
#[derive(Clone)]
pub struct LoggingCommandAdapter<R> {
    inner: R,
}

impl<R> LoggingCommandAdapter<R> {
    /// Wraps `inner` so its command invocations and captured output are traced.
    pub fn new(inner: R) -> Self {
        Self { inner }
    }
}

impl<R: CommandPort> CommandPort for LoggingCommandAdapter<R> {
    #[tracing::instrument(
        skip(self, command),
        fields(program = %shell_token(&command.program), args = %display_arguments(command))
    )]
    fn run(&self, command: &BuildCommand) -> Result<CommandOutput, CommandError> {
        match self.inner.run(command) {
            Ok(output) => {
                let exit = if output.success { "success" } else { "failure" };
                tracing::info!(
                    exit,
                    stdout = %String::from_utf8_lossy(&output.stdout),
                    stderr = %String::from_utf8_lossy(&output.stderr),
                    "command finished"
                );
                Ok(output)
            }
            Err(error) => {
                tracing::debug!(error = %crate::error_chain(&error), "command failed to start");
                Err(error)
            }
        }
    }
}

fn display_arguments(command: &BuildCommand) -> String {
    let arguments = command
        .arguments
        .iter()
        .map(|argument| shell_token(argument))
        .collect::<Vec<_>>()
        .join(" ");
    command
        .current_dir
        .as_ref()
        .map_or(arguments.clone(), |directory| {
            format!(
                "{arguments} (cd {})",
                shell_token(&directory.to_string_lossy())
            )
        })
}

fn shell_token(value: &str) -> String {
    if !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b'_' | b'-' | b'.' | b'/' | b'=' | b':' | b',')
        })
    {
        value.to_owned()
    } else {
        format!("{:?}", value)
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    use super::*;

    #[derive(Clone, Default)]
    struct SharedBuffer(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for SharedBuffer {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().write(buffer)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'writer> tracing_subscriber::fmt::MakeWriter<'writer> for SharedBuffer {
        type Writer = SharedBuffer;

        fn make_writer(&'writer self) -> Self::Writer {
            self.clone()
        }
    }

    struct FailingRunner;

    impl CommandPort for FailingRunner {
        fn run(&self, _command: &BuildCommand) -> Result<CommandOutput, CommandError> {
            Ok(CommandOutput {
                success: false,
                stdout: b"configuration output\n".to_vec(),
                stderr: b"invalid Kconfig symbol\n".to_vec(),
            })
        }
    }

    struct SuccessfulRunner;

    impl CommandPort for SuccessfulRunner {
        fn run(&self, _command: &BuildCommand) -> Result<CommandOutput, CommandError> {
            Ok(CommandOutput {
                success: true,
                stdout: b"compiled 1 object\n".to_vec(),
                stderr: b"warning: retained for diagnosis\n".to_vec(),
            })
        }
    }

    fn captured_output(subscriber_output: SharedBuffer) -> String {
        String::from_utf8(subscriber_output.0.lock().unwrap().clone())
            .expect("tracing output is valid utf8")
    }

    #[test]
    fn emits_one_event_with_the_complete_captured_output_for_a_failed_command() {
        let buffer = SharedBuffer::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(buffer.clone())
            .with_ansi(false)
            .finish();
        let adapter = LoggingCommandAdapter::new(FailingRunner);
        let command = BuildCommand {
            program: "make".to_owned(),
            arguments: vec!["olddefconfig".to_owned()],
            current_dir: Some(PathBuf::from("/home/pi/klipper")),
        };

        let output = tracing::subscriber::with_default(subscriber, || {
            adapter.run(&command).expect("adapter returns output")
        });

        assert!(!output.success);
        let contents = captured_output(buffer);
        assert_eq!(contents.matches("command finished").count(), 1);
        assert!(contents.contains("configuration output"));
        assert!(contents.contains("invalid Kconfig symbol"));
        assert!(contents.contains("failure"));
        assert!(contents.contains("make"));
        assert!(contents.contains("olddefconfig (cd /home/pi/klipper)"));
    }

    #[test]
    fn emits_one_event_with_the_complete_captured_output_for_a_successful_command() {
        let buffer = SharedBuffer::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(buffer.clone())
            .with_ansi(false)
            .finish();
        let adapter = LoggingCommandAdapter::new(SuccessfulRunner);
        let command = BuildCommand {
            program: "make".to_owned(),
            arguments: vec!["KCONFIG_CONFIG=/tmp/mcu.config".to_owned()],
            current_dir: None,
        };

        let output = tracing::subscriber::with_default(subscriber, || {
            adapter.run(&command).expect("adapter returns output")
        });

        assert!(output.success);
        let contents = captured_output(buffer);
        assert_eq!(contents.matches("command finished").count(), 1);
        assert!(contents.contains("compiled 1 object"));
        assert!(contents.contains("warning: retained for diagnosis"));
        assert!(contents.contains("success"));
        assert!(contents.contains("make"));
        assert!(contents.contains("KCONFIG_CONFIG=/tmp/mcu.config"));
    }
}
