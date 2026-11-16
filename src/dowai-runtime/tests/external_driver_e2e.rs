//! ENT-128: `env.write` to an `external` profile runs through a Python driver from `drivers/`.
//! The driver here is a fake that records batches in a file; no database is contacted. Needs a
//! local `python3` able to create a venv.

use dowai_core::{create_project, ExecutionMode};
use dowai_runtime::{run_postgres, RunRequest, RunResult};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

struct EnvironmentRestore {
    name: &'static str,
    previous: Option<OsString>,
}

impl EnvironmentRestore {
    fn set(name: &'static str, value: &OsString) -> Self {
        let previous = std::env::var_os(name);
        std::env::set_var(name, value);
        Self { name, previous }
    }
}

impl Drop for EnvironmentRestore {
    fn drop(&mut self) {
        match self.previous.take() {
            Some(value) => std::env::set_var(self.name, value),
            None => std::env::remove_var(self.name),
        }
    }
}

const FAKE_DRIVER: &str = r#"
import pathlib

class Rejected(Exception):
    def __init__(self, code, message):
        super().__init__(message)
        self.code = code

_state = {"params": {}, "batches": [], "table": None}

def open(params, table, columns, mode, keys, confirm_replace):
    _state["params"] = params
    _state["table"] = table
    _state["columns"] = columns
    _state["mode"] = mode

def write_batch(rows):
    if _state["params"].get("reject"):
        raise Rejected("DOWAI_FAKE_REJECTED", "the fake destination rejects rows")
    _state["batches"].append(rows)
    return len(rows)

def commit():
    total = sum(len(batch) for batch in _state["batches"])
    out = pathlib.Path(_state["params"]["out"])
    lines = [repr(row) for batch in _state["batches"] for row in batch]
    out.write_text("\n".join(lines) + "\n", encoding="utf-8")
    return {"table": _state["table"], "rows": total}

def abort():
    _state["batches"] = []
"#;

const INCOMPLETE_DRIVER: &str = r#"
def open(params, table, columns, mode, keys, confirm_replace):
    pass

def write_batch(rows):
    return len(rows)

def abort():
    pass
"#;

struct Fixture {
    root: PathBuf,
    project: PathBuf,
    _env: Vec<EnvironmentRestore>,
}

