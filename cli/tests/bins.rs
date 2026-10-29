//! The built binaries never print the RPC URL, which may embed a provider API key.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::disallowed_macros,
    missing_docs
)]

use std::process::Command;

use solana_keypair::Keypair;

const URL: &str = "http://127.0.0.1:9/SYNTHETIC-PROVIDER-KEY";
const PROGRAM: &str = "11111111111111111111111111111111";

fn assert_no_url(output: &std::process::Output) {
    assert!(!output.status.success());
    for stream in [&output.stdout, &output.stderr] {
        let text = String::from_utf8_lossy(stream);
        assert!(!text.contains("SYNTHETIC-PROVIDER-KEY"), "{text}");
        assert!(!text.contains("127.0.0.1:9"), "{text}");
    }
}

/// Codex's exact command.
#[test]
fn test_admin_show_does_not_print_the_url() {
    let output = Command::new(env!("CARGO_BIN_EXE_quotebook-admin"))
        .args(["show", "--url", URL, "--program-id", PROGRAM])
        .output()
        .unwrap();
    assert_no_url(&output);
    assert!(String::from_utf8_lossy(&output.stderr).contains("<redacted-url>"));
}

#[test]
fn test_maker_does_not_print_the_url() {
    let dir = std::env::temp_dir().join(format!("qb-bins-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let key = dir.join("maker-test-key.json");
    let bytes = Keypair::new().to_bytes().to_vec();
    std::fs::write(&key, format!("{bytes:?}")).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_quotebook-maker"))
        .args([
            "--url",
            URL,
            "--program-id",
            PROGRAM,
            "--series",
            PROGRAM,
            "--maker",
            key.to_str().unwrap(),
            "--maker-base",
            PROGRAM,
            "--maker-quote",
            PROGRAM,
            "--listen",
            "127.0.0.1:0",
            "--mid",
            "25",
        ])
        .output()
        .unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
    assert_no_url(&output);
}
