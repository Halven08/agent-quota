use std::fs;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

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
fn version_is_successful_and_machine_friendly() {
    let output = binary().arg("--version").output().expect("CLI should run");
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        concat!("agent-quota ", env!("CARGO_PKG_VERSION"))
    );
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
fn check_returns_not_ready_exit_code_for_exhausted_quota() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should be available")
        .as_nanos();
    let extension = if cfg!(windows) { "cmd" } else { "sh" };
    let command_path = std::env::temp_dir().join(format!(
        "agent-quota-cli-codex-{}-{unique}.{extension}",
        std::process::id()
    ));
    #[cfg(windows)]
    let script = concat!(
        "@echo off\r\n",
        "echo {\"id\":2,\"result\":{\"account\":{\"planType\":\"plus\"}}}\r\n",
        "echo {\"id\":3,\"result\":{\"rateLimits\":{\"primary\":{\"usedPercent\":100,\"windowDurationMins\":300,\"resetsAt\":1730947200}}}}\r\n",
        "ping -n 3 127.0.0.1 >nul\r\n"
    );
    #[cfg(unix)]
    let script = concat!(
        "#!/bin/sh\n",
        "printf '%s\\n' '{\"id\":2,\"result\":{\"account\":{\"planType\":\"plus\"}}}'\n",
        "printf '%s\\n' '{\"id\":3,\"result\":{\"rateLimits\":{\"primary\":{\"usedPercent\":100,\"windowDurationMins\":300,\"resetsAt\":1730947200}}}}'\n",
        "sleep 2\n"
    );
    fs::write(&command_path, script).expect("mock Codex executable should write");
    #[cfg(unix)]
    {
        let mut permissions = fs::metadata(&command_path)
            .expect("mock metadata should read")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&command_path, permissions).expect("mock should become executable");
    }

    let config_path = std::env::temp_dir().join(format!(
        "agent-quota-cli-check-{}-{unique}.toml",
        std::process::id()
    ));
    let command_literal = command_path.display().to_string().replace('\'', "''");
    fs::write(
        &config_path,
        format!(
            "[[profiles]]\nid = \"codex-test\"\nprovider = \"codex\"\ncommand_path = '{command_literal}'\n"
        ),
    )
    .expect("config fixture should write");

    let output = binary()
        .args(["check", "--config"])
        .arg(&config_path)
        .output()
        .expect("CLI should run");
    let _ = fs::remove_file(config_path);
    let _ = fs::remove_file(command_path);

    assert_eq!(output.status.code(), Some(4));
    assert!(String::from_utf8_lossy(&output.stdout).contains("Readiness: NOT READY"));
    assert!(output.stderr.is_empty());
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
