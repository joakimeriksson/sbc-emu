use std::process::Command;

fn run(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_sbc-emu"))
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn lists_support_honestly_and_rejects_before_opening_images() {
    let output = run(&["boards", "list"]);
    assert!(output.status.success());
    let listing = String::from_utf8(output.stdout).unwrap();
    assert!(listing
        .lines()
        .any(|l| l.starts_with("orangepi5") && l.contains("not implemented")));
    for (board, error) in [
        ("orangepi5", "not implemented"),
        ("pi3b", "not implemented"),
        ("typo", "unknown board"),
    ] {
        let output = run(&[
            "run",
            "--board",
            board,
            "--kernel",
            "/nonexistent/sbc-board-test-image",
        ]);
        assert_eq!(output.status.code(), Some(2));
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(stderr.contains(error), "{stderr}");
        assert!(
            !stderr.contains("sbc-board-test-image"),
            "image was opened before board validation"
        );
    }
}

#[test]
fn default_and_explicit_pi_reach_the_same_adapter() {
    let default = run(&["run", "--kernel", "/nonexistent/sbc-board-test-image"]);
    assert_eq!(default.status.code(), Some(1));
    for selector in [vec!["--board", "pi4b"], vec!["--board=pi4b"]] {
        let mut args = vec!["run"];
        args.extend(selector);
        args.extend(["--kernel", "/nonexistent/sbc-board-test-image"]);
        let explicit = run(&args);
        assert_eq!(explicit.status.code(), default.status.code());
        assert_eq!(explicit.stderr, default.stderr);
    }
}

#[test]
fn malformed_selector_cannot_silently_boot_pi() {
    for args in [
        vec!["run", "--board"],
        vec!["run", "--board", "--kernel", "missing"],
        vec!["run", "--board="],
        vec!["run", "--board", "pi4b", "--board", "pi4b"],
    ] {
        let output = run(&args);
        assert_eq!(output.status.code(), Some(2));
        assert!(String::from_utf8(output.stderr)
            .unwrap()
            .contains("--board"));
    }
}
