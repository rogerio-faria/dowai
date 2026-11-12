//! ENT-056: permissões granulares por plugin, limite de saída e saneamento de logs.
//! Usa o `python3` do sistema, como os testes de ENT-107.

use dowai_core::plugin_registry::{discover, resolve_call, PluginRoots, PluginScope};
use dowai_core::{AuditLog, DowaiError};
use dowai_runtime::{execute_plugin, PluginExecution, PluginPolicy};
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use uuid::Uuid;

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("dowai-plugin-perm-{}", Uuid::new_v4()));
        fs::create_dir_all(root.join("project/plugins")).expect("project");
        fs::create_dir_all(root.join("state")).expect("state");
        Self { root }
    }

    /// Cria `plugins/<id>/` com permissões declaradas e o código de `main.py`.
    fn plugin(&self, id: &str, permissions: Value, source: &str) {
        let dir = self.root.join("project/plugins").join(id);
        fs::create_dir_all(&dir).expect("plugin dir");
        let manifest = json!({
            "schema_version": 2,
            "id": id,
            "version": "1.0.0",
            "entrypoints": { "run": "main:run" },
            "permissions": permissions,
            "timeout_seconds": 10,
            "side_effects": false,
            "idempotency_key_required": false,
        });
        fs::write(dir.join("plugin.json"), manifest.to_string()).expect("manifest");
        fs::write(dir.join("main.py"), source).expect("module");
    }

    fn audit_path(&self) -> PathBuf {
        self.root.join("state/audit.jsonl")
    }

    fn execute(&self, id: &str, input: Value, policy: &PluginPolicy) -> Result<Value, DowaiError> {
        let roots = PluginRoots {
            project: Some(self.root.join("project/plugins")),
            global: Some(self.root.join("workspace-none")),
        };
        let catalog = discover(&roots, PluginScope::All)?;
        let call = resolve_call(&catalog, &format!("{id}.run"), true)?;
        let audit = AuditLog::new(self.audit_path());
        execute_plugin(&PluginExecution {
            call: &call,
            interpreter: Path::new("python3"),
            input,
            idempotency_key: None,
            store: None,
            audit: Some(&audit),
            policy,
        })
    }

    fn file(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    fn audit_text(&self) -> String {
        fs::read_to_string(self.audit_path()).unwrap_or_default()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn policy(network: bool, filesystem: bool, secrets: bool) -> PluginPolicy {
    PluginPolicy {
        allow_network: network,
        allow_filesystem: filesystem,
        allow_secrets: secrets,
        ..PluginPolicy::default()
    }
}

fn no_permissions() -> Value {
    json!({ "network": [], "filesystem": [], "secrets": [] })
}

fn code(error: &DowaiError) -> &'static str {
    error.code()
}

#[test]
fn declared_network_is_refused_before_plugin_code_runs_when_policy_denies_it() {
    let fixture = Fixture::new();
    let marker = fixture.file("imported.flag");
    fixture.plugin(
        "mailer",
        json!({ "network": ["mail_gateway"], "filesystem": [], "secrets": [] }),
        &format!(
            "open({:?}, 'w').close()\ndef run(**kwargs):\n    return {{}}\n",
            marker.display().to_string()
        ),
    );

    let error = fixture
        .execute("mailer", json!({}), &PluginPolicy::default())
        .expect_err("network declared but not allowed by policy");

    assert_eq!(code(&error), "DOWAI_POLICY_DENIED");
    assert!(!marker.exists(), "plugin code must not run when refused");
    assert!(fixture.audit_text().contains("\"result\":\"REFUSED\""));
}

#[test]
fn declared_secrets_need_allow_secrets() {
    let fixture = Fixture::new();
    fixture.plugin(
        "vault",
        json!({ "network": [], "filesystem": [], "secrets": ["api_token"] }),
        "def run(**kwargs):\n    return {'ok': True}\n",
    );

    let refused = fixture
        .execute("vault", json!({}), &policy(false, false, false))
        .expect_err("secrets declared but not allowed");
    assert_eq!(code(&refused), "DOWAI_POLICY_DENIED");

    let allowed = fixture
        .execute("vault", json!({}), &policy(false, false, true))
        .expect("secrets allowed by policy");
    assert_eq!(allowed["result"], json!({ "ok": true }));
}

#[test]
fn undeclared_network_is_blocked_even_when_policy_allows_it() {
    let fixture = Fixture::new();
    fixture.plugin(
        "probe",
        no_permissions(),
        "import socket\n\
         def run(**kwargs):\n\
         \x20   s = socket.socket()\n\
         \x20   try:\n\
         \x20       s.connect(('127.0.0.1', 9))\n\
         \x20       return {'blocked': False}\n\
         \x20   except PermissionError:\n\
         \x20       return {'blocked': True}\n\
         \x20   except OSError:\n\
         \x20       return {'blocked': False}\n",
    );

    let result = fixture
        .execute("probe", json!({}), &policy(true, false, false))
        .expect("plugin runs; the connection is what gets blocked");

    assert_eq!(result["result"], json!({ "blocked": true }));
}

#[test]
fn undeclared_filesystem_write_is_blocked_and_declared_write_is_allowed() {
    let fixture = Fixture::new();
    let target = fixture.file("blocked.txt");
    let source = format!(
        "def run(**kwargs):\n    try:\n        open({:?}, 'w').write('x')\n        return {{'blocked': False}}\n    except PermissionError:\n        return {{'blocked': True}}\n",
        target.display().to_string()
    );
    fixture.plugin("writer", no_permissions(), &source);

    let result = fixture
        .execute("writer", json!({}), &policy(false, true, false))
        .expect("runs with filesystem policy allowed but not declared");
    assert_eq!(result["result"], json!({ "blocked": true }));
    assert!(
        !target.exists(),
        "undeclared write must not create the file"
    );

    let declared_target = fixture.file("allowed.txt");
    let declared_source = format!(
        "def run(**kwargs):\n    open({:?}, 'w').write('ok')\n    return {{'written': True}}\n",
        declared_target.display().to_string()
    );
    fixture.plugin(
        "declared_writer",
        json!({ "network": [], "filesystem": ["output_dir"], "secrets": [] }),
        &declared_source,
    );
    let result = fixture
        .execute("declared_writer", json!({}), &policy(false, true, false))
        .expect("declared and allowed write");
    assert_eq!(result["result"], json!({ "written": true }));
    assert!(declared_target.exists());
}

#[test]
fn stdout_result_is_bounded_by_the_configured_limit() {
    let fixture = Fixture::new();
    fixture.plugin(
        "big",
        no_permissions(),
        "def run(**kwargs):\n    return {'data': 'x' * 4000}\n",
    );

    let small = PluginPolicy {
        max_output_bytes: 1024,
        ..PluginPolicy::default()
    };
    let refused = fixture
        .execute("big", json!({}), &small)
        .expect_err("result above the configured limit");
    assert_eq!(code(&refused), "DOWAI_PLUGIN_OUTPUT_INVALID");

    let large = PluginPolicy {
        max_output_bytes: 65_536,
        ..PluginPolicy::default()
    };
    let accepted = fixture
        .execute("big", json!({}), &large)
        .expect("result within the configured limit");
    assert_eq!(
        accepted["result"]["data"].as_str().map(str::len),
        Some(4000)
    );
}

#[test]
fn secrets_never_reach_audit_or_errors_from_stdout_stderr_or_input() {
    let fixture = Fixture::new();
    fixture.plugin(
        "leaky",
        json!({ "network": [], "filesystem": [], "secrets": ["api_token"] }),
        "import sys\n\
         def run(**kwargs):\n\
         \x20   print('SEGREDO-STDOUT-7731')\n\
         \x20   sys.stderr.write('SEGREDO-STDERR-7731')\n\
         \x20   if kwargs.get('fail'):\n\
         \x20       raise ValueError('SEGREDO-EXCECAO-7731')\n\
         \x20   return {'ok': True}\n",
    );
    let allowed = policy(false, false, true);
    let input = json!({ "token": "SEGREDO-ENTRADA-7731" });

    let success = fixture
        .execute("leaky", input.clone(), &allowed)
        .expect("plugin succeeds");
    let success_text = success.to_string();
    assert!(!success_text.contains("SEGREDO-STDOUT-7731"));
    assert!(!success_text.contains("SEGREDO-STDERR-7731"));

    let failure = fixture
        .execute(
            "leaky",
            json!({ "fail": true, "token": "SEGREDO-ENTRADA-7731" }),
            &allowed,
        )
        .expect_err("plugin raises");
    let failure_text = failure.to_string();
    assert!(
        !failure_text.contains("SEGREDO"),
        "error leaked: {failure_text}"
    );

    let audit = fixture.audit_text();
    assert!(!audit.is_empty(), "audit must record the executions");
    for leaked in [
        "SEGREDO-STDOUT",
        "SEGREDO-STDERR",
        "SEGREDO-EXCECAO",
        "SEGREDO-ENTRADA",
    ] {
        assert!(!audit.contains(leaked), "audit leaked {leaked}");
    }
}

#[test]
fn plugins_cannot_spawn_processes_that_would_bypass_the_guards() {
    let fixture = Fixture::new();
    fixture.plugin(
        "spawner",
        json!({ "network": ["mail_gateway"], "filesystem": ["out"], "secrets": [] }),
        "import subprocess\n\
         def run(**kwargs):\n\
         \x20   try:\n\
         \x20       subprocess.run(['true'], check=True)\n\
         \x20       return {'spawned': True}\n\
         \x20   except PermissionError:\n\
         \x20       return {'spawned': False}\n",
    );
    let allow_all = policy(true, true, false);

    let result = fixture
        .execute("spawner", json!({}), &allow_all)
        .expect("plugin runs; spawning is what gets blocked");

    assert_eq!(result["result"], json!({ "spawned": false }));
}
