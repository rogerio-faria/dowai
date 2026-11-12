//! ENT-055: `dowai plugins sync` writes typed `.pyi` stubs under `.dowai/stubs/dowai_plugins/` through
//! the real binary. It must be deterministic (second run rewrites nothing), remove stubs of plugins
//! that no longer exist, reflect the local override of a global plugin, warn about untyped
//! parameters, and never import or execute plugin code (a sentinel written at import must not appear).

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
            std::env::temp_dir().join(format!("dowai-sync-cli-{}-{nonce}", std::process::id()));
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

    /// Creates a workspace and a project; returns `(workspace_dir, project_dir)`.
    fn project(&self) -> (PathBuf, PathBuf) {
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
        (
            workspace,
            PathBuf::from(payload["project_dir"].as_str().expect("project_dir")),
        )
    }

    fn stubs_dir(project: &Path) -> PathBuf {
        project.join(".dowai/stubs/dowai_plugins")
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Writes `<root>/plugins/<id>/plugin.json` and a module that declares `source` and, on import,
/// creates `sentinel`.
fn write_plugin(root: &Path, id: &str, methods: &[(&str, &str)], source: &str, sentinel: &Path) {
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
            "open({:?}, 'w').write('imported')\n\n{source}",
            sentinel.display().to_string()
        ),
    )
    .expect("module");
}

const EMAIL_SOURCE: &str = "from pathlib import Path\n\
def send(to: list[str], subject: str, message: str | None = None, priority: int = 0, attachment: Path | None = None, **options) -> dict:\n    return {}\n\
async def notify(channel, retries: int = 3):\n    return None\n";

fn stdout_json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "stdout is not JSON ({error}): {}",
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

