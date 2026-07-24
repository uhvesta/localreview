use std::process::Command;

#[test]
fn json_mode_reports_parse_failures_as_one_stable_object() {
    let output = Command::new(env!("CARGO_BIN_EXE_localreview"))
        .args(["--json", "prompt", "workspace", "--scope", "not-a-scope"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    let error: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(error["ok"], false);
    assert_eq!(error["code"], "usage");
    assert!(error["message"].as_str().is_some_and(|message| {
        message.contains("invalid value") && message.contains("not-a-scope")
    }));
}

#[test]
fn help_remains_a_successful_human_readable_operation() {
    let output = Command::new(env!("CARGO_BIN_EXE_localreview"))
        .arg("--help")
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("Usage: localreview"));
}
