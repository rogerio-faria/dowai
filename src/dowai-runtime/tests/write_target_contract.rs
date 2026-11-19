//! ENT-127: `env.write` accepts only profiles declared in `profiles.json`. An undeclared target is
//! refused before any connection is attempted, so this test needs no database.

use dowai_core::{create_project, ExecutionMode};
use dowai_runtime::{run_postgres, RunRequest};
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

struct Fixture {
    root: PathBuf,
    project: PathBuf,
    _env: Vec<EnvironmentRestore>,
}

impl Fixture {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock is after Unix epoch")
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("dowai-write-target-{}-{nonce}", std::process::id()));
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
        // A declared profile that is never contacted: the undeclared name is refused first.
        let workspace = json!({
            "schema_version": 2,
            "profiles": { "warehouse": {
                "type": "postgres",
                "host": "127.0.0.1",
                "port": 1,
                "user": "nobody",
                "database": "nowhere",
                "password": "{{ env_var('DOWAI_TEST_UNUSED_PASSWORD') }}",
                "tls": "disabled"
            }},
            "defaults": { "execution": { "default_profile": "warehouse" } }
        });
        fs::write(
            root.join("profiles.json"),
            serde_json::to_vec(&workspace).expect("serialize"),
        )
        .expect("write workspace");
        Self {
            root,
            project,
            _env: env,
        }
    }

    fn write_model(&self, file: &str, body: &str) {
        fs::write(self.project.join("models").join(file), body).expect("write model");
    }

    fn run(&self, target: &str) -> Result<dowai_runtime::RunResult, dowai_core::DowaiError> {
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

#[test]
fn env_write_to_an_undeclared_profile_is_refused_before_any_connection() {
    let fixture = Fixture::new();
    fixture.write_model(
        "grava.py",
        "def model(env, origem=None):\n    env.write([{'v': 1}], target='naoexiste', table='t', mode='append')\n",
    );

    let error = fixture
        .run("grava.py")
        .expect_err("an undeclared write target must be refused");

    assert_eq!(error.code(), "DOWAI_CONFIG_INVALID");
    assert!(
        error.message().contains("naoexiste"),
        "the refusal names the undeclared profile: {}",
        error.message()
    );
}
