//! Contrato de exit codes (plano §6.4) exercitado pelo binário `dowai`, comando a comando.
//! Em modo `--json`, stdout deve conter somente o payload JSON.

use serde_json::Value;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

const RUN_MISSING: &str = "cccccccc-cccc-4ccc-8ccc-cccccccccccc";

struct Sandbox(PathBuf);

impl Sandbox {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("dowai-exit-cli-{}-{nonce}", std::process::id()));
        fs::create_dir_all(&root).expect("create sandbox");
        Self(root)
    }

    /// Executa o CLI sem projeto DOWAI no diretório atual e com estado isolado.
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
fn success_path_returns_zero_with_json_only_on_stdout() {
    let sandbox = Sandbox::new();
    let output = sandbox.cli(&["runs", "list", "--json"]);
    assert_eq!(output.status.code(), Some(0));
    let value = json_stdout(&output);
    assert_eq!(value["schema_version"], 2);
    assert!(output.stderr.is_empty(), "stderr must be empty on success");
}

#[test]
fn invalid_input_commands_return_two_in_json_mode() {
    let sandbox = Sandbox::new();
    let cases: &[&[&str]] = &[
        &["run", "model.sql", "--json"],
        &["doctor", "--json"],
        &["validate", "--json"],
        &["runs", "show", "not-a-uuid", "--json"],
        &["logs", "not-a-uuid", "--json"],
        &["session", "show", "abc", "--json"],
        &[
            "config",
            "migrate",
            "missing.yaml",
            "--output",
            "out.json",
            "--json",
        ],
    ];
    for args in cases {
        let output = sandbox.cli(args);
        assert_eq!(output.status.code(), Some(2), "args {args:?}");
        let value = json_stdout(&output);
        assert_eq!(value["status"], "FAILED", "args {args:?}");
        assert!(value["error_code"].is_string(), "args {args:?}");
    }
}

#[test]
fn unknown_state_entities_return_two_with_stable_code() {
    let sandbox = Sandbox::new();
    let output = sandbox.cli(&["runs", "show", RUN_MISSING, "--json"]);
    assert_eq!(output.status.code(), Some(2));
    let value = json_stdout(&output);
    assert_eq!(value["error_code"], "DOWAI_STATE_ERROR");
}

#[test]
fn full_refresh_without_yes_is_refused_with_five() {
    let sandbox = Sandbox::new();
    let output = sandbox.cli(&["run", "--full-refresh", "model.sql"]);
    assert_eq!(output.status.code(), Some(5));
    assert!(
        output.stdout.is_empty(),
        "human mode writes no stdout on error"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("DOWAI_POLICY_DENIED"), "{stderr}");
    assert!(stderr.contains("--yes"), "{stderr}");
}

#[test]
fn full_refresh_without_yes_in_json_mode_is_refused_with_five() {
    let sandbox = Sandbox::new();
    let output = sandbox.cli(&["run", "--full-refresh", "model.sql", "--json"]);
    assert_eq!(output.status.code(), Some(5));
    let value = json_stdout(&output);
    assert_eq!(value["error_code"], "DOWAI_POLICY_DENIED");
}

#[test]
fn usage_errors_from_clap_return_two() {
    let sandbox = Sandbox::new();
    let output = sandbox.cli(&["comando-inexistente"]);
    assert_eq!(output.status.code(), Some(2));
}
