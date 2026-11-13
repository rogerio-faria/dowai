//! `dowai retry` (B-05c) against real Python runs. One test function sequences the scenarios
//! because they share one isolated user state directory (process environment is global).

use dowai_core::{
    create_project, default_run_store_path, default_session_store_path, CancellationToken,
    RunStore, SessionStore,
};
use dowai_runtime::{
    retry_run_with_cancel, run_postgres, run_postgres_with_cancel, RetryRequest, RunRequest,
};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

struct TestWorkspace {
    root: PathBuf,
    project: PathBuf,
}

impl TestWorkspace {
    fn new(system_interpreter: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock is after Unix epoch")
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("dowai-retry-e2e-{}-{nonce}", std::process::id()));
        fs::create_dir_all(&root).expect("create isolated workspace");
        let project = create_project(&root, "fixture").expect("initialize project");
        let virtual_environment = project.join(".venv");
        let venv_creation = Command::new(system_interpreter)
            .args(["-m", "venv", "--without-pip"])
            .arg(&virtual_environment)
            .output()
            .expect("create project virtual environment");
        assert!(venv_creation.status.success(), "create virtual environment");
        let interpreter = if cfg!(windows) {
            virtual_environment.join("Scripts").join("python.exe")
        } else {
            virtual_environment.join("bin").join("python")
        };
        let manifest = project.join("dowai_project.json");
        let mut config: Value =
            serde_json::from_slice(&fs::read(&manifest).expect("read project config"))
                .expect("valid project config");
        config["python"] = json!({"interpreter": interpreter, "timeout_seconds": 30});
        config["config"]["security"]["allow_external_writes"] = json!(true);
        fs::write(&manifest, serde_json::to_vec(&config).expect("serialize")).expect("write");
        Self { root, project }
    }

    fn write_model(&self, name: &str, body: &str) {
        fs::write(self.project.join("models").join(name), body).expect("write Python model");
    }

    fn set_timeout(&self, timeout_seconds: u64) {
        let manifest = self.project.join("dowai_project.json");
        let mut config: Value =
            serde_json::from_slice(&fs::read(&manifest).expect("read project config"))
                .expect("valid project config");
        config["python"]["timeout_seconds"] = json!(timeout_seconds);
        fs::write(&manifest, serde_json::to_vec(&config).expect("serialize")).expect("write");
    }
}

impl Drop for TestWorkspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

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

fn python_executable() -> String {
    let executable = if cfg!(windows) { "python" } else { "python3" };
    let output = Command::new(executable)
        .args(["-c", "import sys; print(sys.executable)"])
        .output()
        .expect("Python 3 must be installed for Python runtime integration tests");
    assert!(output.status.success(), "query Python interpreter path");
    String::from_utf8(output.stdout)
        .expect("Python executable path must be UTF-8")
        .trim()
        .to_owned()
}

/// A path as a Python string literal (JSON string syntax is valid Python for these paths).
fn literal(path: &Path) -> String {
    serde_json::to_string(&path.to_string_lossy().to_string()).expect("encode path")
}

fn journal() -> RunStore {
    RunStore::for_user_state(default_run_store_path().expect("isolated journal path"))
}

fn request(targets: &[&str], variables: BTreeMap<String, Value>) -> RunRequest {
    RunRequest {
        targets: targets.iter().map(|target| (*target).to_owned()).collect(),
        mode: None,
        profile: None,
        session_id: None,
        variables,
        confirm_destructive: false,
        full_refresh: false,
    }
}

fn retry(
    workspace: &TestWorkspace,
    run_id: &str,
    failed_only: bool,
    variables: BTreeMap<String, Value>,
) -> dowai_core::Result<dowai_runtime::RunResult> {
    retry_run_with_cancel(
        &workspace.project,
        run_id,
        RetryRequest {
            failed_only,
            variables,
            confirm_destructive: false,
        },
        &CancellationToken::new(),
    )
}

fn node_ids(result: &dowai_runtime::RunResult) -> Vec<&str> {
    result.nodes.iter().map(|node| node.id.as_str()).collect()
}

