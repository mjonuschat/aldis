use aldis::lock::{LockError, UpdateLock, default_lock_path};

#[test]
fn refuses_a_second_holder_until_the_first_releases() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("aldis.lock");

    let first = UpdateLock::try_acquire(&path).expect("first holder");
    assert!(matches!(
        UpdateLock::try_acquire(&path),
        Err(LockError::Busy)
    ));

    drop(first);
    assert!(UpdateLock::try_acquire(&path).is_ok());
}

#[test]
fn shares_one_lock_between_a_user_and_their_sudo_runs() {
    use aldis::lock::lock_path_for;

    assert_eq!(
        lock_path_for(1000, None),
        std::path::PathBuf::from("/tmp/aldis-1000.lock")
    );
    assert_eq!(
        lock_path_for(0, Some("1000")),
        std::path::PathBuf::from("/tmp/aldis-1000.lock")
    );
    assert_eq!(
        lock_path_for(0, None),
        std::path::PathBuf::from("/tmp/aldis-0.lock")
    );
    assert_eq!(
        lock_path_for(1000, Some("0")),
        std::path::PathBuf::from("/tmp/aldis-1000.lock")
    );
}

#[test]
fn locks_an_existing_file_it_can_only_read() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("aldis.lock");
    std::fs::write(&path, "").unwrap();
    // Stands in for a lock file another account (e.g. root via sudo) created with mode 0644.
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444)).unwrap();

    let first = UpdateLock::try_acquire(&path).expect("read-only lock file is lockable");
    assert!(matches!(
        UpdateLock::try_acquire(&path),
        Err(LockError::Busy)
    ));
    drop(first);
}

#[test]
fn uses_a_fixed_per_user_path_under_tmp() {
    let path = default_lock_path().expect("lock path");
    let name = path.file_name().unwrap().to_string_lossy().into_owned();

    assert_eq!(path.parent(), Some(std::path::Path::new("/tmp")));
    assert!(
        name.starts_with("aldis-") && name.ends_with(".lock"),
        "{name}"
    );
}
