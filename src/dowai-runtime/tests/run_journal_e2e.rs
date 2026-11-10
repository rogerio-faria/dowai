//! Persistent run journal (plan 5.5, 12.2, 12.3; decision D-011): every outcome of a
//! run is recorded without variable values, SQL, or free-form error messages.

use dowai_core::{create_project, default_run_store_path, RunDetail, RunStore};
use dowai_runtime::{run_postgres, RunRequest};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Mutex, MutexGuard};
use uuid::Uuid;

const SENTINEL_VARIABLE: &str = "SENTINEL_VAR_VALUE_5c1e";
const SENTINEL_ERROR: &str = "SENTINEL_PRIVATE_DETAIL_a91d";

static ENVIRONMENT_LOCK: Mutex<()> = Mutex::new(());

struct Harness {
    _lock: MutexGuard<'static, ()>,
    root: PathBuf,
    project: PathBuf,
    restore: Vec<(&'static str, Option<OsString>)>,
}

impl Harness {
    fn new() -> Self {
        let lock = ENVIRONMENT_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let root = std::env::temp_dir().join(format!("dowai-journal-e2e-{}", Uuid::new_v4()));
        fs::create_dir_all(root.join("config")).expect("create config dir");
        let mut restore = Vec::new();
        for (name, value) in [
            ("XDG_STATE_HOME", root.join("state")),
            ("XDG_CONFIG_HOME", root.join("config")),
            ("LOCALAPPDATA", root.join("local")),
            ("APPDATA", root.join("config")),
            ("PROGRAMDATA", root.join("system")),
        ] {
            restore.push((name, std::env::var_os(name)));
            std::env::set_var(name, value);
        }
        let project = create_project(&root, "journal-fixture").expect("create project");
        let environment = project.join(".venv");
        let created = Command::new("python3")
            .args(["-m", "venv", "--without-pip"])
            .arg(&environment)
            .output()
            .expect("create virtual environment");
        assert!(created.status.success(), "venv creation must succeed");
        let interpreter = environment.join("bin").join("python");
        let manifest_path = project.join("dowai_project.json");
        let mut manifest: Value =
            serde_json::from_slice(&fs::read(&manifest_path).expect("read manifest"))
                .expect("manifest JSON");
        manifest["python"] = json!({"interpreter": interpreter, "timeout_seconds": 30});
        manifest["config"]["security"]["allow_external_writes"] = json!(true);
        fs::write(
            &manifest_path,
            serde_json::to_vec(&manifest).expect("encode"),
        )
        .expect("write manifest");
        Self {
            _lock: lock,
            root,
            project,
            restore,
        }
    }

    fn model(&self, name: &str, body: &str) {
        fs::write(self.project.join("models").join(name), body).expect("write model");
    }

    fn set_timeout(&self, seconds: u64) {
        let path = self.project.join("dowai_project.json");
        let mut manifest: Value =
            serde_json::from_slice(&fs::read(&path).expect("read")).expect("manifest JSON");
        manifest["python"]["timeout_seconds"] = json!(seconds);
        fs::write(path, serde_json::to_vec(&manifest).expect("encode")).expect("write");
    }

    fn run(
        &self,
        target: &str,
        variables: BTreeMap<String, Value>,
    ) -> dowai_core::Result<dowai_runtime::RunResult> {
        run_postgres(
            &self.project,
            RunRequest {
                targets: vec![target.to_owned()],
                mode: None,
                profile: None,
                session_id: None,
                variables,
                confirm_destructive: false,
            },
        )
    }

    fn store(&self) -> RunStore {
        RunStore::for_user_state(default_run_store_path().expect("isolated path"))
    }

