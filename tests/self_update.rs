use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use aldis::build::{BuildCommand, CommandError, CommandOutput, CommandPort};
use aldis::self_update::{ReleaseError, extract_binary, install_binary};

#[test]
fn extracts_the_binary_tar_produced_at_the_destination_directory() {
    let dir = temporary_directory();
    fs::create_dir_all(&dir).expect("create test dir");
    let dest = dir.join("dest");
    fs::create_dir_all(&dest).expect("create dest dir");
    let archive = dir.join("aldis.tar.xz");
    let runner = FakeRunner::new(vec![success_output()], {
        let dest = dest.clone();
        move || fs::write(dest.join("aldis"), b"binary").expect("simulate tar output")
    });

    let extracted = extract_binary(&runner, &archive, &dest).expect("extraction should succeed");

    assert_eq!(extracted, dest.join("aldis"));
    let commands = runner.commands.lock().expect("runner lock");
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].program, "tar");
    assert!(commands[0].arguments.contains(&"xJf".to_owned()));
    assert!(
        commands[0]
            .arguments
            .contains(&"--strip-components=1".to_owned())
    );

    fs::remove_dir_all(dir).expect("cleanup");
}

#[test]
fn reports_tar_failure_output_instead_of_pretending_to_have_extracted() {
    let dir = temporary_directory();
    fs::create_dir_all(&dir).expect("create test dir");
    let archive = dir.join("aldis.tar.xz");
    let runner = FakeRunner::new(
        vec![CommandOutput {
            success: false,
            stdout: Vec::new(),
            stderr: b"tar: short read".to_vec(),
        }],
        || {},
    );

    let error = extract_binary(&runner, &archive, &dir).expect_err("tar failure should surface");

    assert!(
        matches!(error, ReleaseError::ExtractFailed(message) if message.contains("short read"))
    );

    fs::remove_dir_all(dir).expect("cleanup");
}

#[test]
fn reports_a_missing_binary_after_extraction_reports_success() {
    let dir = temporary_directory();
    fs::create_dir_all(&dir).expect("create test dir");
    let archive = dir.join("aldis.tar.xz");
    let runner = FakeRunner::new(vec![success_output()], || {});

    let error = extract_binary(&runner, &archive, &dir).expect_err("missing binary should error");

    assert!(matches!(error, ReleaseError::MissingBinary));

    fs::remove_dir_all(dir).expect("cleanup");
}

#[test]
fn installs_the_new_binary_executable_and_atomically_in_place() {
    let dir = temporary_directory();
    fs::create_dir_all(&dir).expect("create test dir");
    let new_binary = dir.join("aldis.new");
    fs::write(&new_binary, b"new binary contents").expect("write staged binary");
    fs::set_permissions(&new_binary, fs::Permissions::from_mode(0o644)).expect("clear exec bit");
    let current_exe = dir.join("aldis");
    fs::write(&current_exe, b"old binary contents").expect("write old binary");

    install_binary(&new_binary, &current_exe).expect("install should succeed");

    assert_eq!(
        fs::read(&current_exe).expect("read installed binary"),
        b"new binary contents"
    );
    let mode = fs::metadata(&current_exe)
        .expect("installed binary metadata")
        .permissions()
        .mode();
    assert_eq!(mode & 0o111, 0o111, "installed binary should be executable");
    assert!(
        !dir.join(".aldis.new").exists(),
        "the temporary staging file should not remain after install"
    );

    fs::remove_dir_all(dir).expect("cleanup");
}

#[derive(Clone)]
struct FakeRunner {
    commands: Arc<Mutex<Vec<BuildCommand>>>,
    outputs: Arc<Mutex<Vec<CommandOutput>>>,
    on_run: Arc<dyn Fn() + Send + Sync>,
}

impl FakeRunner {
    fn new(outputs: Vec<CommandOutput>, on_run: impl Fn() + Send + Sync + 'static) -> Self {
        Self {
            commands: Arc::new(Mutex::new(Vec::new())),
            outputs: Arc::new(Mutex::new(outputs)),
            on_run: Arc::new(on_run),
        }
    }
}

impl CommandPort for FakeRunner {
    fn run(&self, command: &BuildCommand) -> Result<CommandOutput, CommandError> {
        self.commands
            .lock()
            .expect("runner lock")
            .push(command.clone());
        (self.on_run)();
        let mut outputs = self.outputs.lock().expect("runner lock");
        if outputs.is_empty() {
            return Err(CommandError::Spawn(std::io::Error::other(
                "missing fake output",
            )));
        }
        Ok(outputs.remove(0))
    }
}

fn success_output() -> CommandOutput {
    CommandOutput::success()
}

fn temporary_directory() -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should be after epoch")
        .as_nanos();
    std::env::temp_dir().join(format!("aldis-self-update-{}-{nonce}", std::process::id()))
}
