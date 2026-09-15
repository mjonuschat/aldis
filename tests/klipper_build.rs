use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use mcu_update::build::{
    BuildCommand, BuildError, BuildRequest, CommandOutput, CommandPort, KlipperBuilder,
};

#[test]
fn builds_with_an_explicit_kconfig_and_copies_the_artifact() {
    let root = temporary_directory("success");
    let source_dir = root.join("klipper");
    let config_path = root.join("run/mcu-toolhead/.config");
    let artifact_path = root.join("artifacts/toolhead.bin");
    fs::create_dir_all(source_dir.join("out")).expect("source output directory should exist");
    fs::write(source_dir.join("out/klipper.bin"), b"firmware").expect("fixture artifact");
    let runner = FakeRunner::success();
    let builder = KlipperBuilder::new(&source_dir, runner.clone());

    let artifact = builder
        .build(&BuildRequest {
            kconfig: "CONFIG_MACH_STM32G0B1=y\n".to_owned(),
            config_path: config_path.clone(),
            artifact_path: artifact_path.clone(),
        })
        .expect("build should succeed");

    assert_eq!(
        fs::read_to_string(&config_path).expect("written Kconfig"),
        "CONFIG_MACH_STM32G0B1=y\n"
    );
    assert_eq!(
        fs::read(&artifact_path).expect("copied artifact"),
        b"firmware"
    );
    assert_eq!(artifact.path, artifact_path);
    assert_eq!(artifact.byte_count, 8);
    assert_eq!(artifact.kconfig, "CONFIG_MACH_STM32G0B1=y\n");

    let commands = runner.commands.lock().expect("runner lock");
    assert_eq!(commands.len(), 2);
    assert_eq!(commands[0].program, "make");
    assert_eq!(commands[0].current_dir, Some(source_dir));
    assert_eq!(
        commands[0].arguments,
        vec![
            "olddefconfig".to_owned(),
            format!("KCONFIG_CONFIG={}", config_path.display()),
        ]
    );
    assert_eq!(
        commands[1].arguments,
        vec![format!("KCONFIG_CONFIG={}", config_path.display())]
    );

    fs::remove_dir_all(root).expect("test directory cleanup");
}

#[test]
fn copies_uf2_artifact_for_a_no_bootloader_rp2040_build() {
    let root = temporary_directory("rp2040");
    let source_dir = root.join("klipper");
    let config_path = root.join("run/mcu/.config");
    let artifact_path = root.join("artifacts/mcu.bin");
    fs::create_dir_all(source_dir.join("out")).expect("source output directory should exist");
    fs::write(source_dir.join("out/klipper.uf2"), b"uf2 firmware").expect("fixture artifact");
    let builder = KlipperBuilder::new(&source_dir, FakeRunner::success());

    let artifact = builder
        .build(&BuildRequest {
            kconfig: "CONFIG_MACH_RPXXXX=y\nCONFIG_RPXXXX_FLASH_START_0100=y\n".to_owned(),
            config_path,
            artifact_path: artifact_path.clone(),
        })
        .expect("build should succeed");

    assert_eq!(
        fs::read(&artifact_path).expect("copied artifact"),
        b"uf2 firmware"
    );
    assert_eq!(artifact.byte_count, 12);
    fs::remove_dir_all(root).expect("test directory cleanup");
}

#[test]
fn returns_the_expanded_kconfig_after_olddefconfig() {
    let root = temporary_directory("expanded");
    let source_dir = root.join("klipper");
    let config_path = root.join("run/mcu/.config");
    let artifact_path = root.join("artifacts/mcu.bin");
    fs::create_dir_all(source_dir.join("out")).expect("source output directory should exist");
    fs::write(source_dir.join("out/klipper.bin"), b"firmware").expect("fixture artifact");
    let builder = KlipperBuilder::new(
        &source_dir,
        ExpandingRunner {
            expanded_kconfig: "CONFIG_MACH_ATSAMD=y\nCONFIG_SAMD_FLASH_START_2000=y\n".to_owned(),
        },
    );

    let artifact = builder
        .build(&BuildRequest {
            kconfig: "CONFIG_MACH_ATSAMD=y\n".to_owned(),
            config_path,
            artifact_path,
        })
        .expect("build should succeed");

    assert_eq!(
        artifact.kconfig,
        "CONFIG_MACH_ATSAMD=y\nCONFIG_SAMD_FLASH_START_2000=y\n"
    );
    fs::remove_dir_all(root).expect("test directory cleanup");
}

#[test]
fn preserves_make_stderr_when_kconfig_expansion_fails() {
    let root = temporary_directory("failure");
    let source_dir = root.join("klipper");
    let config_path = root.join("run/mcu/.config");
    let artifact_path = root.join("artifacts/mcu.bin");
    let builder = KlipperBuilder::new(&source_dir, FailingRunner);

    let error = builder
        .build(&BuildRequest {
            kconfig: "CONFIG_INVALID=y\n".to_owned(),
            config_path: config_path.clone(),
            artifact_path: artifact_path.clone(),
        })
        .expect_err("failed Make command should stop the build");

    match error {
        BuildError::CommandFailed { command, output } => {
            assert_eq!(command.arguments[0], "olddefconfig");
            assert_eq!(output.stderr, b"unknown Kconfig symbol\n");
        }
        other => panic!("expected command failure, got {other:?}"),
    }
    assert!(config_path.exists());
    assert!(!artifact_path.exists());

    fs::remove_dir_all(root).expect("test directory cleanup");
}

#[derive(Clone)]
struct FakeRunner {
    commands: Arc<Mutex<Vec<BuildCommand>>>,
}

impl FakeRunner {
    fn success() -> Self {
        Self {
            commands: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl CommandPort for FakeRunner {
    fn run(
        &self,
        command: &BuildCommand,
    ) -> Result<CommandOutput, mcu_update::build::CommandError> {
        self.commands
            .lock()
            .expect("runner lock")
            .push(command.clone());
        Ok(CommandOutput::success())
    }
}

struct FailingRunner;

impl CommandPort for FailingRunner {
    fn run(
        &self,
        _command: &BuildCommand,
    ) -> Result<CommandOutput, mcu_update::build::CommandError> {
        Ok(CommandOutput {
            success: false,
            stdout: Vec::new(),
            stderr: b"unknown Kconfig symbol\n".to_vec(),
        })
    }
}

struct ExpandingRunner {
    expanded_kconfig: String,
}

impl CommandPort for ExpandingRunner {
    fn run(
        &self,
        command: &BuildCommand,
    ) -> Result<CommandOutput, mcu_update::build::CommandError> {
        if command
            .arguments
            .first()
            .is_some_and(|argument| argument == "olddefconfig")
        {
            let config_path = command
                .arguments
                .iter()
                .find_map(|argument| argument.strip_prefix("KCONFIG_CONFIG="))
                .expect("olddefconfig command should include KCONFIG_CONFIG");
            fs::write(config_path, &self.expanded_kconfig).expect("expanded Kconfig should write");
        }
        Ok(CommandOutput::success())
    }
}

fn temporary_directory(name: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should be after epoch")
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "mcu-update-build-{name}-{}-{nonce}",
        std::process::id()
    ));
    assert!(!Path::new(&path).exists(), "test directory must be unique");
    path
}
