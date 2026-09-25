use aldis::build::{BuildCommand, CommandPort, CommandStdin, SystemCommandAdapter};

fn command(program: &str, arguments: &[&str], stdin: Option<Vec<u8>>) -> BuildCommand {
    BuildCommand {
        program: program.to_owned(),
        arguments: arguments
            .iter()
            .map(|argument| (*argument).to_owned())
            .collect(),
        current_dir: None,
        stdin: stdin.map(CommandStdin),
    }
}

#[test]
fn delivers_the_stdin_payload_to_the_child() {
    let payload = b"\x7fELF payload\n".to_vec();

    let output = SystemCommandAdapter
        .run(&command("cat", &[], Some(payload.clone())))
        .expect("cat should run");

    assert!(output.success);
    assert_eq!(output.stdout, payload);
}

#[test]
fn delivers_a_payload_larger_than_the_pipe_buffer() {
    let payload = vec![0xa5; 4 * 1024 * 1024];

    let output = SystemCommandAdapter
        .run(&command("wc", &["-c"], Some(payload)))
        .expect("wc should run");

    assert!(output.success);
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "4194304");
}

#[test]
fn reports_the_childs_failure_when_it_exits_without_reading_stdin() {
    let payload = vec![0; 4 * 1024 * 1024];

    let output = SystemCommandAdapter
        .run(&command(
            "sh",
            &["-c", "echo 'sudo: a password is required' >&2; exit 1"],
            Some(payload),
        ))
        .expect("an early exit must still produce the child's output");

    assert!(!output.success);
    assert_eq!(
        String::from_utf8_lossy(&output.stderr).trim(),
        "sudo: a password is required"
    );
}

#[test]
fn runs_without_stdin_exactly_as_before() {
    let output = SystemCommandAdapter
        .run(&command("cat", &[], None))
        .expect("cat should run");

    assert!(output.success);
    assert!(output.stdout.is_empty());
}

#[test]
fn debug_output_never_contains_the_payload() {
    let rendered = format!("{:?}", CommandStdin(b"secret firmware".to_vec()));

    assert_eq!(rendered, "CommandStdin(15 bytes)");
}
