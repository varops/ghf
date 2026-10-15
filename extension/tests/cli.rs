use std::process::Command;

use serde_json::Value;

fn git_hf(args: &[&str]) -> (i32, Value) {
    let out = Command::new(env!("CARGO_BIN_EXE_git-hf"))
        .args(args)
        .output()
        .expect("run git-hf");
    let json = serde_json::from_slice(&out.stdout).expect("stdout is JSON");
    (out.status.code().expect("exit code"), json)
}

#[test]
fn stub_returns_versioned_envelope() {
    let (code, json) = git_hf(&["trace", "log", "--json"]);
    assert_eq!(code, 64);
    assert_eq!(json["schema"], "ghf/v1");
    assert_eq!(json["command"], "trace log");
    assert_eq!(json["ok"], false);
    assert_eq!(json["error"]["code"], "E_NOT_IMPLEMENTED");
}

#[test]
fn usage_error_is_json_when_requested() {
    let (code, json) = git_hf(&["trace", "finalize", "--json"]);
    assert_eq!(code, 2);
    assert_eq!(json["error"]["code"], "E_USAGE");
}
