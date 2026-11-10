//! `dowai runs list` / `dowai runs show` against an isolated state directory.

use dowai_core::{NewInvocation, NewRun, RunStore};
use serde_json::Value;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

const RUN_A: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
const RUN_B: &str = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
const RUN_MISSING: &str = "cccccccc-cccc-4ccc-8ccc-cccccccccccc";

struct State(PathBuf);

impl State {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("dowai-runs-cli-{}-{nonce}", std::process::id()));
        fs::create_dir_all(&root).expect("create state root");
        Self(root)
    }

    fn store(&self) -> RunStore {
        let relative = if cfg!(windows) {
            PathBuf::from("DOWAI").join("state").join("runs.sqlite3")
        } else {
            PathBuf::from("dowai").join("runs.sqlite3")
        };
        RunStore::new(self.0.join(relative))
    }

    fn seed(&self) {
        let store = self.store();
        for (id, started, status, code) in [
            (RUN_A, 1_700_000_000_000, "SUCCEEDED", None),
            (
                RUN_B,
                1_700_000_100_000,
                "FAILED",
                Some("DOWAI_PYTHON_FAILED"),
            ),
        ] {
            store
                .record(
                    &NewRun {
                        run_id: id.to_owned(),
                        session_id: None,
                        project: "fixture".to_owned(),
                        status: status.to_owned(),
                        execution_mode: Some("automatic".to_owned()),
                        started_at_ms: started,
                        finished_at_ms: started + 40,
                        duration_ms: 40,
                        error_code: code.map(str::to_owned),
                    },
                    &[NewInvocation {
                        invocation_id: format!("inv-{id}"),
                        parent_invocation_id: None,
                        kind: "python".to_owned(),
                        target: "model.py".to_owned(),
                        call_type: "static".to_owned(),
                        status: status.to_owned(),
                        error_code: code.map(str::to_owned),
                        duration_ms: 12,
                    }],
                )
                .expect("seed journal");
        }
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
}

impl Drop for State {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn json_stdout(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).expect("stdout must be only JSON")
}

#[test]
fn list_json_is_newest_first_with_schema_version() {
    let state = State::new();
    state.seed();
    let output = state.cli(&["runs", "list", "--json"]);
    assert!(output.status.success());
    let value = json_stdout(&output);
    assert_eq!(value["schema_version"], 2);
    let ids: Vec<_> = value["runs"]
        .as_array()
        .expect("runs array")
        .iter()
        .map(|run| run["run_id"].as_str().expect("id"))
        .collect();
    assert_eq!(ids, [RUN_B, RUN_A]);
    assert_eq!(value["runs"][0]["error_code"], "DOWAI_PYTHON_FAILED");
    assert_eq!(value["runs"][0]["started_at"], "2023-11-14T22:15:00.000Z");
}

#[test]
fn list_honours_limit_and_human_output_is_not_json() {
    let state = State::new();
    state.seed();
    let limited = json_stdout(&state.cli(&["runs", "list", "--limit", "1", "--json"]));
    assert_eq!(limited["runs"].as_array().expect("runs").len(), 1);

    let human = state.cli(&["runs", "list"]);
    assert!(human.status.success());
    let text = String::from_utf8_lossy(&human.stdout);
    assert!(text.contains(RUN_B) && text.contains("FAILED"));
    assert!(serde_json::from_str::<Value>(&text).is_err());
}

#[test]
fn list_on_empty_state_succeeds_with_no_runs() {
    let state = State::new();
    let value = json_stdout(&state.cli(&["runs", "list", "--json"]));
    assert_eq!(value["runs"].as_array().expect("runs").len(), 0);
}

#[test]
fn invalid_limits_are_validation_errors() {
    let state = State::new();
    state.seed();
    for limit in ["0", "1001"] {
        let output = state.cli(&["runs", "list", "--limit", limit, "--json"]);
        assert_eq!(output.status.code(), Some(2), "limit {limit}");
        let value = json_stdout(&output);
        assert_eq!(value["error_code"], "DOWAI_CONFIG_INVALID");
        assert_eq!(value["status"], "FAILED");
    }
    assert_eq!(
        state.cli(&["runs", "list", "--limit", "abc"]).status.code(),
        Some(2)
    );
    assert_eq!(
        state
            .cli(&["runs", "list", "--limit", "1000", "--json"])
            .status
            .code(),
        Some(0)
    );
}

#[test]
fn show_json_includes_invocations() {
    let state = State::new();
    state.seed();
    let output = state.cli(&["runs", "show", RUN_B, "--json"]);
    assert!(output.status.success());
    let value = json_stdout(&output);
    assert_eq!(value["schema_version"], 2);
    assert_eq!(value["run"]["run_id"], RUN_B);
    assert_eq!(value["run"]["status"], "FAILED");
    assert_eq!(value["run"]["invocations"][0]["target"], "model.py");
    assert_eq!(value["run"]["invocations"][0]["kind"], "python");

    let human = state.cli(&["runs", "show", RUN_A]);
    assert!(human.status.success());
    assert!(String::from_utf8_lossy(&human.stdout).contains("model.py"));
}

#[test]
fn show_unknown_run_fails_with_state_error() {
    let state = State::new();
    state.seed();
    let output = state.cli(&["runs", "show", RUN_MISSING, "--json"]);
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(json_stdout(&output)["error_code"], "DOWAI_STATE_ERROR");
    let human = state.cli(&["runs", "show", RUN_MISSING]);
    assert_eq!(human.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&human.stderr).contains("DOWAI_STATE_ERROR"));
}

#[test]
fn show_malformed_run_id_is_a_validation_error() {
    let state = State::new();
    let output = state.cli(&["runs", "show", "not-a-uuid", "--json"]);
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(json_stdout(&output)["error_code"], "DOWAI_CONFIG_INVALID");
}
