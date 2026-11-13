use dowai_core::{create_project, default_session_store_path, SessionStore};
use dowai_runtime::{run_postgres, RunRequest};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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
            std::env::temp_dir().join(format!("dowai-python-e2e-{}-{nonce}", std::process::id()));
        fs::create_dir_all(&root).expect("create isolated workspace");
        let project = create_project(&root, "fixture").expect("initialize project");
        let virtual_environment = project.join(".venv");
        let venv_creation = Command::new(system_interpreter)
            .args(["-m", "venv", "--without-pip"])
            .arg(&virtual_environment)
            .output()
            .expect("create project virtual environment");
        assert!(
            venv_creation.status.success(),
            "Python must be able to create a project virtual environment"
        );
        let interpreter = if cfg!(windows) {
            virtual_environment.join("Scripts").join("python.exe")
        } else {
            virtual_environment.join("bin").join("python")
        };
        let project_manifest = project.join("dowai_project.json");
        let mut project_config: Value = serde_json::from_slice(
            &fs::read(&project_manifest).expect("read generated project config"),
        )
        .expect("generated project config is valid JSON");
        project_config["python"] = json!({
            "interpreter": interpreter,
            "timeout_seconds": 30
        });
        project_config["config"]["security"]["allow_external_writes"] = json!(true);
        fs::write(
            project_manifest,
            serde_json::to_vec(&project_config).expect("serialize project config"),
        )
        .expect("write project config");
        Self { root, project }
    }

    fn write_model(&self, name: &str, body: &str) {
        fs::write(self.project.join("models").join(name), body).expect("write Python model");
    }

    fn set_timeout(&self, timeout_seconds: u64) {
        let project_manifest = self.project.join("dowai_project.json");
        let mut project_config: Value = serde_json::from_slice(
            &fs::read(&project_manifest).expect("read generated project config"),
        )
        .expect("generated project config is valid JSON");
        project_config["python"]["timeout_seconds"] = json!(timeout_seconds);
        fs::write(
            project_manifest,
            serde_json::to_vec(&project_config).expect("serialize project config"),
        )
        .expect("write project config");
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

#[test]
fn supervised_python_dag_persists_variables_and_does_not_commit_failed_runs() {
    let state_root = std::env::temp_dir().join(format!(
        "dowai-python-state-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock is after Unix epoch")
            .as_nanos()
    ));
    fs::create_dir_all(&state_root).expect("create isolated state root");
    let local_app_data = state_root.join("local");
    let app_data = state_root.join("roaming");
    let system_data = state_root.join("system");
    fs::create_dir_all(&local_app_data).expect("create local app data");
    fs::create_dir_all(&app_data).expect("create roaming app data");
    fs::create_dir_all(&system_data).expect("create system data");
    let system_interpreter = python_executable();
    let _local_app_data = EnvironmentRestore::set("LOCALAPPDATA", &local_app_data.into_os_string());
    let _app_data = EnvironmentRestore::set("APPDATA", &app_data.clone().into_os_string());
    let _program_data = EnvironmentRestore::set("PROGRAMDATA", &system_data.into_os_string());
    let _xdg_config =
        EnvironmentRestore::set("XDG_CONFIG_HOME", &app_data.clone().into_os_string());
    let _xdg_state =
        EnvironmentRestore::set("XDG_STATE_HOME", &state_root.clone().into_os_string());
    let _secret =
        EnvironmentRestore::set("DOWAI_E2E_SECRET", &OsString::from("must-not-reach-python"));
    let workspace = TestWorkspace::new(&system_interpreter);
    workspace.write_model(
        "upstream.py",
        "def model(env):\n    env.setVar('stage', 'ready')\n    return None\n",
    );
    workspace.write_model(
        "consumer.py",
        "import os\ndef model(env):\n    env.depends_on('upstream')\n    assert env.getVar('stage') == 'ready'\n    assert 'DOWAI_E2E_SECRET' not in os.environ\n    env.setVar('result', 42)\n    return None\n",
    );

    let result = run_postgres(
        &workspace.project,
        RunRequest {
            targets: vec!["consumer.py".to_owned()],
            mode: None,
            profile: None,
            session_id: None,
            variables: BTreeMap::from([("input".to_owned(), json!({"batch": 7}))]),
            confirm_destructive: false,
            full_refresh: false,
        },
    )
    .expect("Python dependency DAG should execute without a database profile");

    assert_eq!(result.profile, "python");
    assert_eq!(
        result
            .nodes
            .iter()
            .map(|node| node.id.as_str())
            .collect::<Vec<_>>(),
        ["upstream.py", "consumer.py"]
    );
    let session_store =
        SessionStore::for_user_state(default_session_store_path().expect("isolated state path"));
    let variables = session_store
        .snapshot(&result.session_id)
        .expect("load persisted session variables");
    assert_eq!(variables["input"], json!({"batch": 7}));
    assert_eq!(variables["stage"], json!("ready"));
    assert_eq!(variables["result"], json!(42));

    workspace.write_model("warehouse.sql", "select 1 as id");
    workspace.write_model(
        "mixed.py",
        "def model(env):\n    env.depends_on('warehouse')\n    return None\n",
    );
    let mixed = run_postgres(
        &workspace.project,
        RunRequest {
            targets: vec!["mixed.py".to_owned()],
            mode: None,
            profile: None,
            session_id: Some(result.session_id.clone()),
            variables: BTreeMap::new(),
            confirm_destructive: false,
            full_refresh: false,
        },
    )
    .expect_err("mixed SQL/Python plans need a PostgreSQL profile; none is configured here");
    // Mixed execution is supported (see mixed_sql_python_e2e.rs); without a profile the
    // run is rejected as a configuration error before any node executes.
    assert_eq!(mixed.code(), "DOWAI_CONFIG_INVALID");

    workspace.write_model(
        "failure.py",
        "def model(env):\n    env.setVar('uncommitted', True)\n    raise RuntimeError('private model detail')\n",
    );
    let failure = run_postgres(
        &workspace.project,
        RunRequest {
            targets: vec!["failure.py".to_owned()],
            mode: None,
            profile: None,
            session_id: Some(result.session_id.clone()),
            variables: BTreeMap::new(),
            confirm_destructive: false,
            full_refresh: false,
        },
    )
    .expect_err("Python exception must be reported as a failed model");
    assert_eq!(failure.code(), "DOWAI_PYTHON_FAILED");
    assert!(!failure.message().contains("private model detail"));
    let variables_after_failure = session_store
        .snapshot(&result.session_id)
        .expect("failed runs do not commit staged variables");
    assert!(!variables_after_failure.contains_key("uncommitted"));

    workspace.write_model(
        "unsupported.py",
        "def model(env):\n    env.setVar('uncommitted', True)\n    env.write('upstream', 'x', 'y', mode='bogus')\n",
    );
    let unsupported = run_postgres(
        &workspace.project,
        RunRequest {
            targets: vec!["unsupported.py".to_owned()],
            mode: None,
            profile: None,
            session_id: Some(result.session_id.clone()),
            variables: BTreeMap::new(),
            confirm_destructive: false,
            full_refresh: false,
        },
    )
    .expect_err("invalid data API calls must not simulate success");
    // `env.write` is implemented (ENT-048); an invalid call fails the model, never succeeds.
    assert_eq!(unsupported.code(), "DOWAI_PYTHON_FAILED");

    workspace.write_model("returned.py", "def model(env):\n    return {'rows': []}\n");
    let unsupported_return = run_postgres(
        &workspace.project,
        RunRequest {
            targets: vec!["returned.py".to_owned()],
            mode: None,
            profile: None,
            session_id: Some(result.session_id.clone()),
            variables: BTreeMap::new(),
            confirm_destructive: false,
            full_refresh: false,
        },
    )
    .expect_err("dataset returns are not implemented");
    assert_eq!(unsupported_return.code(), "DOWAI_NOT_IMPLEMENTED");

    workspace.write_model(
        "malformed.py",
        "import os\ndef model(env):\n    os._exit(0)\n",
    );
    let malformed = run_postgres(
        &workspace.project,
        RunRequest {
            targets: vec!["malformed.py".to_owned()],
            mode: None,
            profile: None,
            session_id: Some(result.session_id.clone()),
            variables: BTreeMap::new(),
            confirm_destructive: false,
            full_refresh: false,
        },
    )
    .expect_err("malformed worker output must be reported");
    assert_eq!(malformed.code(), "DOWAI_PYTHON_PROTOCOL");

    workspace.set_timeout(5);
    let child_sentinel = workspace.root.join("child-survived-timeout");
    let parent_marker = workspace.root.join("timeout-model-started");
    let child_source = format!(
        "import time,pathlib; time.sleep(5.5); pathlib.Path({}).write_text('survived')",
        serde_json::to_string(&child_sentinel.to_string_lossy().to_string())
            .expect("encode child sentinel path")
    );
    let timeout_model = format!(
        "import pathlib,subprocess,sys,time\ndef model(env):\n    pathlib.Path({}).write_text('started')\n    env.setVar('timed_out', True)\n    subprocess.Popen([sys.executable, '-c', {}])\n    time.sleep(30)\n",
        serde_json::to_string(&parent_marker.to_string_lossy().to_string())
            .expect("encode model marker path"),
        serde_json::to_string(&child_source).expect("encode child process source")
    );
    workspace.write_model("timeout.py", &timeout_model);
    let timeout = run_postgres(
        &workspace.project,
        RunRequest {
            targets: vec!["timeout.py".to_owned()],
            mode: None,
            profile: None,
            session_id: Some(result.session_id.clone()),
            variables: BTreeMap::new(),
            confirm_destructive: false,
            full_refresh: false,
        },
    )
    .expect_err("timed-out models must be terminated");
    assert_eq!(timeout.code(), "DOWAI_PYTHON_TIMEOUT");
    assert!(
        parent_marker.exists(),
        "timeout must occur after the model starts"
    );
    thread::sleep(Duration::from_secs(2));
    assert!(
        !child_sentinel.exists(),
        "timeout should terminate subprocess descendants"
    );
    let final_variables = session_store
        .snapshot(&result.session_id)
        .expect("load variables after unsupported and timed-out runs");
    assert!(!final_variables.contains_key("uncommitted"));
    assert!(!final_variables.contains_key("timed_out"));

    drop(workspace);
    fs::remove_dir_all(state_root).expect("remove isolated session state");
}

