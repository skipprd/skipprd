#[test]
fn chat_help_lists_subcommands() {
    let exe = option_env!("CARGO_BIN_EXE_skippr")
        .expect("CARGO_BIN_EXE_skippr set for integration tests");
    let out = std::process::Command::new(exe)
        .args(["chat", "--help"])
        .output()
        .expect("run skippr chat --help");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("send"), "stdout: {s}");
    assert!(s.contains("threads"), "stdout: {s}");
    assert!(s.contains("docs-search"), "stdout: {s}");
}

#[test]
fn ask_help_still_documents_cli() {
    let exe = option_env!("CARGO_BIN_EXE_skippr")
        .expect("CARGO_BIN_EXE_skippr set for integration tests");
    let out = std::process::Command::new(exe)
        .args(["ask", "--help"])
        .output()
        .expect("run skippr ask --help");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(
        s.contains("question") || s.contains("pipeline"),
        "stdout: {s}"
    );
}
