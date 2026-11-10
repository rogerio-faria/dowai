use dowai_core::create_project;
use serde_json::{json, Value};
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

struct Workspace(PathBuf);

impl Workspace {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock is after Unix epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "dowai-python-cli-e2e-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&root).expect("create isolated CLI workspace");
        Self(root)
    }

    fn project(&self, interpreter: &str) -> PathBuf {
        let project = create_project(&self.0, "fixture").expect("initialize project");
        let environment = project.join(".venv");
        let output = Command::new(interpreter)
            .args(["-m", "venv", "--without-pip"])
            .arg(&environment)
            .output()
            .expect("create project virtual environment");
        assert!(output.status.success(), "create virtual environment");
        let interpreter = if cfg!(windows) {
            environment.join("Scripts").join("python.exe")
        } else {
            environment.join("bin").join("python")
        };
        let project_file = project.join("dowai_project.json");
        let mut manifest: Value = serde_json::from_slice(
            &fs::read(&project_file).expect("read generated project manifest"),
        )
        .expect("generated project manifest is valid JSON");
        manifest["python"] = json!({
            "interpreter": interpreter,
            "timeout_seconds": 30
        });
        manifest["config"]["security"]["allow_external_writes"] = json!(true);
        fs::write(
            project_file,
            serde_json::to_vec(&manifest).expect("serialize project manifest"),
        )
        .expect("write project manifest");
        fs::write(
            project.join("models").join("model.py"),
            "def model(env):\n    env.setVar('cli_executed', True)\n    return None\n",
        )
        .expect("write Python model");
        project
    }

    fn state_directory(&self, name: &str) -> PathBuf {
        let path = self.0.join(name);
        fs::create_dir_all(&path).expect("create isolated CLI state directory");
        path
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn system_python() -> String {
    let executable = if cfg!(windows) { "python" } else { "python3" };
    let output = Command::new(executable)
        .args(["-c", "import sys; print(sys.executable)"])
        .output()
        .expect("Python 3 must be installed for CLI integration tests");
    assert!(output.status.success(), "query system Python executable");
    String::from_utf8(output.stdout)
        .expect("Python executable path must be UTF-8")
        .trim()
        .to_owned()
}

#[test]
fn cli_runs_python_models_without_a_database_profile() {
    let workspace = Workspace::new();
    let project = workspace.project(&system_python());
    let state = workspace.state_directory("state");
    let config = workspace.state_directory("config");
    let appdata = workspace.state_directory("appdata");
    let system_data = workspace.state_directory("system");
    let output = Command::new(env!("CARGO_BIN_EXE_dowai"))
        .args(["run", "model.py", "--json"])
        .current_dir(&project)
        .env("XDG_STATE_HOME", &state)
        .env("XDG_CONFIG_HOME", &config)
        .env("LOCALAPPDATA", &state)
        .env("APPDATA", &appdata)
        .env("PROGRAMDATA", &system_data)
        .output()
        .expect("run DOWAI CLI");

    assert!(
        output.status.success(),
        "CLI should run a Python-only target without a PostgreSQL profile"
    );
    let result: Value =
        serde_json::from_slice(&output.stdout).expect("CLI emits a JSON run result");
    assert_eq!(result["status"], json!("SUCCEEDED"));
    assert_eq!(result["profile"], json!("python"));
    assert_eq!(result["nodes"][0]["id"], json!("model.py"));
    assert!(result["session_id"].as_str().is_some());
}
