use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use aldis::build::{
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
            clean: false,
        })
        .expect("build should succeed");

    let checkout_config_path = source_dir.join(".config");
    assert_eq!(
        fs::read_to_string(&config_path).expect("written audit Kconfig"),
        "CONFIG_MACH_STM32G0B1=y\n"
    );
    assert_eq!(
        fs::read_to_string(&checkout_config_path).expect("written checkout Kconfig"),
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
            format!("KCONFIG_CONFIG={}", checkout_config_path.display()),
        ]
    );
    assert_eq!(
        commands[1].arguments,
        vec![
            format!("KCONFIG_CONFIG={}", checkout_config_path.display()),
            format!(
                "-j{}",
                std::thread::available_parallelism().map_or(1, |n| n.get())
            ),
        ]
    );

    fs::remove_dir_all(root).expect("test directory cleanup");
}

#[test]
fn leaves_an_unchanged_checkout_kconfig_untouched_so_make_can_build_incrementally() {
    let root = temporary_directory("unchanged");
    let source_dir = root.join("klipper");
    let config_path = root.join("run/mcu/.config");
    let artifact_path = root.join("artifacts/mcu.bin");
    fs::create_dir_all(source_dir.join("out")).expect("source output directory should exist");
    fs::write(source_dir.join("out/klipper.bin"), b"firmware").expect("fixture artifact");
    let builder = KlipperBuilder::new(&source_dir, FakeRunner::success());
    let request = BuildRequest {
        kconfig: "CONFIG_MACH_STM32G0B1=y\n".to_owned(),
        config_path,
        artifact_path,
        clean: false,
    };

    builder.build(&request).expect("first build should succeed");
    let checkout_config_path = source_dir.join(".config");
    let first_modified = fs::metadata(&checkout_config_path)
        .expect("checkout Kconfig metadata")
        .modified()
        .expect("modified time");

    std::thread::sleep(std::time::Duration::from_millis(10));
    builder
        .build(&request)
        .expect("second build with unchanged Kconfig should succeed");
    let second_modified = fs::metadata(&checkout_config_path)
        .expect("checkout Kconfig metadata")
        .modified()
        .expect("modified time");

    assert_eq!(first_modified, second_modified);
    fs::remove_dir_all(root).expect("test directory cleanup");
}

#[test]
fn runs_make_clean_before_configuring_when_requested() {
    let root = temporary_directory("clean");
    let source_dir = root.join("klipper");
    let config_path = root.join("run/mcu/.config");
    let artifact_path = root.join("artifacts/mcu.bin");
    fs::create_dir_all(source_dir.join("out")).expect("source output directory should exist");
    fs::write(source_dir.join("out/klipper.bin"), b"firmware").expect("fixture artifact");
    let runner = FakeRunner::success();
    let builder = KlipperBuilder::new(&source_dir, runner.clone());

    builder
        .build(&BuildRequest {
            kconfig: "CONFIG_MACH_STM32G0B1=y\n".to_owned(),
            config_path,
            artifact_path,
            clean: true,
        })
        .expect("build should succeed");

    let commands = runner.commands.lock().expect("runner lock");
    assert_eq!(commands.len(), 3);
    assert_eq!(commands[0].arguments, vec!["clean".to_owned()]);
    assert_eq!(commands[1].arguments[0], "olddefconfig");
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
            clean: false,
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
fn copies_the_elf_artifact_for_a_linux_host_build() {
    let root = temporary_directory("linux-host");
    let source_dir = root.join("klipper");
    let config_path = root.join("run/mcu host/.config");
    let artifact_path = root.join("artifacts/mcu host.bin");
    fs::create_dir_all(source_dir.join("out")).expect("source output directory should exist");
    fs::write(source_dir.join("out/klipper.elf"), b"\x7fELF host").expect("fixture artifact");
    let builder = KlipperBuilder::new(&source_dir, FakeRunner::success());

    builder
        .build(&BuildRequest {
            kconfig: "CONFIG_LOW_LEVEL_OPTIONS=y\nCONFIG_MACH_LINUX=y\n".to_owned(),
            config_path,
            artifact_path: artifact_path.clone(),
            clean: false,
        })
        .expect("build should succeed");

    assert_eq!(
        fs::read(&artifact_path).expect("copied artifact"),
        b"\x7fELF host"
    );
    fs::remove_dir_all(root).expect("test directory cleanup");
}

#[test]
fn forces_the_version_to_be_re_embedded_before_compiling() {
    let root = temporary_directory("reembed-version");
    let source_dir = root.join("klipper");
    fs::create_dir_all(source_dir.join("out")).expect("source output directory should exist");
    fs::write(source_dir.join("out/compile_time_request.o"), b"stale").expect("stale object");
    fs::write(source_dir.join("out/klipper.bin"), b"firmware").expect("fixture artifact");
    let runner = VersionObjectRunner {
        object: source_dir.join("out/compile_time_request.o"),
        present_at_compile: Arc::new(Mutex::new(None)),
    };
    let builder = KlipperBuilder::new(&source_dir, runner.clone());

    builder
        .build(&BuildRequest {
            kconfig: "CONFIG_MACH_STM32=y\n".to_owned(),
            config_path: root.join("run/mcu/.config"),
            artifact_path: root.join("artifacts/mcu.bin"),
            clean: false,
        })
        .expect("build should succeed");

    assert_eq!(
        *runner.present_at_compile.lock().expect("runner lock"),
        Some(false),
        "make must not see the stale version object, or it keeps the old version string"
    );
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
            clean: false,
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
    fs::create_dir_all(&source_dir).expect("source checkout directory should exist");
    let builder = KlipperBuilder::new(&source_dir, FailingRunner);

    let error = builder
        .build(&BuildRequest {
            kconfig: "CONFIG_INVALID=y\n".to_owned(),
            config_path: config_path.clone(),
            artifact_path: artifact_path.clone(),
            clean: false,
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
    fn run(&self, command: &BuildCommand) -> Result<CommandOutput, aldis::build::CommandError> {
        self.commands
            .lock()
            .expect("runner lock")
            .push(command.clone());
        Ok(CommandOutput::success())
    }
}

struct FailingRunner;

impl CommandPort for FailingRunner {
    fn run(&self, _command: &BuildCommand) -> Result<CommandOutput, aldis::build::CommandError> {
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
    fn run(&self, command: &BuildCommand) -> Result<CommandOutput, aldis::build::CommandError> {
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
    let path =
        std::env::temp_dir().join(format!("aldis-build-{name}-{}-{nonce}", std::process::id()));
    assert!(!Path::new(&path).exists(), "test directory must be unique");
    path
}

#[derive(Clone)]
struct VersionObjectRunner {
    object: PathBuf,
    present_at_compile: Arc<Mutex<Option<bool>>>,
}

impl CommandPort for VersionObjectRunner {
    fn run(&self, command: &BuildCommand) -> Result<CommandOutput, aldis::build::CommandError> {
        if command
            .arguments
            .iter()
            .any(|argument| argument.starts_with("-j"))
        {
            *self.present_at_compile.lock().expect("runner lock") = Some(self.object.exists());
        }
        Ok(CommandOutput::success())
    }
}
