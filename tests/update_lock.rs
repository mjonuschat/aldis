use aldis::lock::{LockError, UpdateLock};

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
fn uses_one_fixed_path_for_every_user() {
    assert_eq!(
        aldis::lock::default_lock_path(),
        std::path::PathBuf::from("/tmp/aldis.lock")
    );
}

#[test]
fn creates_the_lock_file_readable_by_other_users() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("aldis.lock");

    let _held = UpdateLock::try_acquire(&path).expect("holder");

    let mode = std::fs::metadata(&path).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o644);
}