/// AC002: without SQL models in the plan there are no datasets; `env.ref` fails with a stable,
/// catchable code and the run still succeeds when the model handles it.
#[test]
fn env_ref_without_sql_datasets_fails_with_a_catchable_code() {
    let system_interpreter = python_executable();
    let workspace = TestWorkspace::new(&system_interpreter);
    workspace.write_model("upstream.py", "def model(env):\n    return None\n");
    workspace.write_model(
        "reader.py",
        "def model(env):\n    try:\n        env.ref('upstream').schema()\n    except Exception as error:\n        env.setVar('ref_code', error.code)\n    return None\n",
    );

    let result = run_postgres(
        &workspace.project,
        RunRequest {
            targets: vec!["reader.py".to_owned()],
            mode: None,
            profile: None,
            session_id: None,
            variables: BTreeMap::new(),
            confirm_destructive: false,
            full_refresh: false,
        },
    )
    .expect("Python-only run should succeed when the model handles the refusal");

    assert_eq!(result.status, "SUCCEEDED");
    let session_store =
        SessionStore::for_user_state(default_session_store_path().expect("isolated state path"));
    let variables = session_store
        .snapshot(&result.session_id)
        .expect("load persisted session variables");
    assert_eq!(variables["ref_code"], json!("DOWAI_DATASET_UNAVAILABLE"));
}