fn sync_json(sandbox: &Sandbox, project: &Path) -> Value {
    let output = sandbox.command(project, &["plugins", "sync", "--json"]);
    assert!(
        output.status.success(),
        "sync failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    stdout_json(&output)
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .expect("array")
        .iter()
        .map(|item| item.as_str().expect("string").to_owned())
        .collect()
}

#[test]
fn writes_typed_stub_without_importing_plugin_and_warns_on_untyped_parameters() {
    let sandbox = Sandbox::new();
    let (_workspace, project) = sandbox.project();
    let sentinel = sandbox.0.join("imported.flag");
    write_plugin(
        &project,
        "email",
        &[("send", "sender:send"), ("notify", "sender:notify")],
        EMAIL_SOURCE,
        &sentinel,
    );

    let payload = sync_json(&sandbox, &project);
    assert_eq!(strings(&payload["plugins"]), vec!["email"]);
    assert_eq!(
        strings(&payload["written"]),
        vec!["__init__.pyi", "email.pyi"]
    );
    assert!(!sentinel.exists(), "sync must not import plugin code");

    let stub = fs::read_to_string(Sandbox::stubs_dir(&project).join("email.pyi")).expect("stub");
    assert!(
        stub.contains(
            "def send(*, to: list[str], subject: str, message: str | None = ..., priority: int = ..., attachment: Any = ..., **options: Any) -> Any: ..."
        ),
        "typed signature missing:\n{stub}"
    );
    assert!(
        stub.contains("async def notify(*, channel: Any, retries: int = ...) -> Any: ..."),
        "async entrypoint or untyped parameter wrong:\n{stub}"
    );
    assert!(
        !stub.contains("Path"),
        "module-only annotation must not leak"
    );

    let init = fs::read_to_string(Sandbox::stubs_dir(&project).join("__init__.pyi")).expect("init");
    assert!(init.contains("from . import email as email"));

    let codes: Vec<&str> = payload["warnings"]
        .as_array()
        .expect("warnings")
        .iter()
        .map(|warning| warning["code"].as_str().expect("code"))
        .collect();
    assert!(codes.contains(&"DOWAI_STUB_UNTYPED_PARAM"), "{codes:?}");
    assert!(
        codes.contains(&"DOWAI_STUB_TYPE_NOT_REPRODUCED"),
        "{codes:?}"
    );
}

#[test]
fn repeated_sync_is_byte_identical_and_rewrites_nothing() {
    let sandbox = Sandbox::new();
    let (_workspace, project) = sandbox.project();
    let sentinel = sandbox.0.join("imported.flag");
    write_plugin(
        &project,
        "email",
        &[("send", "sender:send")],
        EMAIL_SOURCE,
        &sentinel,
    );

    let first = sync_json(&sandbox, &project);
    let stub_path = Sandbox::stubs_dir(&project).join("email.pyi");
    let before = fs::read(&stub_path).expect("stub");

    let second = sync_json(&sandbox, &project);
    assert_eq!(fs::read(&stub_path).expect("stub"), before);
    assert!(strings(&first["written"]).contains(&"email.pyi".to_owned()));
    assert!(second["written"].as_array().expect("array").is_empty());
    assert_eq!(
        strings(&second["unchanged"]),
        vec!["__init__.pyi", "email.pyi"]
    );
}

#[test]
fn removes_stub_of_plugin_that_no_longer_exists() {
    let sandbox = Sandbox::new();
    let (_workspace, project) = sandbox.project();
    let sentinel = sandbox.0.join("imported.flag");
    write_plugin(
        &project,
        "email",
        &[("send", "sender:send")],
        EMAIL_SOURCE,
        &sentinel,
    );
    write_plugin(
        &project,
        "files",
        &[("read", "sender:send")],
        EMAIL_SOURCE,
        &sentinel,
    );
    sync_json(&sandbox, &project);
    let files_stub = Sandbox::stubs_dir(&project).join("files.pyi");
    assert!(files_stub.exists());

    fs::remove_dir_all(project.join("plugins/files")).expect("remove plugin");
    let payload = sync_json(&sandbox, &project);
    assert_eq!(strings(&payload["removed"]), vec!["files.pyi"]);
    assert!(!files_stub.exists(), "obsolete stub must be removed");
    assert!(Sandbox::stubs_dir(&project).join("email.pyi").exists());
}

#[test]
fn local_override_replaces_global_plugin_entirely() {
    let sandbox = Sandbox::new();
    let (workspace, project) = sandbox.project();
    let sentinel = sandbox.0.join("imported.flag");
    write_plugin(
        &workspace,
        "email",
        &[("send", "sender:send"), ("legacy", "sender:send")],
        EMAIL_SOURCE,
        &sentinel,
    );
    write_plugin(
        &project,
        "email",
        &[("send", "sender:send")],
        EMAIL_SOURCE,
        &sentinel,
    );

    let payload = sync_json(&sandbox, &project);
    assert_eq!(strings(&payload["plugins"]), vec!["email"]);
    let stub = fs::read_to_string(Sandbox::stubs_dir(&project).join("email.pyi")).expect("stub");
    assert!(stub.contains("origem=project"), "{stub}");
    assert!(stub.contains("def send("));
    assert!(
        !stub.contains("def legacy("),
        "global methods must not be merged into the local plugin"
    );
    assert!(!sentinel.exists());
}

#[test]
fn global_only_plugin_is_exported_from_the_workspace() {
    let sandbox = Sandbox::new();
    let (workspace, project) = sandbox.project();
    let sentinel = sandbox.0.join("imported.flag");
    write_plugin(
        &workspace,
        "alerts",
        &[("fire", "sender:send")],
        EMAIL_SOURCE,
        &sentinel,
    );

    let payload = sync_json(&sandbox, &project);
    assert_eq!(strings(&payload["plugins"]), vec!["alerts"]);
    let stub = fs::read_to_string(Sandbox::stubs_dir(&project).join("alerts.pyi")).expect("stub");
    assert!(stub.contains("origem=global"));
    assert!(!sentinel.exists());
}
