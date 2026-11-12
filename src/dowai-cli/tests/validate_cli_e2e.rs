//! ENT-018: `dowai validate [PATH] [--strict] [--json]` reports errors and warnings with a stable
//! severity, locates diagnostics by project-relative path, converts warnings into failures under
//! `--strict` (exit 2), and produces byte-identical reports across runs. Exercised through the
//! `dowai` binary with isolated state and configuration.

use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

struct Sandbox(PathBuf);

impl Sandbox {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("dowai-validate-{}-{nonce}", std::process::id()));
        fs::create_dir_all(&root).expect("create sandbox");
        Self(root)
    }

    /// Runs the CLI in `dir` with every user-level location inside the sandbox.
    fn cli_in(&self, dir: &Path, args: &[&str]) -> Output {
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

    /// Creates a project (with an empty `models/` directory) and returns its directory.
    fn project(&self) -> PathBuf {
        let workspace = self.0.join("workspace");
        fs::create_dir_all(&workspace).expect("create workspace");
        let init = self.cli_in(
            &workspace,
            &[
                "init",
                "demo",
                "--workspace",
                workspace.to_str().expect("utf-8 path"),
                "--json",
            ],
        );
        assert_eq!(init.status.code(), Some(0), "init must succeed");
        workspace.join("projects").join("demo")
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

/// Success payloads carry `valid` at the top level; failures nest the report in `details`.
fn report(output: &Output) -> Value {
    let value = json_stdout(output);
    if value.get("details").is_some_and(Value::is_object) {
        value["details"].clone()
    } else {
        value
    }
}

fn find<'a>(report: &'a Value, code: &str) -> Option<&'a Value> {
    report["diagnostics"]
        .as_array()?
        .iter()
        .find(|item| item["code"] == code)
}

#[test]
fn empty_models_directory_is_a_warning_that_keeps_the_project_valid() {
    let sandbox = Sandbox::new();
    let project = sandbox.project();

    let output = sandbox.cli_in(&project, &["validate", "--json"]);
    assert_eq!(output.status.code(), Some(0));
    let value = json_stdout(&output);
    assert_eq!(value["valid"], true);
    let warning = find(&value, "DOWAI_MODELS_EMPTY").expect("empty models warning");
    assert_eq!(warning["severity"], "warning");
    assert_eq!(warning["path"], "models");
}

#[test]
fn strict_mode_turns_the_warning_into_an_exit_2_failure() {
    let sandbox = Sandbox::new();
    let project = sandbox.project();

    let output = sandbox.cli_in(&project, &["validate", "--strict", "--json"]);
    assert_eq!(output.status.code(), Some(2));
    let value = report(&output);
    assert_eq!(value["valid"], false);
    let diagnostic = find(&value, "DOWAI_MODELS_EMPTY").expect("diagnostic kept");
    assert_eq!(diagnostic["severity"], "error");
}

#[test]
fn clean_project_passes_in_normal_and_strict_modes_without_diagnostics() {
    let sandbox = Sandbox::new();
    let project = sandbox.project();
    fs::write(project.join("models").join("orders.sql"), "select 1").expect("write model");

    for args in [
        &["validate", "--json"][..],
        &["validate", "--strict", "--json"][..],
    ] {
        let output = sandbox.cli_in(&project, args);
        assert_eq!(output.status.code(), Some(0), "args: {args:?}");
        let value = json_stdout(&output);
        assert_eq!(value["valid"], true);
        assert_eq!(value["diagnostics"], serde_json::json!([]));
    }
}

#[test]
fn missing_project_file_is_an_error_located_at_the_manifest() {
    let sandbox = Sandbox::new();
    let project = sandbox.project();
    fs::remove_file(project.join("dowai_project.json")).expect("remove manifest");

    let output = sandbox.cli_in(&project, &["validate", "--json"]);
    assert_eq!(output.status.code(), Some(2));
    let value = report(&output);
    assert_eq!(value["valid"], false);
    let diagnostic = find(&value, "DOWAI_CONFIG_INVALID").expect("manifest error");
    assert_eq!(diagnostic["severity"], "error");
    assert_eq!(diagnostic["path"], "dowai_project.json");
}

#[test]
fn reports_are_byte_identical_across_runs() {
    let sandbox = Sandbox::new();
    let project = sandbox.project();

    for args in [
        &["validate", "--json"][..],
        &["validate", "--strict", "--json"][..],
    ] {
        let first = sandbox.cli_in(&project, args);
        let second = sandbox.cli_in(&project, args);
        assert_eq!(first.stdout, second.stdout, "args: {args:?}");
        assert_eq!(first.status.code(), second.status.code());
    }
}

#[test]
fn human_output_lists_warnings_on_stderr_and_keeps_the_success_exit() {
    let sandbox = Sandbox::new();
    let project = sandbox.project();

    let output = sandbox.cli_in(&project, &["validate"]);
    assert_eq!(output.status.code(), Some(0));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Warning [DOWAI_MODELS_EMPTY] (models)"),
        "{stderr}"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("valid with 1 warning(s)"), "{stdout}");
}
