use std::collections::VecDeque;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use mcu_update::build::{BuildCommand, CommandError, CommandOutput, CommandRunner};
use mcu_update::coordinator::BuildCoordinator;
use mcu_update::moonraker::parse_inventory;
use mcu_update::plan::build_update_plan;
use mcu_update::workspace::RunWorkspace;

#[test]
fn executes_an_approved_build_after_stopping_klipper_without_restarting_it() {
    let root = temporary_directory();
    let source_dir = root.join("klipper");
    fs::create_dir_all(source_dir.join("out")).expect("source output directory");
    fs::write(source_dir.join("out/klipper.bin"), b"firmware").expect("source artifact");
    let workspace = RunWorkspace::create(root.join("run")).expect("workspace should create");
    let inventory =
        parse_inventory(include_str!("fixtures/mcu-inventory.json")).expect("fixture should parse");
    let plan = build_update_plan(&inventory);
    let make_runner = FakeRunner::new([success_output(), success_output()]);
    let service_runner = FakeRunner::new([
        state_output(true, b"active\n"),
        success_output(),
        state_output(false, b"inactive\n"),
    ]);
    let coordinator =
        BuildCoordinator::new(&source_dir, make_runner.clone(), service_runner.clone());

    let pending = coordinator
        .prepare(&inventory, &plan, &workspace, "mcu toolhead")
        .expect("build should prepare");
    assert!(
        pending
            .artifact_path()
            .parent()
            .expect("artifact parent")
            .exists()
    );
    assert!(make_runner.commands.lock().expect("runner lock").is_empty());
    assert!(
        service_runner
            .commands
            .lock()
            .expect("runner lock")
            .is_empty()
    );

    let update = coordinator
        .execute_and_flash(pending.approve(), |prepared, firmware| {
            assert_eq!(prepared.target_name, "mcu toolhead");
            assert_eq!(firmware, b"firmware");
            Ok::<_, ()>(mcu_update::flash::FlashResult {
                reported_pages: Some(1),
                padded_bytes: 64,
            })
        })
        .expect("approved build should succeed");

    assert_eq!(
        fs::read(&update.artifact.path).expect("copied artifact"),
        b"firmware"
    );
    assert_eq!(update.flash.reported_pages, Some(1));
    let service_commands = service_runner.commands.lock().expect("runner lock");
    assert_eq!(
        service_commands
            .iter()
            .map(|command| command.arguments.as_slice())
            .collect::<Vec<_>>(),
        vec![
            ["is-active".to_owned(), "klipper".to_owned()].as_slice(),
            ["stop".to_owned(), "klipper".to_owned()].as_slice(),
            ["is-active".to_owned(), "klipper".to_owned()].as_slice(),
        ]
    );
    let make_commands = make_runner.commands.lock().expect("runner lock");
    assert_eq!(make_commands.len(), 2);
    assert_eq!(make_commands[0].arguments[0], "olddefconfig");

    fs::remove_dir_all(root).expect("test directory cleanup");
}

#[test]
fn starts_klipper_once_after_a_completed_batch() {
    let root = temporary_directory();
    let runner = FakeRunner::new([success_output(), state_output(true, b"active\n")]);
    let coordinator = BuildCoordinator::new(&root, FakeRunner::new([]), runner.clone());

    coordinator
        .start_after_batch()
        .expect("Klipper should restart");

    let commands = runner.commands.lock().expect("runner lock");
    assert_eq!(
        commands
            .iter()
            .map(|command| command.arguments.as_slice())
            .collect::<Vec<_>>(),
        vec![
            ["start".to_owned(), "klipper".to_owned()].as_slice(),
            ["is-active".to_owned(), "klipper".to_owned()].as_slice(),
        ]
    );
}

#[test]
fn does_not_restart_klipper_when_a_later_batch_flash_fails() {
    let root = temporary_directory();
    let source_dir = root.join("klipper");
    fs::create_dir_all(source_dir.join("out")).expect("source output directory");
    fs::write(source_dir.join("out/klipper.bin"), b"firmware").expect("source artifact");
    let workspace = RunWorkspace::create(root.join("run")).expect("workspace");
    let inventory =
        parse_inventory(include_str!("fixtures/mcu-inventory.json")).expect("inventory");
    let plan = build_update_plan(&inventory);
    let build_runner = FakeRunner::new([success_output(), success_output()]);
    let service_runner = FakeRunner::new([
        state_output(true, b"active\n"),
        success_output(),
        state_output(false, b"inactive\n"),
    ]);
    let coordinator = BuildCoordinator::new(&source_dir, build_runner, service_runner.clone());
    let pending = coordinator
        .prepare(&inventory, &plan, &workspace, "mcu toolhead")
        .expect("prepare");

    assert!(
        coordinator
            .execute_and_flash(pending.approve(), |_, _| Err::<
                mcu_update::flash::FlashResult,
                _,
            >("flash failed"))
            .is_err()
    );
    assert!(
        service_runner
            .commands
            .lock()
            .expect("runner lock")
            .iter()
            .all(|command| command
                .arguments
                .first()
                .is_none_or(|action| action != "start"))
    );
}

#[test]
fn rejects_a_restart_that_does_not_make_klipper_active() {
    let root = temporary_directory();
    let runner = FakeRunner::new([success_output(), state_output(false, b"inactive\n")]);
    let coordinator = BuildCoordinator::new(&root, FakeRunner::new([]), runner);

    assert!(coordinator.start_after_batch().is_err());
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

impl CommandRunner for FakeRunner {
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

fn success_output() -> CommandOutput {
    CommandOutput::success()
}

fn state_output(success: bool, stdout: &[u8]) -> CommandOutput {
    CommandOutput {
        success,
        stdout: stdout.to_vec(),
        stderr: Vec::new(),
    }
}

fn temporary_directory() -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should be after epoch")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "mcu-update-coordinator-{}-{nonce}",
        std::process::id()
    ))
}
