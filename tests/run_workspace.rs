use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use mcu_update::workspace::RunWorkspace;

#[test]
fn creates_isolated_paths_for_a_selected_mcu() {
    let root = unique_temporary_path();

    let workspace = RunWorkspace::create(root.clone()).expect("workspace should create");
    let paths = workspace
        .build_paths("mcu toolhead")
        .expect("target paths should create");

    assert_eq!(workspace.root(), root);
    assert!(paths.config_path.starts_with(&root));
    assert!(paths.artifact_path.starts_with(&root));
    assert!(paths.config_path.ends_with(".config"));
    assert!(paths.artifact_path.ends_with("klipper.bin"));
    assert!(!paths.config_path.exists());
    assert!(!paths.artifact_path.exists());

    fs::remove_dir_all(root).expect("test directory cleanup");
}

fn unique_temporary_path() -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should be after epoch")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "mcu-update-workspace-{}-{nonce}",
        std::process::id()
    ))
}