    fn database_text(&self) -> String {
        let path = default_run_store_path().expect("isolated path");
        let mut text = String::new();
        for suffix in ["", "-wal"] {
            let mut name = path.clone().into_os_string();
            name.push(suffix);
            if let Ok(bytes) = fs::read(PathBuf::from(name)) {
                text.push_str(&String::from_utf8_lossy(&bytes));
            }
        }
        text
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        for (name, previous) in self.restore.drain(..) {
            match previous {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn only_run(harness: &Harness) -> RunDetail {
    let runs = harness.store().list(10).expect("list runs");
    assert_eq!(runs.len(), 1, "exactly one journal entry expected");
    harness.store().get(&runs[0].run_id).expect("show run")
}

#[test]
fn successful_python_run_is_journaled_without_values() {
    let harness = Harness::new();
    harness.model(
        "upstream.py",
        &format!(
            "def model(env):\n    env.setVar('stage', '{SENTINEL_VARIABLE}')\n    return None\n"
        ),
    );
    harness.model(
        "consumer.py",
        "def model(env):\n    env.depends_on('upstream')\n    return None\n",
    );
    let result = harness
        .run(
            "consumer.py",
            BTreeMap::from([("input".to_owned(), json!(SENTINEL_VARIABLE))]),
        )
        .expect("run succeeds");

    let detail = only_run(&harness);
    assert_eq!(detail.run.run_id, result.run_id);
    assert_eq!(detail.run.status, "SUCCEEDED");
    assert_eq!(detail.run.execution_mode.as_deref(), Some("automatic"));
    assert_eq!(
        detail.run.session_id.as_deref(),
        Some(result.session_id.as_str())
    );
    assert_eq!(detail.run.project, "journal-fixture");
    assert_eq!(detail.run.error_code, None);
    assert!(detail.run.started_at.ends_with('Z'));
    let targets: Vec<_> = detail
        .invocations
        .iter()
        .map(|i| i.target.as_str())
        .collect();
    assert_eq!(targets, ["upstream.py", "consumer.py"]);
    assert!(detail
        .invocations
        .iter()
        .all(|i| i.kind == "python" && i.call_type == "static" && i.status == "SUCCEEDED"));
    assert!(!harness.database_text().contains(SENTINEL_VARIABLE));
}

#[test]
fn failed_python_node_is_journaled_even_though_run_returns_err() {
    let harness = Harness::new();
    harness.model("ok.py", "def model(env):\n    return None\n");
    harness.model(
        "boom.py",
        &format!("def model(env):\n    env.depends_on('ok')\n    raise RuntimeError('{SENTINEL_ERROR}')\n"),
    );
    let error = harness
        .run("boom.py", BTreeMap::new())
        .expect_err("must fail");
    assert_eq!(error.code(), "DOWAI_PYTHON_FAILED");

    let detail = only_run(&harness);
    assert_eq!(detail.run.status, "FAILED");
    assert_eq!(
        detail.run.error_code.as_deref(),
        Some("DOWAI_PYTHON_FAILED")
    );
    let statuses: Vec<_> = detail
        .invocations
        .iter()
        .map(|i| (i.target.as_str(), i.status.as_str()))
        .collect();
    assert_eq!(statuses, [("ok.py", "SUCCEEDED"), ("boom.py", "FAILED")]);
    assert_eq!(
        detail.invocations[1].error_code.as_deref(),
        Some("DOWAI_PYTHON_FAILED")
    );
    assert!(!harness.database_text().contains(SENTINEL_ERROR));
}

#[test]
fn timeout_is_journaled_with_its_error_code() {
    let harness = Harness::new();
    harness.set_timeout(1);
    harness.model(
        "slow.py",
        "import time\ndef model(env):\n    time.sleep(30)\n",
    );
    let error = harness
        .run("slow.py", BTreeMap::new())
        .expect_err("timeout");
    assert_eq!(error.code(), "DOWAI_PYTHON_TIMEOUT");
    let detail = only_run(&harness);
    assert_eq!(detail.run.status, "FAILED");
    assert_eq!(
        detail.run.error_code.as_deref(),
        Some("DOWAI_PYTHON_TIMEOUT")
    );
    assert_eq!(detail.invocations[0].status, "FAILED");
}

#[test]
fn configuration_failure_after_run_start_is_journaled() {
    let harness = Harness::new();
    harness.model("warehouse.sql", "select 1 as id");
    harness.model(
        "mixed.py",
        "def model(env):\n    env.depends_on('warehouse')\n    return None\n",
    );
    let error = harness
        .run("mixed.py", BTreeMap::new())
        .expect_err("no profile");
    assert_eq!(error.code(), "DOWAI_CONFIG_INVALID");
    let detail = only_run(&harness);
    assert_eq!(detail.run.status, "FAILED");
    assert_eq!(
        detail.run.error_code.as_deref(),
        Some("DOWAI_CONFIG_INVALID")
    );
    assert_eq!(detail.run.execution_mode.as_deref(), Some("automatic"));
    assert!(detail.invocations.is_empty());
}

#[test]
fn journal_write_failure_does_not_mask_the_run_outcome() {
    let harness = Harness::new();
    harness.model("ok.py", "def model(env):\n    return None\n");
    harness.model("bad.py", "def model(env):\n    raise RuntimeError('x')\n");
    // A directory where the database file should be makes every journal write fail.
    let path = default_run_store_path().expect("isolated path");
    fs::create_dir_all(&path).expect("block journal path");

    let result = harness
        .run("ok.py", BTreeMap::new())
        .expect("run still succeeds");
    assert_eq!(result.status, "SUCCEEDED");
    assert!(result
        .warnings
        .iter()
        .any(|warning| warning.contains("journal")));

    let error = harness
        .run("bad.py", BTreeMap::new())
        .expect_err("original error");
    assert_eq!(error.code(), "DOWAI_PYTHON_FAILED");
}
