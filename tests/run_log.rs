use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use aldis::run_log::RunLog;

#[test]
fn retains_actions_for_a_run() {
    let root = temporary_directory();
    fs::create_dir_all(&root).expect("create run directory");
    let log = RunLog::create(&root).expect("create run log");
    log.action("starting firmware build");

    let contents = fs::read_to_string(log.path()).expect("read run log");
    assert!(contents.contains("action: run started"));
    assert!(contents.contains("action: starting firmware build"));

    fs::remove_dir_all(root).expect("remove test directory");
}

fn temporary_directory() -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should be after epoch")
        .as_nanos();
    std::env::temp_dir().join(format!("aldis-run-log-{}-{nonce}", std::process::id()))
}
