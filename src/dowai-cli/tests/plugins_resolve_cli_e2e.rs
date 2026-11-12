//! ENT-106: `dowai plugins resolve <namespace.method> [--json]` runs the single resolver through the
//! real binary. It resolves the target to its entrypoint, refuses undeclared methods, unknown
//! plugins and overrides disabled by `plugins.allow_local_override`, and never imports or executes
//! plugin code (a sentinel written at import time must not appear).

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
            std::env::temp_dir().join(format!("dowai-resolve-cli-{}-{nonce}", std::process::id()));
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

    /// Creates a workspace and a project; returns the project directory.
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
        assert!(
            init.status.success(),
            "init failed: {}",
            String::from_utf8_lossy(&init.stderr)
        );
        let payload: Value = serde_json::from_slice(&init.stdout).expect("init JSON");
        PathBuf::from(payload["project_dir"].as_str().expect("project_dir"))
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Writes `<root>/plugins/<id>/plugin.json` and a module whose import would create `sentinel`.
fn write_plugin(root: &Path, id: &str, methods: &[(&str, &str)], sentinel: &Path) {
    let dir = root.join("plugins").join(id);
    fs::create_dir_all(&dir).expect("plugin dir");
    let entrypoints = methods
        .iter()
        .map(|(method, target)| format!("\"{method}\": \"{target}\""))
        .collect::<Vec<_>>()
        .join(", ");
    fs::write(
        dir.join("plugin.json"),
        format!(
            r#"{{
  "schema_version": 2,
  "id": "{id}",
  "version": "1.0.0",
  "entrypoints": {{ {entrypoints} }},
  "permissions": {{ "network": [], "filesystem": [], "secrets": [] }},
  "timeout_seconds": 30,
  "side_effects": false,
  "idempotency_key_required": false
}}"#
        ),
    )
    .expect("manifest");
    fs::write(
        dir.join("sender.py"),
        format!(
            "open({:?}, 'w').write('imported')\n\ndef send(**kwargs):\n    return None\n",
            sentinel.display().to_string()
        ),
    )
    .expect("module");
}

fn stdout_json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "stdout is not JSON ({error}): {}",
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

#[test]
fn resolves_target_to_declared_entrypoint_without_importing_plugin() {
    let sandbox = Sandbox::new();
    let project = sandbox.project();
    let sentinel = sandbox.0.join("imported.flag");
    write_plugin(&project, "email", &[("send", "sender:send")], &sentinel);

    let output = sandbox.command(&project, &["plugins", "resolve", "email.send", "--json"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let payload = stdout_json(&output);
    assert_eq!(payload["target"], "email.send");
    assert_eq!(payload["entrypoint"], "sender:send");
    assert_eq!(payload["plugin"]["id"], "email");
    assert!(payload["override"].is_null());
    assert!(!sentinel.exists(), "resolve must not import plugin code");
}

#[test]
fn text_output_names_the_resolved_entrypoint() {
    let sandbox = Sandbox::new();
    let project = sandbox.project();
    write_plugin(
        &project,
        "email",
        &[("send", "sender:send")],
        &sandbox.0.join("s"),
    );

    let output = sandbox.command(&project, &["plugins", "resolve", "email.send"]);
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "email.send -> sender:send"
    );
}

#[test]
fn undeclared_method_unknown_plugin_and_malformed_target_are_refused() {
    let sandbox = Sandbox::new();
    let project = sandbox.project();
    write_plugin(
        &project,
        "email",
        &[("send", "sender:send")],
        &sandbox.0.join("s"),
    );

    for target in ["email.delete", "missing.send", "email", ".send"] {
        let output = sandbox.command(&project, &["plugins", "resolve", target, "--json"]);
        assert!(!output.status.success(), "{target} must be refused");
        let payload = stdout_json(&output);
        assert_eq!(
            payload["error_code"], "DOWAI_CONFIG_INVALID",
            "target {target}"
        );
    }
}

#[test]
fn override_is_applied_by_default_and_recorded() {
    let sandbox = Sandbox::new();
    let project = sandbox.project();
    let workspace = sandbox.0.join("workspace");
    let sentinel = sandbox.0.join("s");
    write_plugin(&workspace, "email", &[("send", "global:send")], &sentinel);
    write_plugin(&project, "email", &[("send", "local:send")], &sentinel);

    let output = sandbox.command(&project, &["plugins", "resolve", "email.send", "--json"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let payload = stdout_json(&output);
    assert_eq!(payload["entrypoint"], "local:send");
    assert_eq!(payload["override"]["replaced_origin"], "global");
    assert!(!sentinel.exists());
}

#[test]
fn override_is_refused_when_disabled_in_project_configuration() {
    let sandbox = Sandbox::new();
    let project = sandbox.project();
    let workspace = sandbox.0.join("workspace");
    let sentinel = sandbox.0.join("s");
    write_plugin(&workspace, "email", &[("send", "global:send")], &sentinel);
    write_plugin(&project, "email", &[("send", "local:send")], &sentinel);

    let manifest_path = project.join("dowai_project.json");
    let mut manifest: Value =
        serde_json::from_str(&fs::read_to_string(&manifest_path).expect("read manifest"))
            .expect("manifest JSON");
    manifest["config"]["plugins"]["allow_local_override"] = Value::Bool(false);
    fs::write(&manifest_path, manifest.to_string()).expect("write manifest");

    let output = sandbox.command(&project, &["plugins", "resolve", "email.send", "--json"]);
    assert!(!output.status.success());
    let payload = stdout_json(&output);
    assert_eq!(payload["error_code"], "DOWAI_POLICY_DENIED");
    assert!(!sentinel.exists());
}
