use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use aldis::checkout::{CheckoutError, refresh, revision};
use aldis::eligibility::CheckoutRevision;

#[test]
fn fast_forwards_the_configured_upstream_branch() {
    let root = temporary_directory("fast-forward");
    let remote_path = root.join("remote.git");
    let source_path = root.join("source");
    let checkout_path = root.join("checkout");
    init_bare(&remote_path);
    init(&source_path);
    commit_file(&source_path, "README", "first", "initial");
    push_master(&source_path, &remote_path);

    clone(&remote_path, &checkout_path);
    let before = head_commit(&checkout_path);
    let writer_path = root.join("writer");
    clone(&remote_path, &writer_path);
    commit_file(&writer_path, "new-file", "new", "remote update");
    push_master(&writer_path, &remote_path);

    let result = refresh(&checkout_path).expect("fast-forward refresh");

    assert!(result.advanced);
    assert_eq!(result.commits_advanced, 1);
    assert_ne!(result.before, result.after);
    assert_ne!(head_commit(&checkout_path), before);
    assert_eq!(
        fs::read_to_string(checkout_path.join("new-file")).expect("new file"),
        "new"
    );
    fs::remove_dir_all(root).expect("test directory cleanup");
}

#[test]
fn preserves_non_conflicting_local_modifications_during_fast_forward() {
    let root = temporary_directory("local-change");
    let remote_path = root.join("remote.git");
    let source_path = root.join("source");
    let checkout_path = root.join("checkout");
    init_bare(&remote_path);
    init(&source_path);
    commit_file(&source_path, "README", "first", "initial");
    push_master(&source_path, &remote_path);

    clone(&remote_path, &checkout_path);
    fs::write(checkout_path.join("local-plugin.py"), "local plugin").expect("local plugin");
    let writer_path = root.join("writer");
    clone(&remote_path, &writer_path);
    commit_file(&writer_path, "remote-file", "remote", "remote update");
    push_master(&writer_path, &remote_path);

    let result = refresh(&checkout_path).expect("fast-forward refresh");

    assert!(result.advanced);
    assert_eq!(
        fs::read_to_string(checkout_path.join("local-plugin.py")).expect("local plugin"),
        "local plugin"
    );
    assert_eq!(
        fs::read_to_string(checkout_path.join("remote-file")).expect("remote file"),
        "remote"
    );
    fs::remove_dir_all(root).expect("test directory cleanup");
}

#[test]
fn fast_forwards_a_shallow_checkout() {
    let root = temporary_directory("shallow");
    let remote_path = root.join("remote.git");
    let source_path = root.join("source");
    let checkout_path = root.join("checkout");
    init_bare(&remote_path);
    init(&source_path);
    commit_file(&source_path, "README", "first", "initial");
    push_master(&source_path, &remote_path);

    clone_with(
        &format!("file://{}", remote_path.display()),
        &checkout_path,
        &["--depth", "1"],
    );
    assert_eq!(
        run(&checkout_path, &["rev-parse", "--is-shallow-repository"]),
        "true"
    );

    commit_file(&source_path, "remote-file", "remote", "remote update");
    push_master(&source_path, &remote_path);

    let result = refresh(&checkout_path).expect("fast-forward a shallow checkout");

    assert!(result.advanced);
    assert_eq!(result.commits_advanced, 1);
    assert_eq!(
        fs::read_to_string(checkout_path.join("remote-file")).expect("remote file"),
        "remote"
    );
    fs::remove_dir_all(root).expect("test directory cleanup");
}

#[test]
fn aborts_before_changing_head_when_the_checkout_has_diverged() {
    let root = temporary_directory("diverged");
    let remote_path = root.join("remote.git");
    let source_path = root.join("source");
    let checkout_path = root.join("checkout");
    init_bare(&remote_path);
    init(&source_path);
    commit_file(&source_path, "README", "first", "initial");
    push_master(&source_path, &remote_path);

    clone(&remote_path, &checkout_path);
    commit_file(&checkout_path, "local-file", "local", "local update");
    let before = head_commit(&checkout_path);
    let writer_path = root.join("writer");
    clone(&remote_path, &writer_path);
    commit_file(&writer_path, "remote-file", "remote", "remote update");
    push_master(&writer_path, &remote_path);

    assert!(matches!(
        refresh(&checkout_path),
        Err(CheckoutError::NotFastForward { .. })
    ));
    assert_eq!(head_commit(&checkout_path), before);
    assert!(!checkout_path.join("remote-file").exists());
    fs::remove_dir_all(root).expect("test directory cleanup");
}

#[test]
fn reports_tracked_changes_without_treating_untracked_plugins_as_dirty() {
    let root = temporary_directory("revision");
    init(&root);
    commit_file(&root, "README", "first", "initial");
    fs::write(root.join("local-plugin.py"), "local plugin").expect("local plugin");

    assert!(
        matches!(revision(&root), Ok(CheckoutRevision::Known(value)) if !value.ends_with("-dirty"))
    );

    fs::write(root.join("README"), "changed").expect("tracked change");
    assert!(
        matches!(revision(&root), Ok(CheckoutRevision::Known(value)) if value.ends_with("-dirty"))
    );

    fs::remove_dir_all(root).expect("test directory cleanup");
}

fn run(path: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(args)
        .env("LC_ALL", "C")
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn init(path: &Path) {
    fs::create_dir_all(path).expect("fixture directory");
    run(path, &["init", "--initial-branch=master"]);
    run(path, &["config", "user.name", "aldis test"]);
    run(path, &["config", "user.email", "test@example.com"]);
}

fn init_bare(path: &Path) {
    fs::create_dir_all(path).expect("fixture directory");
    run(path, &["init", "--bare", "--initial-branch=master"]);
}

fn clone(remote_path: &Path, checkout_path: &Path) {
    clone_with(
        remote_path.to_str().expect("UTF-8 path"),
        checkout_path,
        &[],
    );
}

// A local `git clone` silently ignores `--depth`; only a `file://` remote
// honors it, so the shallow-checkout test passes one explicitly.
fn clone_with(remote: &str, checkout_path: &Path, extra_args: &[&str]) {
    let status = Command::new("git")
        .arg("clone")
        .args(extra_args)
        .arg(remote)
        .arg(checkout_path)
        .status()
        .expect("run git clone");
    assert!(status.success());
    run(checkout_path, &["config", "user.name", "aldis test"]);
    run(checkout_path, &["config", "user.email", "test@example.com"]);
}

fn commit_file(repository_path: &Path, file_name: &str, contents: &str, message: &str) {
    fs::write(repository_path.join(file_name), contents).expect("fixture file");
    run(repository_path, &["add", file_name]);
    run(repository_path, &["commit", "-m", message]);
}

fn push_master(repository_path: &Path, remote_path: &Path) {
    let remotes = run(repository_path, &["remote"]);
    if !remotes.lines().any(|remote| remote == "origin") {
        run(
            repository_path,
            &[
                "remote",
                "add",
                "origin",
                remote_path.to_str().expect("UTF-8 path"),
            ],
        );
    }
    run(repository_path, &["push", "origin", "master:master"]);
}

fn head_commit(repository_path: &Path) -> String {
    run(repository_path, &["rev-parse", "HEAD"])
}

fn temporary_directory(name: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should be after epoch")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "aldis-checkout-{name}-{}-{nonce}",
        std::process::id()
    ))
}
