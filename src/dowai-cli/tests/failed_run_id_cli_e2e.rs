//! ENT-015: a failed run that started reports its `run_id` in the `--json` envelope, and that
//! identifier names the FAILED run in the journal. A refused retry starts no run and reports
//! no `run_id`. Exercised through the `dowai` binary with isolated state and configuration.

use serde_json::Value;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

const UNKNOWN_RUN: &str = "00000000-0000-4000-8000-000000000000";

struct Sandbox(PathBuf);

impl Sandbox {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root = std::env::temp_dir().join(format!("dowai-runid-{}-{nonce}", std::process::id()));
        fs::create_dir_all(&root).expect("create sandbox");
        Self(root)
    }

    /// Runs the CLI in `dir` with every user-level location inside the sandbox.
    fn cli_in(&self, dir: &PathBuf, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_dowai"))
            .args(args)
            .current_dir(dir)
            .env("XDG_STATE_HOME", self.0.join("state"))
            .env("XDG_CONFIG_HOME", self.0.join("config"))
            .env("LOCALAPPDATA", self.0.join("local"))
            .env("APPDATA", self.0.join("appdata"))
            .env("PROGRAMDATA", self.0.join("programdata"))
            .output()
            .expect("run DOWAI CLI")
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn json_stdout(output: &Output) -> Value {
    assert!(
        output.stderr.is_empty(),
        "JSON mode must not write diagnostics to stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("stdout must contain only one JSON payload")
}

#[test]
fn failed_run_envelope_carries_the_journaled_run_id() {
    let sandbox = Sandbox::new();
    let init = sandbox.cli_in(&sandbox.0, &["init", "demo", "--json"]);
    assert_eq!(init.status.code(), Some(0));
    let project = PathBuf::from(
        json_stdout(&init)["project_dir"]
            .as_str()
            .expect("project_dir"),
    );
    fs::write(project.join("models/bad.sql"), "select 1 as value\n").expect("write model");

    // External writes are disabled by default, so this run fails after it has started.
    let output = sandbox.cli_in(&project, &["run", "bad.sql", "--json"]);
    assert_eq!(output.status.code(), Some(5));
    let envelope = json_stdout(&output);
    assert_eq!(envelope["status"], "FAILED");
    assert_eq!(envelope["error_code"], "DOWAI_POLICY_DENIED");
    let run_id = envelope["run_id"]
        .as_str()
        .expect("a started run reports run_id");
    assert!(!run_id.is_empty());

    let shown = sandbox.cli_in(&project, &["runs", "show", run_id, "--json"]);
    assert_eq!(shown.status.code(), Some(0));
    let run = &json_stdout(&shown)["run"];
    assert_eq!(run["run_id"], run_id);
    assert_eq!(run["status"], "FAILED");
    assert_eq!(run["error_code"], "DOWAI_POLICY_DENIED");
}

#[test]
fn refused_retry_starts_no_run_and_reports_no_run_id() {
    let sandbox = Sandbox::new();
    // `retry` needs a project to read the journal; the refusal happens before any run starts.
    let init = sandbox.cli_in(&sandbox.0, &["init", "demo", "--json"]);
    assert_eq!(init.status.code(), Some(0));
    let project = PathBuf::from(
        json_stdout(&init)["project_dir"]
            .as_str()
            .expect("project_dir"),
    );
    let output = sandbox.cli_in(&project, &["retry", UNKNOWN_RUN, "--json"]);
    assert_eq!(output.status.code(), Some(2));
    let envelope = json_stdout(&output);
    assert_eq!(envelope["error_code"], "DOWAI_RUN_NOT_FOUND");
    assert!(
        envelope.get("run_id").is_none(),
        "refusal must not carry run_id: {envelope}"
    );
}
