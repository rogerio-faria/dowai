//! `dowai logs <run_id> [--node] [--level] [--json]` against an isolated state directory.

use dowai_core::{NewInvocation, NewRun, RunStore};
use serde_json::Value;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

const RUN: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
const MISSING: &str = "cccccccc-cccc-4ccc-8ccc-cccccccccccc";

struct State(PathBuf);

impl State {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("dowai-logs-cli-{}-{nonce}", std::process::id()));
        fs::create_dir_all(&root).expect("create state root");
        let relative = if cfg!(windows) {
            PathBuf::from("DOWAI").join("state").join("runs.sqlite3")
        } else {
            PathBuf::from("dowai").join("runs.sqlite3")
        };
        let invocation =
            |id: &str, parent: Option<&str>, target: &str, status: &str, code| NewInvocation {
                invocation_id: id.to_owned(),
                parent_invocation_id: parent.map(str::to_owned),
                kind: "sql".to_owned(),
                target: target.to_owned(),
                call_type: if parent.is_some() {
                    "imperative"
                } else {
                    "static"
                }
                .to_owned(),
                status: status.to_owned(),
                error_code: code,
                duration_ms: 5,
            };
        RunStore::new(root.join(relative))
            .record(
                &NewRun {
                    run_id: RUN.to_owned(),
                    session_id: None,
                    project: "fixture".to_owned(),
                    status: "FAILED".to_owned(),
                    execution_mode: Some("automatic".to_owned()),
                    started_at_ms: 1_700_000_000_000,
                    finished_at_ms: 1_700_000_000_040,
                    duration_ms: 40,
                    error_code: Some("DOWAI_SQL_FAILED".to_owned()),
                    requested_targets: Some(vec!["root.sql".to_owned()]),
                    profile: None,
                    variable_count: Some(0),
                },
                &[
                    invocation("i1", None, "root.sql", "SUCCEEDED", None),
                    invocation("i2", Some("i1"), "skipped.sql", "SKIPPED", None),
                    invocation(
                        "i3",
                        Some("i1"),
                        "broken.sql",
                        "FAILED",
                        Some("DOWAI_SQL_FAILED".to_owned()),
                    ),
                ],
            )
            .expect("seed journal");
        Self(root)
    }

    fn cli(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_dowai"))
            .args(args)
            .current_dir(&self.0)
            .env("XDG_STATE_HOME", &self.0)
            .env("LOCALAPPDATA", &self.0)
            .output()
            .expect("run DOWAI CLI")
    }

    fn json(&self, args: &[&str]) -> (Option<i32>, Value) {
        let output = self.cli(args);
        let value = serde_json::from_slice(&output.stdout).expect("stdout must be only JSON");
        (output.status.code(), value)
    }
}

impl Drop for State {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn targets(value: &Value) -> Vec<&str> {
    value["invocations"]
        .as_array()
        .expect("invocations")
        .iter()
        .map(|entry| entry["target"].as_str().expect("target"))
        .collect()
}

#[test]
fn json_lists_the_tree_with_schema_version_and_no_free_text() {
    let state = State::new();
    let (code, value) = state.json(&["logs", RUN, "--json"]);
    assert_eq!(code, Some(0));
    assert_eq!(value["schema_version"], 2);
    assert_eq!(value["run_id"], RUN);
    assert_eq!(value["level"], "INFO");
    assert_eq!(targets(&value), ["root.sql", "skipped.sql", "broken.sql"]);
    let last = &value["invocations"][2];
    assert_eq!(last["status"], "FAILED");
    assert_eq!(last["error_code"], "DOWAI_SQL_FAILED");
    assert_eq!(last["call_type"], "imperative");
    assert_eq!(last["parent_invocation_id"], "i1");
    assert_eq!(last["duration_ms"], 5);
    assert_eq!(last["depth"], 1);
    assert!(value.get("variables").is_none() && last.get("message").is_none());
}

#[test]
fn level_maps_error_to_failed_warn_to_skipped_and_failed() {
    let state = State::new();
    let (_, error) = state.json(&["logs", RUN, "--level", "ERROR", "--json"]);
    assert_eq!(targets(&error), ["broken.sql"]);
    let (_, warn) = state.json(&["logs", RUN, "--level", "warn", "--json"]);
    assert_eq!(targets(&warn), ["skipped.sql", "broken.sql"]);
    for level in ["INFO", "DEBUG", "TRACE"] {
        let (_, all) = state.json(&["logs", RUN, "--level", level, "--json"]);
        assert_eq!(targets(&all).len(), 3, "{level}");
        assert_eq!(all["level"], level);
    }
}

#[test]
fn node_filter_selects_by_target_and_can_combine_with_level() {
    let state = State::new();
    let (_, node) = state.json(&["logs", RUN, "--node", "skipped.sql", "--json"]);
    assert_eq!(targets(&node), ["skipped.sql"]);
    assert_eq!(node["node"], "skipped.sql");
    let (code, none) = state.json(&[
        "logs", RUN, "--node", "root.sql", "--level", "ERROR", "--json",
    ]);
    assert_eq!(code, Some(0));
    assert!(targets(&none).is_empty());
}

#[test]
fn human_output_is_an_indented_tree_not_json() {
    let state = State::new();
    let output = state.cli(&["logs", RUN]);
    assert!(output.status.success());
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains(RUN));
    assert!(text.contains("\nSUCCEEDED sql static root.sql"), "{text}");
    assert!(
        text.contains("\n  FAILED sql imperative broken.sql"),
        "{text}"
    );
    assert!(text.contains("DOWAI_SQL_FAILED"));
    assert!(serde_json::from_str::<Value>(&text).is_err());
}

#[test]
fn unknown_run_is_a_state_error_and_malformed_id_a_validation_error() {
    let state = State::new();
    let (code, missing) = state.json(&["logs", MISSING, "--json"]);
    assert_eq!(code, Some(2));
    assert_eq!(missing["error_code"], "DOWAI_STATE_ERROR");
    let (code, malformed) = state.json(&["logs", "not-a-uuid", "--json"]);
    assert_eq!(code, Some(2));
    assert_eq!(malformed["error_code"], "DOWAI_CONFIG_INVALID");
}

#[test]
fn invalid_level_is_a_validation_error() {
    let state = State::new();
    let (code, value) = state.json(&["logs", RUN, "--level", "FATAL", "--json"]);
    assert_eq!(code, Some(2));
    assert_eq!(value["error_code"], "DOWAI_CONFIG_INVALID");
    assert_eq!(value["status"], "FAILED");
}
