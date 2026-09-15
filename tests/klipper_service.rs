use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use aldis::build::{BuildCommand, CommandError, CommandOutput, CommandPort};
use aldis::service::{KlipperService, ServiceState};

#[test]
fn checks_then_stops_klipper_with_explicit_systemctl_commands() {
    let runner = FakeRunner::new([command_output(true, b"active\n"), command_output(true, b"")]);
    let service = KlipperService::new(runner.clone());

    assert_eq!(
        service.state().expect("state should resolve"),
        ServiceState::Active
    );
    service.stop().expect("stop should succeed");

    let commands = runner.commands.lock().expect("runner lock");
    assert_eq!(commands.len(), 2);
    assert_eq!(commands[0].program, "sudo");
    assert_eq!(commands[0].current_dir, None);
    assert_eq!(
        commands[0].arguments,
        ["-n", "/bin/systemctl", "is-active", "klipper"]
    );
    assert_eq!(
        commands[1].arguments,
        ["-n", "/bin/systemctl", "stop", "klipper"]
    );
}

#[test]
fn recognizes_inactive_even_when_systemctl_returns_nonzero() {
    let runner = FakeRunner::new([command_output(false, b"inactive\n")]);
    let service = KlipperService::new(runner);

    assert_eq!(
        service.state().expect("state should resolve"),
        ServiceState::Inactive
    );
}

#[derive(Clone)]
struct FakeRunner {
    commands: Arc<Mutex<Vec<BuildCommand>>>,
    outputs: Arc<Mutex<VecDeque<CommandOutput>>>,
}

impl FakeRunner {
    fn new(outputs: impl IntoIterator<Item = CommandOutput>) -> Self {
        Self {
            commands: Arc::new(Mutex::new(Vec::new())),
            outputs: Arc::new(Mutex::new(outputs.into_iter().collect())),
        }
    }
}

impl CommandPort for FakeRunner {
    fn run(&self, command: &BuildCommand) -> Result<CommandOutput, CommandError> {
        self.commands
            .lock()
            .expect("runner lock")
            .push(command.clone());
        self.outputs
            .lock()
            .expect("runner lock")
            .pop_front()
            .ok_or_else(|| CommandError::Spawn(std::io::Error::other("missing fake output")))
    }
}

fn command_output(success: bool, stdout: &[u8]) -> CommandOutput {
    CommandOutput {
        success,
        stdout: stdout.to_vec(),
        stderr: Vec::new(),
    }
}