impl Fixture {
    fn new(profile: Value) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock is after Unix epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "dowai-external-driver-{}-{nonce}",
            std::process::id()
        ));
        let state = root.join("state");
        for dir in ["local", "roaming", "system"] {
            fs::create_dir_all(state.join(dir)).expect("create state dir");
        }
        let env = vec![
            EnvironmentRestore::set("LOCALAPPDATA", &state.join("local").into_os_string()),
            EnvironmentRestore::set("APPDATA", &state.join("roaming").into_os_string()),
            EnvironmentRestore::set("PROGRAMDATA", &state.join("system").into_os_string()),
            EnvironmentRestore::set("XDG_CONFIG_HOME", &state.join("roaming").into_os_string()),
            EnvironmentRestore::set("XDG_STATE_HOME", &state.clone().into_os_string()),
        ];
        let project = create_project(&root, "fixture").expect("initialize project");
        let venv = project.join(".venv");
        let created = Command::new(if cfg!(windows) { "python" } else { "python3" })
            .args(["-m", "venv", "--without-pip"])
            .arg(&venv)
            .output()
            .expect("create project virtual environment");
        assert!(created.status.success(), "venv creation failed");
        let interpreter = if cfg!(windows) {
            venv.join("Scripts").join("python.exe")
        } else {
            venv.join("bin").join("python")
        };
        let manifest_path = project.join("dowai_project.json");
        let mut manifest: Value =
            serde_json::from_slice(&fs::read(&manifest_path).expect("read manifest"))
                .expect("manifest is JSON");
        manifest["python"] = json!({ "interpreter": interpreter, "timeout_seconds": 30 });
        manifest["config"]["security"]["allow_external_writes"] = json!(true);
        fs::write(
            &manifest_path,
            serde_json::to_vec(&manifest).expect("serialize"),
        )
        .expect("write manifest");
        let workspace = json!({
            "schema_version": 2,
            "profiles": { "fake": profile },
        });
        fs::write(
            root.join("workspace.json"),
            serde_json::to_vec(&workspace).expect("serialize"),
        )
        .expect("write workspace");
        fs::create_dir_all(project.join("drivers")).expect("create drivers directory");
        fs::write(project.join("drivers").join("fake_driver.py"), FAKE_DRIVER).expect("driver");
        fs::write(
            project.join("drivers").join("incomplete.py"),
            INCOMPLETE_DRIVER,
        )
        .expect("incomplete driver");
        Self {
            root,
            project,
            _env: env,
        }
    }

    fn write_model(&self, file: &str, body: &str) {
        fs::write(self.project.join("models").join(file), body).expect("write model");
    }

    fn run(&self, target: &str) -> Result<RunResult, dowai_core::DowaiError> {
        run_postgres(
            &self.project,
            RunRequest {
                targets: vec![target.to_owned()],
                mode: Some(ExecutionMode::Automatic),
                profile: None,
                session_id: None,
                variables: BTreeMap::new(),
                confirm_destructive: false,
                full_refresh: false,
            },
        )
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn fake_profile(driver: &str, params: Value) -> Value {
    json!({ "type": "external", "driver": driver, "params": params, "env": [] })
}

const WRITE_TWO_ROWS: &str = "def model(env, origem=None):\n    env.write([{'v': 1}, {'v': 2}], target='fake', table='t', mode='append')\n    env.setVar('written', True)\n";

#[test]
fn external_driver_receives_the_batches_and_commits() {
    let fixture = Fixture::new(fake_profile(
        "fake_driver.py",
        json!({ "out": "captured.txt" }),
    ));
    fixture.write_model("grava.py", WRITE_TWO_ROWS);

    let result = fixture
        .run("grava.py")
        .expect("driver write should succeed");

    assert_eq!(result.status, "SUCCEEDED");
    let captured = fs::read_to_string(fixture.project.join("captured.txt"))
        .expect("the driver committed its file");
    assert!(
        captured.contains("{'v': 1}") && captured.contains("{'v': 2}"),
        "{captured}"
    );
}

#[test]
fn a_driver_error_keeps_its_code_and_message_and_nothing_is_committed() {
    let fixture = Fixture::new(fake_profile(
        "fake_driver.py",
        json!({ "out": "never.txt", "reject": true }),
    ));
    fixture.write_model("grava.py", WRITE_TWO_ROWS);

    let error = fixture
        .run("grava.py")
        .expect_err("a rejected batch must fail the run");

    assert_eq!(error.code(), "DOWAI_WRITE_DRIVER_FAILED");
    assert!(
        error.message().contains("DOWAI_FAKE_REJECTED"),
        "{}",
        error.message()
    );
    assert!(
        !fixture.project.join("never.txt").exists(),
        "no commit after a failed batch"
    );
}

#[test]
fn a_driver_without_the_full_interface_is_refused() {
    let fixture = Fixture::new(fake_profile("incomplete.py", json!({})));
    fixture.write_model("grava.py", WRITE_TWO_ROWS);

    let error = fixture
        .run("grava.py")
        .expect_err("an incomplete driver must be refused");

    assert_eq!(error.code(), "DOWAI_WRITE_DRIVER_INVALID");
}

#[test]
fn a_missing_driver_file_is_refused_before_any_row() {
    let fixture = Fixture::new(fake_profile("absent.py", json!({})));
    fixture.write_model("grava.py", WRITE_TWO_ROWS);

    let error = fixture
        .run("grava.py")
        .expect_err("a missing driver must be refused");

    assert_eq!(error.code(), "DOWAI_WRITE_DRIVER_INVALID");
}
