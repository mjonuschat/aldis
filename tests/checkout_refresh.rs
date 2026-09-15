use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use aldis::checkout::{CheckoutError, refresh, revision};
use aldis::eligibility::CheckoutRevision;
use git2::{Commit, Repository, Signature};

#[test]
fn fast_forwards_the_configured_upstream_branch() {
    let root = temporary_directory("fast-forward");
    let remote_path = root.join("remote.git");
    let source_path = root.join("source");
    let checkout_path = root.join("checkout");
    let remote = Repository::init_bare(&remote_path).expect("remote repository");
    let source = Repository::init(&source_path).expect("source repository");
    commit_file(&source, "README", "first", "initial");
    push_master(&source, &remote_path);
    drop(remote);

    let checkout = Repository::clone(remote_path.to_str().expect("UTF-8 path"), &checkout_path)
        .expect("checkout clone");
    let before = checkout
        .head()
        .expect("checkout head")
        .target()
        .expect("head target");
    let writer = Repository::clone(
        remote_path.to_str().expect("UTF-8 path"),
        root.join("writer"),
    )
    .expect("writer clone");
    commit_file(&writer, "new-file", "new", "remote update");
    push_master(&writer, &remote_path);

    let result = refresh(&checkout_path).expect("fast-forward refresh");

    assert!(result.advanced);
    assert_eq!(result.commits_advanced, 1);
    assert_ne!(result.before, result.after);
    assert_ne!(
        Repository::open(&checkout_path)
            .expect("reopen checkout")
            .head()
            .expect("head")
            .target()
            .expect("target"),
        before
    );
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
    Repository::init_bare(&remote_path).expect("remote repository");
    let source = Repository::init(&source_path).expect("source repository");
    commit_file(&source, "README", "first", "initial");
    push_master(&source, &remote_path);

    Repository::clone(remote_path.to_str().expect("UTF-8 path"), &checkout_path)
        .expect("checkout clone");
    fs::write(checkout_path.join("local-plugin.py"), "local plugin").expect("local plugin");
    let writer = Repository::clone(
        remote_path.to_str().expect("UTF-8 path"),
        root.join("writer"),
    )
    .expect("writer clone");
    commit_file(&writer, "remote-file", "remote", "remote update");
    push_master(&writer, &remote_path);

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
fn aborts_before_changing_head_when_the_checkout_has_diverged() {
    let root = temporary_directory("diverged");
    let remote_path = root.join("remote.git");
    let source_path = root.join("source");
    let checkout_path = root.join("checkout");
    Repository::init_bare(&remote_path).expect("remote repository");
    let source = Repository::init(&source_path).expect("source repository");
    commit_file(&source, "README", "first", "initial");
    push_master(&source, &remote_path);

    let checkout = Repository::clone(remote_path.to_str().expect("UTF-8 path"), &checkout_path)
        .expect("checkout clone");
    commit_file(&checkout, "local-file", "local", "local update");
    let before = checkout.head().expect("head").target().expect("target");
    let writer = Repository::clone(
        remote_path.to_str().expect("UTF-8 path"),
        root.join("writer"),
    )
    .expect("writer clone");
    commit_file(&writer, "remote-file", "remote", "remote update");
    push_master(&writer, &remote_path);

    assert!(matches!(
        refresh(&checkout_path),
        Err(CheckoutError::NotFastForward { .. })
    ));
    assert_eq!(
        Repository::open(&checkout_path)
            .expect("reopen checkout")
            .head()
            .expect("head")
            .target()
            .expect("target"),
        before
    );
    assert!(!checkout_path.join("remote-file").exists());
    fs::remove_dir_all(root).expect("test directory cleanup");
}

#[test]
fn reports_tracked_changes_without_treating_untracked_plugins_as_dirty() {
    let root = temporary_directory("revision");
    let repository = Repository::init(&root).expect("repository");
    commit_file(&repository, "README", "first", "initial");
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

fn commit_file(repository: &Repository, file_name: &str, contents: &str, message: &str) {
    fs::write(
        repository
            .workdir()
            .expect("non-bare repository")
            .join(file_name),
        contents,
    )
    .expect("fixture file");
    let mut index = repository.index().expect("index");
    index.add_path(Path::new(file_name)).expect("index path");
    index.write().expect("write index");
    let tree_id = index.write_tree().expect("write tree");
    let tree = repository.find_tree(tree_id).expect("tree");
    let signature = Signature::now("aldis test", "test@example.com").expect("signature");
    let parents = repository
        .head()
        .ok()
        .and_then(|head| head.peel_to_commit().ok())
        .into_iter()
        .collect::<Vec<Commit<'_>>>();
    let parent_references = parents.iter().collect::<Vec<_>>();
    repository
        .commit(
            Some("HEAD"),
            &signature,
            &signature,
            message,
            &tree,
            &parent_references,
        )
        .expect("commit");
}

fn push_master(repository: &Repository, remote_path: &Path) {
    let mut remote = repository
        .find_remote("origin")
        .or_else(|_| repository.remote("origin", remote_path.to_str().expect("UTF-8 path")))
        .expect("origin remote");
    remote
        .push(&["refs/heads/master:refs/heads/master"], None)
        .expect("push master");
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
