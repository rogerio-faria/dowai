//! ENT-016: `dowai doctor` reports every problem as a diagnostic with a stable code, never
//! connects without a declared profile, and never echoes connection details. Exercised through
//! the `dowai` binary with isolated state and configuration.

use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static NEXT_SANDBOX: AtomicU64 = AtomicU64::new(0);

struct Sandbox(PathBuf);

impl Sandbox {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let id = NEXT_SANDBOX.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("dowai-doctor-{}-{nonce}-{id}", std::process::id()));
        fs::create_dir(&root).expect("create sandbox");
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

    /// Creates a project inside the sandbox and returns its directory.
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

    /// Declares a Python interpreter in `dowai_project.json`.
    fn declare_interpreter(&self, project: &Path, interpreter: &Path) {
        let path = project.join("dowai_project.json");
        let mut value: Value =
            serde_json::from_slice(&fs::read(&path).expect("read project")).expect("json");
        value["python"] = serde_json::json!({ "interpreter": interpreter });
        fs::write(&path, serde_json::to_vec_pretty(&value).expect("serialize")).expect("write");
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

/// The success payload carries `checks` at the top level; the failure envelope nests it in `details`.
fn health_report(output: &Output) -> Value {
    let value = json_stdout(output);
    if value.get("details").is_some_and(Value::is_object) {
        value["details"].clone()
    } else {
        value
    }
}

fn has_diagnostic(report: &Value, code: &str) -> bool {
    report["diagnostics"]
        .as_array()
        .is_some_and(|items| items.iter().any(|item| item["code"] == code))
}

#[test]
fn project_without_python_is_healthy_and_does_not_connect() {
    let sandbox = Sandbox::new();
    let project = sandbox.project();

    let output = sandbox.cli_in(&project, &["doctor", "--json"]);
    assert_eq!(output.status.code(), Some(0));
    let report = json_stdout(&output);
    assert_eq!(report["status"], "HEALTHY");
    assert_eq!(report["checks"]["python_runtime"], "not_configured");
    assert_eq!(report["checks"]["sql_database"], "not_checked");
    assert_eq!(report["diagnostics"], serde_json::json!([]));
}

#[test]
fn check_connections_without_declared_profile_reports_not_declared() {
    let sandbox = Sandbox::new();
    let project = sandbox.project();

    let output = sandbox.cli_in(&project, &["doctor", "--check-connections", "--json"]);
    assert_eq!(output.status.code(), Some(0));
    let report = json_stdout(&output);
    assert_eq!(report["status"], "HEALTHY");
    assert_eq!(report["checks"]["sql_database"], "not_declared");
}

#[test]
fn interpreter_outside_project_is_a_coded_diagnostic() {
    let sandbox = Sandbox::new();
    let project = sandbox.project();
    // Caminho absoluto existente e fora do projeto em qualquer plataforma
    // (`/usr/bin/python3` não é absoluto no Windows e invalidaria o projeto antes da checagem).
    let outside = std::env::current_exe().expect("current exe");
    sandbox.declare_interpreter(&project, &outside);

    let output = sandbox.cli_in(&project, &["doctor", "--json"]);
    assert_eq!(output.status.code(), Some(2));
    let report = health_report(&output);
    assert_eq!(report["status"], "DEGRADED");
    assert_eq!(report["checks"]["python_runtime"], "invalid");
    assert!(has_diagnostic(&report, "DOWAI_POLICY_PATH"));
}

#[cfg(unix)]
fn write_interpreter(project: &Path, name: &str, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = project.join(name);
    fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("write interpreter");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod");
    path
}

#[cfg(unix)]
#[test]
fn sdk_importable_from_project_interpreter_is_healthy() {
    let sandbox = Sandbox::new();
    let project = sandbox.project();
    let interpreter = write_interpreter(&project, "python-ok", "exit 0");
    sandbox.declare_interpreter(&project, &interpreter);

    let output = sandbox.cli_in(&project, &["doctor", "--json"]);
    assert_eq!(output.status.code(), Some(0));
    let report = json_stdout(&output);
    assert_eq!(report["status"], "HEALTHY");
    assert_eq!(report["checks"]["python_runtime"], "configured");
    assert_eq!(report["checks"]["python_sdk"], "available");
}

#[cfg(unix)]
#[test]
fn sdk_missing_from_project_interpreter_is_a_coded_diagnostic() {
    let sandbox = Sandbox::new();
    let project = sandbox.project();
    let interpreter = write_interpreter(&project, "python-no-sdk", "exit 1");
    sandbox.declare_interpreter(&project, &interpreter);

    let output = sandbox.cli_in(&project, &["doctor", "--json"]);
    assert_eq!(output.status.code(), Some(2));
    let report = health_report(&output);
    assert_eq!(report["checks"]["python_runtime"], "configured");
    assert_eq!(report["checks"]["python_sdk"], "unavailable");
    assert!(has_diagnostic(&report, "DOWAI_PYTHON_FAILED"));
}