#[test]
fn retry_refusals_selective_and_full_retries_are_journaled() {
    let state_root = std::env::temp_dir().join(format!(
        "dowai-retry-state-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock is after Unix epoch")
            .as_nanos()
    ));
    let local_app_data = state_root.join("local");
    let app_data = state_root.join("roaming");
    let system_data = state_root.join("system");
    for directory in [&local_app_data, &app_data, &system_data] {
        fs::create_dir_all(directory).expect("create isolated app data");
    }
    let system_interpreter = python_executable();
    let _local = EnvironmentRestore::set("LOCALAPPDATA", &local_app_data.clone().into_os_string());
    let _app = EnvironmentRestore::set("APPDATA", &app_data.clone().into_os_string());
    let _program = EnvironmentRestore::set("PROGRAMDATA", &system_data.into_os_string());
    let _config = EnvironmentRestore::set("XDG_CONFIG_HOME", &app_data.clone().into_os_string());
    let _state = EnvironmentRestore::set("XDG_STATE_HOME", &state_root.clone().into_os_string());

    let workspace = TestWorkspace::new(&system_interpreter);
    let upstream_log = workspace.root.join("upstream-runs.log");
    let flaky_marker = workspace.root.join("flaky-first-attempt-done");
    let independent_marker = workspace.root.join("independent-first-attempt-done");
    let sleepy_marker = workspace.root.join("sleepy-first-attempt-done");
    workspace.write_model(
        "upstream.py",
        &format!(
            "import pathlib\ndef model(env):\n    with pathlib.Path({}).open('a') as handle:\n        handle.write('run\\n')\n    env.setVar('stage', 'ready')\n",
            literal(&upstream_log)
        ),
    );
    workspace.write_model(
        "flaky.py",
        &format!(
            "import pathlib\ndef model(env):\n    env.depends_on('upstream')\n    marker = pathlib.Path({})\n    if not marker.exists():\n        marker.write_text('x')\n        raise RuntimeError('first attempt fails')\n    assert env.getVar('stage') == 'ready'\n    assert env.getVar('input') == {{'batch': 7, 'note': 'RETRY_SENTINEL_9d1'}}\n    env.setVar('recovered', True)\n",
            literal(&flaky_marker)
        ),
    );
    workspace.write_model(
        "independent.py",
        &format!(
            "import pathlib\ndef model(env):\n    marker = pathlib.Path({})\n    if not marker.exists():\n        marker.write_text('x')\n        raise RuntimeError('first attempt fails')\n    env.setVar('independent', True)\n",
            literal(&independent_marker)
        ),
    );
    workspace.write_model(
        "sleepy.py",
        &format!(
            "import pathlib, time\ndef model(env):\n    marker = pathlib.Path({})\n    if not marker.exists():\n        marker.write_text('x')\n        time.sleep(20)\n    env.setVar('slept', True)\n",
            literal(&sleepy_marker)
        ),
    );
    let session_store =
        SessionStore::for_user_state(default_session_store_path().expect("session path"));
    let sentinel_variables = BTreeMap::from([(
        "input".to_owned(),
        json!({"batch": 7, "note": "RETRY_SENTINEL_9d1"}),
    )]);

    // 1. A run with a Python predecessor that fails on its first attempt.
    let failure = run_postgres(
        &workspace.project,
        request(&["flaky.py"], sentinel_variables.clone()),
    )
    .expect_err("first attempt must fail");
    assert_eq!(failure.code(), "DOWAI_PYTHON_FAILED");
    let failed_id = journal().list(1).expect("list runs")[0].run_id.clone();
    assert_eq!(
        fs::read_to_string(&upstream_log)
            .expect("upstream log")
            .lines()
            .count(),
        1
    );

    // 2. Selective retry is refused: upstream succeeded, but its variables were never committed.
    let refused = retry(&workspace, &failed_id, true, sentinel_variables.clone())
        .expect_err("selective retry must not miss uncommitted variables");
    assert_eq!(refused.code(), "DOWAI_RETRY_UNCOMMITTED_VARIABLES");

    // 3. Original variables are required; the refusal starts no run and writes no journal entry.
    let runs_before = journal().list(100).expect("list").len();
    let missing =
        retry(&workspace, &failed_id, false, BTreeMap::new()).expect_err("variables are required");
    assert_eq!(missing.code(), "DOWAI_RETRY_VARIABLES_REQUIRED");
    assert_eq!(journal().list(100).expect("list").len(), runs_before);

    // 4. Full retry with the variables re-executes every node and succeeds.
    let retried = retry(&workspace, &failed_id, false, sentinel_variables.clone())
        .expect("full retry should succeed on the second attempt");
    assert_eq!(retried.status, "SUCCEEDED");
    assert_eq!(node_ids(&retried), ["upstream.py", "flaky.py"]);
    assert_eq!(
        fs::read_to_string(&upstream_log)
            .expect("upstream log")
            .lines()
            .count(),
        2,
        "full retry re-executes the predecessor"
    );
    let variables = session_store
        .snapshot(&retried.session_id)
        .expect("retry session");
    assert_eq!(variables["recovered"], json!(true));

    // 5. A succeeded run is not retried.
    let succeeded = retry(
        &workspace,
        &retried.run_id,
        false,
        sentinel_variables.clone(),
    )
    .expect_err("succeeded runs are refused");
    assert_eq!(succeeded.code(), "DOWAI_RUN_NOT_RETRYABLE");

    // 6. Selective retry of a model without uncommitted predecessors runs only that model.
    let independent_failure = run_postgres(
        &workspace.project,
        request(&["independent.py"], BTreeMap::new()),
    )
    .expect_err("first attempt must fail");
    assert_eq!(independent_failure.code(), "DOWAI_PYTHON_FAILED");
    let independent_id = journal().list(1).expect("list")[0].run_id.clone();
    let selective = retry(&workspace, &independent_id, true, BTreeMap::new())
        .expect("selective retry succeeds");
    assert_eq!(node_ids(&selective), ["independent.py"]);
    assert_eq!(selective.status, "SUCCEEDED");

    // 7. A run ended CANCELLED is retryable; with nothing FAILED, selective retry is refused.
    let cancel = CancellationToken::new();
    cancel.cancel();
    let cancelled = run_postgres_with_cancel(
        &workspace.project,
        request(&["independent.py"], BTreeMap::new()),
        &cancel,
    )
    .expect_err("pre-cancelled run ends CANCELLED");
    assert_eq!(cancelled.code(), "DOWAI_CANCELLED");
    let cancelled_id = journal().list(1).expect("list")[0].run_id.clone();
    let no_failed = retry(&workspace, &cancelled_id, true, BTreeMap::new())
        .expect_err("no FAILED model to retry");
    assert_eq!(no_failed.code(), "DOWAI_RETRY_NO_FAILED_NODES");
    let after_cancel = retry(&workspace, &cancelled_id, false, BTreeMap::new())
        .expect("cancelled runs can be retried");
    assert_eq!(after_cancel.status, "SUCCEEDED");

    // 8. A timed-out model is FAILED and can be retried.
    workspace.set_timeout(2);
    let timeout = run_postgres(&workspace.project, request(&["sleepy.py"], BTreeMap::new()))
        .expect_err("first attempt times out");
    assert_eq!(timeout.code(), "DOWAI_PYTHON_TIMEOUT");
    let timed_out_id = journal().list(1).expect("list")[0].run_id.clone();
    workspace.set_timeout(30);
    let after_timeout = retry(&workspace, &timed_out_id, false, BTreeMap::new())
        .expect("timed-out runs can be retried");
    assert_eq!(after_timeout.status, "SUCCEEDED");

    // 9. The journal keeps variable names and counts, never values.
    let database = default_run_store_path().expect("journal path");
    let wal = PathBuf::from(format!("{}-wal", database.display()));
    let mut bytes = fs::read(&database).expect("journal file");
    if let Ok(extra) = fs::read(&wal) {
        bytes.extend(extra);
    }
    assert!(
        !bytes
            .windows(b"RETRY_SENTINEL_9d1".len())
            .any(|window| window == b"RETRY_SENTINEL_9d1"),
        "variable values must not reach the run journal"
    );

    drop(workspace);
    fs::remove_dir_all(state_root).expect("remove isolated state");
}
