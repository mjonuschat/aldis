use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use aldis::self_update::install_binary;

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

fn temporary_directory() -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should be after epoch")
        .as_nanos();
    std::env::temp_dir().join(format!("aldis-self-update-{}-{nonce}", std::process::id()))
}
