use std::fs;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn binary() -> Command {
    Command::new(env!("CARGO_BIN_EXE_agent-quota"))
}

#[test]
fn help_is_successful_and_uses_stdout() {
    let output = binary().arg("--help").output().expect("CLI should run");
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("Agent Quota"));
    assert!(output.stderr.is_empty());
}

#[test]
fn unsafe_watch_interval_is_rejected_without_probing() {
    let output = binary()
        .args(["watch", "--interval", "0"])
        .output()
        .expect("CLI should run");
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("at least 60 seconds"));
    assert!(output.stdout.is_empty());
}

#[test]
fn unknown_config_fields_are_actionable() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should be available")
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "agent-quota-cli-config-{}-{unique}.toml",
        std::process::id()
    ));
    fs::write(
        &path,
        r#"
            [[profiles]]
            id = "codex"
            provider = "codex"
            comand_path = "codex"
        "#,
    )
    .expect("config fixture should write");
    let output = binary()
        .args(["profiles", "list", "--config"])
        .arg(&path)
        .output()
        .expect("CLI should run");
    let _ = fs::remove_file(path);

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("unknown field"));
    assert!(stderr.contains("comand_path"));
}
