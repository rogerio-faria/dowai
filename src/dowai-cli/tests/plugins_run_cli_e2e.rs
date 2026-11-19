//! ENT-107: `dowai plugins run <namespace.method> [--input <json>] [--idempotency-key <chave>] [--json]`
//! pelo binário real. Alvo não resolvido nunca executa código; timeout encerra o grupo; a
//! execução vai para o AUDIT; a chave evita repetir o efeito externo.

use serde_json::{json, Value};
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
            std::env::temp_dir().join(format!("dowai-run-cli-{}-{nonce}", std::process::id()));
        fs::create_dir_all(&root).expect("create sandbox");
        Self(root)
    }

    fn command(&self, cwd: &Path, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_dowai"))
            .args(args)
            .current_dir(cwd)
            .env("XDG_STATE_HOME", self.0.join("state"))
            .env("XDG_CONFIG_HOME", self.0.join("config"))
            .env("LOCALAPPDATA", self.0.join("local"))
            .env("APPDATA", self.0.join("appdata"))
            .env("PROGRAMDATA", self.0.join("programdata"))
            .output()
            .expect("run DOWAI CLI")
    }

    /// Projeto com `python.interpreter` em um venv do próprio projeto (mesma regra do runtime).
    fn project(&self) -> PathBuf {
        let workspace = self.0.join("workspace");
        fs::create_dir_all(&workspace).expect("create workspace");
        let init = self.command(
            &self.0,
            &[
                "init",
                "demo",
                "--workspace",
                workspace.to_str().expect("utf-8"),
                "--json",
            ],
        );
        assert!(init.status.success());
        let payload: Value = serde_json::from_slice(&init.stdout).expect("init JSON");
        let project = PathBuf::from(payload["project_dir"].as_str().expect("project_dir"));
        let venv = project.join(".venv");
        let system_python = if cfg!(windows) { "python" } else { "python3" };
        let created = Command::new(system_python)
            .args(["-m", "venv", "--without-pip"])
            .arg(&venv)
            .output()
            .expect("system python");
        assert!(created.status.success(), "create venv");
        // Layout do venv: `Scripts\python.exe` no Windows; `bin/python` em Unix.
        let venv_python = if cfg!(windows) {
            venv.join("Scripts").join("python.exe")
        } else {
            venv.join("bin").join("python")
        };
        let file = project.join("dowai_project.json");
        let mut manifest: Value =
            serde_json::from_slice(&fs::read(&file).expect("read")).expect("project manifest JSON");
        manifest["python"] = json!({
            "interpreter": venv_python,
            "timeout_seconds": 30
        });
        fs::write(&file, manifest.to_string()).expect("write manifest");
        project
    }

    fn audit_events(&self) -> Vec<Value> {
        let path = if cfg!(windows) {
            self.0.join("local/DOWAI/state/audit.jsonl")
        } else {
            self.0.join("state/dowai/audit.jsonl")
        };
        fs::read_to_string(path)
            .expect("read audit")
            .lines()
            .map(|line| serde_json::from_str(line).expect("audit line"))
            .collect()
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn write_plugin(root: &Path, id: &str, timeout: u64, required: bool, source: &str) {
    let dir = root.join("plugins").join(id);
    fs::create_dir_all(&dir).expect("plugin dir");
    fs::write(
        dir.join("plugin.json"),
        json!({
            "schema_version": 2,
            "id": id,
            "version": "1.0.0",
            "entrypoints": { "send": "sender:send" },
            "permissions": { "network": [], "filesystem": [], "secrets": [] },
            "timeout_seconds": timeout,
            "side_effects": required,
            "idempotency_key_required": required,
        })
        .to_string(),
    )
    .expect("manifest");
    fs::write(dir.join("sender.py"), source).expect("module");
}

/// Declara `filesystem` no manifesto do plugin e libera `plugins.allow_filesystem` no projeto (ENT-056).
fn allow_filesystem_for(project: &Path, id: &str, resource: &str) {
    let plugin_path = project.join("plugins").join(id).join("plugin.json");
    let mut plugin: Value =
        serde_json::from_str(&fs::read_to_string(&plugin_path).expect("read plugin"))
            .expect("json");
    plugin["permissions"]["filesystem"] = json!([resource]);
    fs::write(&plugin_path, plugin.to_string()).expect("write plugin");
    let manifest_path = project.join("dowai_project.json");
    let mut manifest: Value =
        serde_json::from_str(&fs::read_to_string(&manifest_path).expect("read manifest"))
            .expect("manifest JSON");
    manifest["config"]["plugins"]["allow_filesystem"] = Value::Bool(true);
    fs::write(&manifest_path, manifest.to_string()).expect("write manifest");
}

fn stdout_json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "stdout is not JSON ({error}): {} / {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

#[test]
fn runs_a_resolved_plugin_with_json_input_and_audits_it() {
    let sandbox = Sandbox::new();
    let project = sandbox.project();
    write_plugin(
        &project,
        "email",
        30,
        false,
        "def send(**kwargs):\n    return {'got': kwargs}\n",
    );

    let output = sandbox.command(
        &project,
        &[
            "plugins",
            "run",
            "email.send",
            "--input",
            r#"{"to": "a@example.test"}"#,
            "--json",
        ],
    );
    assert!(
        output.status.success(),
        "{} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let payload = stdout_json(&output);
    assert_eq!(payload["status"], "SUCCEEDED");
    assert_eq!(payload["target"], "email.send");
    assert_eq!(payload["result"], json!({"got": {"to": "a@example.test"}}));
    assert_eq!(payload["audit_recorded"], true);

    let events = sandbox.audit_events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["event"], "plugin.executed");
    assert_eq!(events[0]["plugin_id"], "email");
    assert_eq!(events[0]["plugin_version"], "1.0.0");
    assert_eq!(
        events[0]["manifest_sha256"],
        payload["plugin"]["manifest_sha256"]
    );
}

#[test]
fn unresolved_target_never_executes_plugin_code() {
    let sandbox = Sandbox::new();
    let project = sandbox.project();
    let sentinel = sandbox.0.join("imported.flag");
    write_plugin(
        &project,
        "email",
        30,
        false,
        &format!(
            "open({:?}, 'w').write('x')\ndef send(**kw):\n    return 1\n",
            sentinel.display().to_string()
        ),
    );

    for target in ["email.delete", "missing.send", "email", ".send"] {
        let output = sandbox.command(&project, &["plugins", "run", target, "--json"]);
        assert!(!output.status.success(), "{target} must be refused");
        assert_eq!(stdout_json(&output)["error_code"], "DOWAI_CONFIG_INVALID");
    }
    assert!(!sentinel.exists(), "unresolved targets never run code");
}

#[test]
fn missing_key_exit_code_and_repeated_key_do_not_repeat_the_effect() {
    let sandbox = Sandbox::new();
    let project = sandbox.project();
    let effects = sandbox.0.join("effects.log");
    write_plugin(
        &project,
        "email",
        30,
        true,
        &format!(
            "def send(**kw):\n    open({:?}, 'a').write('x')\n    return {{'sent': True}}\n",
            effects.display().to_string()
        ),
    );
    allow_filesystem_for(&project, "email", "effects");

    let refused = sandbox.command(&project, &["plugins", "run", "email.send", "--json"]);
    assert_eq!(refused.status.code(), Some(2));
    assert_eq!(
        stdout_json(&refused)["error_code"],
        "DOWAI_PLUGIN_IDEMPOTENCY_KEY_REQUIRED"
    );
    assert!(!effects.exists());

    let args = [
        "plugins",
        "run",
        "email.send",
        "--idempotency-key",
        "alert-1",
        "--json",
    ];
    let first = sandbox.command(&project, &args);
    assert!(first.status.success());
    assert_eq!(stdout_json(&first)["status"], "SUCCEEDED");
    let second = sandbox.command(&project, &args);
    assert!(second.status.success());
    assert_eq!(stdout_json(&second)["status"], "DUPLICATE");
    assert_eq!(fs::read_to_string(&effects).expect("effects"), "x");
}

#[test]
fn timeout_fails_with_a_stable_code() {
    let sandbox = Sandbox::new();
    let project = sandbox.project();
    write_plugin(
        &project,
        "email",
        1,
        false,
        "import time\ndef send(**kw):\n    time.sleep(60)\n",
    );
    let output = sandbox.command(&project, &["plugins", "run", "email.send", "--json"]);
    assert_eq!(output.status.code(), Some(3));
    assert_eq!(stdout_json(&output)["error_code"], "DOWAI_PLUGIN_TIMEOUT");
}

#[test]
fn project_without_python_interpreter_is_refused_before_running() {
    let sandbox = Sandbox::new();
    let project = sandbox.project();
    let file = project.join("dowai_project.json");
    let mut manifest: Value =
        serde_json::from_slice(&fs::read(&file).expect("read")).expect("json");
    manifest.as_object_mut().expect("object").remove("python");
    fs::write(&file, manifest.to_string()).expect("write");
    let sentinel = sandbox.0.join("imported.flag");
    write_plugin(
        &project,
        "email",
        30,
        false,
        &format!(
            "open({:?}, 'w').write('x')\ndef send(**kw):\n    return 1\n",
            sentinel.display().to_string()
        ),
    );
    let output = sandbox.command(&project, &["plugins", "run", "email.send", "--json"]);
    assert!(!output.status.success());
    assert_eq!(stdout_json(&output)["error_code"], "DOWAI_CONFIG_INVALID");
    assert!(!sentinel.exists());
}

#[test]
fn invalid_input_text_is_refused_with_the_input_code() {
    let sandbox = Sandbox::new();
    let project = sandbox.project();
    write_plugin(
        &project,
        "email",
        30,
        false,
        "def send(**kw):\n    return 1\n",
    );
    for input in ["not json", "[1]"] {
        let output = sandbox.command(
            &project,
            &["plugins", "run", "email.send", "--input", input, "--json"],
        );
        assert_eq!(output.status.code(), Some(2), "{input}");
        assert_eq!(
            stdout_json(&output)["error_code"],
            "DOWAI_PLUGIN_INPUT_INVALID"
        );
    }
}

#[test]
fn declared_network_without_policy_is_refused_with_exit_5_and_never_runs() {
    let sandbox = Sandbox::new();
    let project = sandbox.project();
    let marker = sandbox.0.join("imported.flag");
    write_plugin(
        &project,
        "email",
        30,
        false,
        &format!(
            "open({:?}, 'w').close()\ndef send(**kw):\n    return {{}}\n",
            marker.display().to_string()
        ),
    );
    let plugin_path = project.join("plugins").join("email").join("plugin.json");
    let mut plugin: Value =
        serde_json::from_str(&fs::read_to_string(&plugin_path).expect("read plugin"))
            .expect("json");
    plugin["permissions"]["network"] = json!(["mail_gateway"]);
    fs::write(&plugin_path, plugin.to_string()).expect("write plugin");

    let output = sandbox.command(&project, &["plugins", "run", "email.send", "--json"]);
    assert_eq!(output.status.code(), Some(5));
    assert_eq!(stdout_json(&output)["error_code"], "DOWAI_POLICY_DENIED");
    assert!(!marker.exists());
}
