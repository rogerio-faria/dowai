//! ENT-035: `dowai compatibility check <project_path> [--json]` runs through the real binary,
//! prints a JSON report that validates against `specs/compatibility.schema.json`, reports
//! unsupported macros with location, and exits 0 because the report is a diagnostic.

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
            std::env::temp_dir().join(format!("dowai-compat-cli-{}-{nonce}", std::process::id()));
        fs::create_dir_all(&root).expect("create sandbox");
        Self(root)
    }

    fn cli(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_dowai"))
            .args(args)
            .current_dir(&self.0)
            .env("XDG_STATE_HOME", self.0.join("state"))
            .env("XDG_CONFIG_HOME", self.0.join("config"))
            .env("LOCALAPPDATA", self.0.join("local"))
            .env("APPDATA", self.0.join("appdata"))
            .env("PROGRAMDATA", self.0.join("programdata"))
            .output()
            .expect("run DOWAI CLI")
    }

    fn project(&self) -> PathBuf {
        let workspace = self.0.join("workspace");
        fs::create_dir_all(&workspace).expect("create workspace");
        let init = Command::new(env!("CARGO_BIN_EXE_dowai"))
            .args([
                "init",
                "demo",
                "--workspace",
                workspace.to_str().expect("utf-8 path"),
                "--json",
            ])
            .env("XDG_STATE_HOME", self.0.join("state"))
            .env("XDG_CONFIG_HOME", self.0.join("config"))
            .output()
            .expect("run init");
        assert_eq!(init.status.code(), Some(0), "init must succeed");
        workspace.join("projects").join("demo")
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn write(root: &Path, relative: &str, content: &str) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().expect("parent")).expect("create parent");
    fs::write(path, content).expect("write file");
}

#[test]
fn compatibility_check_reports_unsupported_macro_and_validates_schema() {
    let sandbox = Sandbox::new();
    let project = sandbox.project();
    write(
        &project,
        "models/stage.sql",
        "select 1 as id\nfrom {{ ref('base') }}\nwhere {{ dbt_utils.not_null() }}\n",
    );
    let project_arg = project.to_str().expect("utf-8 path");

    let output = sandbox.cli(&["compatibility", "check", project_arg, "--json"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let report: Value = serde_json::from_slice(&output.stdout).expect("stdout is JSON only");
    assert_eq!(report["status"], "UNSUPPORTED");
    assert_eq!(report["models_checked"], 1);
    let finding = &report["findings"][0];
    assert_eq!(finding["code"], "DOWAI_COMPAT_MACRO_UNSUPPORTED");
    assert_eq!(finding["path"], "models/stage.sql");
    assert_eq!(finding["line"], 3);

    let schema: Value = serde_json::from_str(
        &fs::read_to_string(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../specs/compatibility.schema.json"),
        )
        .expect("read schema"),
    )
    .expect("schema json");
    let validator = jsonschema::validator_for(&schema).expect("schema compiles");
    let errors: Vec<String> = validator
        .iter_errors(&report)
        .map(|error| error.to_string())
        .collect();
    assert!(errors.is_empty(), "schema errors: {errors:?}");
}

#[test]
fn compatibility_check_human_output_lists_location() {
    let sandbox = Sandbox::new();
    let project = sandbox.project();
    write(&project, "models/ok.sql", "select 1\n");
    let project_arg = project.to_str().expect("utf-8 path");

    let output = sandbox.cli(&["compatibility", "check", project_arg]);
    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Compatibility: SUPPORTED"), "{stdout}");
}

#[test]
fn compatibility_check_on_non_project_fails_with_error() {
    let sandbox = Sandbox::new();
    let empty = sandbox.0.join("empty");
    fs::create_dir_all(&empty).expect("create dir");

    let output = sandbox.cli(&[
        "compatibility",
        "check",
        empty.to_str().expect("utf-8 path"),
        "--json",
    ]);
    assert_ne!(output.status.code(), Some(0));
}
