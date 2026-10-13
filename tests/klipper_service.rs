use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use aldis::build::{BuildCommand, CommandError, CommandOutput, CommandPort};
use aldis::service::{KlipperService, KlipperUnit, ServiceState};

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

#[test]
fn accepts_only_klipper_shaped_unit_names() {
    for name in [
        "klipper",
        "klipper-2",
        "klipper_2",
        "klipper.1",
        "klipper@voron",
    ] {
        assert_eq!(KlipperUnit::parse(name).unwrap().as_str(), name, "{name}");
    }
    let too_long = format!("klipper{}", "x".repeat(60));
    for name in [
        "",
        "Klipper",
        "moonraker",
        "klipper evil.service",
        "klipper;reboot",
        "klipper/../x",
        "klipper\n",
        "-klipper",
        too_long.as_str(),
    ] {
        assert!(KlipperUnit::parse(name).is_err(), "{name:?}");
    }
}

#[test]
fn controls_the_configured_unit() {
    let runner = FakeRunner::new([
        command_output(true, b"inactive\n"),
        command_output(true, b""),
        command_output(true, b""),
    ]);
    let service =
        KlipperService::with_unit(runner.clone(), KlipperUnit::parse("klipper-2").unwrap());

    service.state().unwrap();
    service.start().unwrap();
    service.stop().unwrap();

    let commands = runner.commands.lock().expect("runner lock");
    for (command, verb) in commands.iter().zip(["is-active", "start", "stop"]) {
        assert_eq!(
            command.arguments,
            ["-n", "/bin/systemctl", verb, "klipper-2"]
        );
    }
    assert_eq!(service.unit().as_str(), "klipper-2");
}
