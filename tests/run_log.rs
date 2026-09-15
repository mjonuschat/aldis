use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use mcu_update::build::{BuildCommand, CommandOutput, CommandPort};
use mcu_update::run_log::{LoggingCommandAdapter, RunLog};

#[test]
fn retains_commands_actions_and_complete_output_for_failed_runs() {
    let root = temporary_directory();
    fs::create_dir_all(&root).expect("create run directory");
    let log = RunLog::create(&root).expect("create run log");
    log.action("starting firmware build");
    let runner = LoggingCommandAdapter::new(FailingRunner, log.clone());
    let command = BuildCommand {
        program: "make".to_owned(),
        arguments: vec!["olddefconfig".to_owned()],
        current_dir: Some(PathBuf::from("/home/pi/klipper")),
    };

    let output = runner.run(&command).expect("runner should return output");

    assert!(!output.success);
    let contents = fs::read_to_string(log.path()).expect("read run log");
    assert!(contents.contains("action: starting firmware build"));
    assert!(contents.contains("$ (cd /home/pi/klipper && make olddefconfig)"));
    assert!(contents.contains("stdout:\nconfiguration output\n"));
    assert!(contents.contains("stderr:\ninvalid Kconfig symbol\n"));
    assert!(contents.contains("exit: failure"));

    fs::remove_dir_all(root).expect("remove test directory");
}

#[test]
fn retains_complete_output_for_successful_commands() {
    let root = temporary_directory();
    fs::create_dir_all(&root).expect("create run directory");
    let log = RunLog::create(&root).expect("create run log");
    let runner = LoggingCommandAdapter::new(SuccessfulRunner, log.clone());
    let command = BuildCommand {
        program: "make".to_owned(),
        arguments: vec!["KCONFIG_CONFIG=/tmp/mcu.config".to_owned()],
        current_dir: None,
    };

    assert!(
        runner
            .run(&command)
            .expect("runner should return output")
            .success
    );

    let contents = fs::read_to_string(log.path()).expect("read run log");
    assert!(contents.contains("stdout:\ncompiled 1 object\n"));
    assert!(contents.contains("stderr:\nwarning: retained for diagnosis\n"));
    assert!(contents.contains("exit: success"));

    fs::remove_dir_all(root).expect("remove test directory");
}

struct FailingRunner;

impl CommandPort for FailingRunner {
    fn run(
        &self,
        _command: &BuildCommand,
    ) -> Result<CommandOutput, mcu_update::build::CommandError> {
        Ok(CommandOutput {
            success: false,
            stdout: b"configuration output\n".to_vec(),
            stderr: b"invalid Kconfig symbol\n".to_vec(),
        })
    }
}

struct SuccessfulRunner;

impl CommandPort for SuccessfulRunner {
    fn run(
        &self,
        _command: &BuildCommand,
    ) -> Result<CommandOutput, mcu_update::build::CommandError> {
        Ok(CommandOutput {
            success: true,
            stdout: b"compiled 1 object\n".to_vec(),
            stderr: b"warning: retained for diagnosis\n".to_vec(),
        })
    }
}

fn temporary_directory() -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should be after epoch")
        .as_nanos();
    std::env::temp_dir().join(format!("mcu-update-run-log-{}-{nonce}", std::process::id()))
}
