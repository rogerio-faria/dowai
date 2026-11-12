//! ENT-107: execução de plugin em subprocesso, com timeout sem órfãos, AUDIT e idempotência pela
//! outbox. Usa o interpretador `python3` do sistema; o runtime recebe o interpretador por parâmetro.

use dowai_core::plugin_registry::{discover, resolve_call, PluginRoots, PluginScope};
use dowai_core::state::OutboxStatus;
use dowai_core::{AuditLog, DowaiError, RunStore};
use dowai_runtime::{execute_plugin, PluginExecution, PluginPolicy};
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use uuid::Uuid;

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("dowai-plugin-exec-{}", Uuid::new_v4()));
        fs::create_dir_all(root.join("project")).expect("project");
        fs::create_dir_all(root.join("workspace")).expect("workspace");
        Self { root }
    }

    fn plugin_dir(&self, scope: &str, id: &str) -> PathBuf {
        self.root.join(scope).join("plugins").join(id)
    }

    /// Escreve `plugin.json` e `main.py` com `source`; `side_effects` exige a chave.
    fn plugin(&self, scope: &str, id: &str, timeout: u64, required: bool, source: &str) {
        let dir = self.plugin_dir(scope, id);
        fs::create_dir_all(&dir).expect("plugin dir");
        let manifest = json!({
            "schema_version": 2,
            "id": id,
            "version": if scope == "project" { "2.0.0" } else { "1.0.0" },
            "entrypoints": { "send": "main:send" },
            "permissions": { "network": [], "filesystem": ["effects"], "secrets": [] },
            "timeout_seconds": timeout,
            "side_effects": required,
            "idempotency_key_required": required,
        });
        fs::write(dir.join("plugin.json"), manifest.to_string()).expect("manifest");
        fs::write(dir.join("main.py"), source).expect("module");
    }

    fn store(&self) -> RunStore {
        RunStore::new(self.root.join("state/runs.sqlite3"))
    }

    fn audit(&self) -> AuditLog {
        AuditLog::new(self.root.join("state/audit.jsonl"))
    }

    fn run(&self, input: Value, key: Option<&str>) -> Result<Value, DowaiError> {
        self.run_target("email.send", input, key)
    }

    fn run_target(
        &self,
        target: &str,
        input: Value,
        key: Option<&str>,
    ) -> Result<Value, DowaiError> {
        let roots = PluginRoots {
            project: Some(self.root.join("project/plugins")),
            global: Some(self.root.join("workspace/plugins")),
        };
        let catalog = discover(&roots, PluginScope::All)?;
        let call = resolve_call(&catalog, target, true)?;
        let store = self.store();
        let audit = self.audit();
        execute_plugin(&PluginExecution {
            call: &call,
            interpreter: Path::new("python3"),
            input,
            idempotency_key: key,
            store: Some(&store),
            audit: Some(&audit),
            policy: &PluginPolicy {
                allow_filesystem: true,
                ..PluginPolicy::default()
            },
        })
    }

    fn counter(&self) -> String {
        fs::read_to_string(self.root.join("effects.log")).unwrap_or_default()
    }

    fn effects_path(&self) -> String {
        self.root.join("effects.log").display().to_string()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn code(error: &DowaiError) -> &'static str {
    error.code()
}

fn process_alive(pid: u32) -> bool {
    match fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => stat
            .rsplit(')')
            .next()
            .map(|rest| !rest.trim_start().starts_with('Z'))
            .unwrap_or(false),
        Err(_) => false,
    }
}

const ECHO: &str = "def send(**kwargs):\n    return {'echo': kwargs}\n";

#[test]
fn executes_the_entrypoint_with_json_input_and_output() {
    let fixture = Fixture::new();
    fixture.plugin("project", "email", 30, false, ECHO);

    let result = fixture
        .run(json!({"to": "a@example.test", "n": 3}), None)
        .expect("run");

    assert_eq!(result["status"], "SUCCEEDED");
    assert_eq!(result["target"], "email.send");
    assert_eq!(
        result["result"],
        json!({"echo": {"to": "a@example.test", "n": 3}})
    );
    assert_eq!(result["audit_recorded"], true);
    assert!(result["override"].is_null());
}

#[test]
fn unresolved_target_never_executes_plugin_code() {
    let fixture = Fixture::new();
    let sentinel = fixture.effects_path();
    fixture.plugin(
        "project",
        "email",
        30,
        false,
        &format!("open({sentinel:?}, 'a').write('x')\ndef send(**kw):\n    return 1\n"),
    );

    for target in ["email.delete", "missing.send", "email"] {
        let error = fixture
            .run_target(target, json!({}), None)
            .expect_err("must be refused");
        assert_eq!(code(&error), "DOWAI_CONFIG_INVALID", "{target}");
    }
    assert_eq!(
        fixture.counter(),
        "",
        "plugin module must never be imported"
    );
    assert!(
        fixture.audit().read_all().expect("audit").is_empty(),
        "nothing was executed, so nothing is audited as an execution"
    );
}

#[test]
fn audit_records_plugin_version_manifest_hash_and_the_replaced_plugin() {
    let fixture = Fixture::new();
    fixture.plugin("workspace", "email", 30, false, ECHO);
    fixture.plugin("project", "email", 30, false, ECHO);
    let local = dowai_core::plugin_manifest::read_manifest(&fixture.plugin_dir("project", "email"))
        .expect("local manifest");
    let global =
        dowai_core::plugin_manifest::read_manifest(&fixture.plugin_dir("workspace", "email"))
            .expect("global manifest");

    let result = fixture.run(json!({}), None).expect("run");
    assert_eq!(result["override"]["replaced_origin"], "global");

    let events = fixture.audit().read_all().expect("audit");
    assert_eq!(events.len(), 1);
    let event = &events[0];
    assert_eq!(event.event, "plugin.executed");
    assert_eq!(event.plugin_id.as_deref(), Some("email"));
    assert_eq!(event.plugin_method.as_deref(), Some("send"));
    assert_eq!(event.plugin_version.as_deref(), Some("2.0.0"));
    assert_eq!(
        event.manifest_sha256.as_deref(),
        Some(local.manifest_sha256.as_str())
    );
    assert_eq!(event.replaced_origin.as_deref(), Some("global"));
    assert_eq!(event.replaced_plugin_version.as_deref(), Some("1.0.0"));
    assert_eq!(
        event.replaced_manifest_sha256.as_deref(),
        Some(global.manifest_sha256.as_str())
    );
    assert_eq!(event.result.as_deref(), Some("SUCCEEDED"));
    assert!(event.error_code.is_none());
}

#[test]
fn audit_without_override_has_no_replaced_fields_and_failures_are_audited() {
    let fixture = Fixture::new();
    fixture.plugin(
        "project",
        "email",
        30,
        false,
        "def send(**kw):\n    raise RuntimeError('segredo-livre')\n",
    );

    let error = fixture.run(json!({}), None).expect_err("must fail");
    assert_eq!(code(&error), "DOWAI_PLUGIN_FAILED");
    assert!(!error.message().contains("segredo-livre"));

    let events = fixture.audit().read_all().expect("audit");
    assert_eq!(events.len(), 1);
    assert!(events[0].replaced_origin.is_none());
    assert_eq!(events[0].result.as_deref(), Some("FAILED"));
    assert_eq!(events[0].error_code.as_deref(), Some("DOWAI_PLUGIN_FAILED"));
    assert!(events[0].manifest_sha256.is_some());
}

#[test]
fn timeout_kills_the_process_group_and_leaves_no_orphan() {
    let fixture = Fixture::new();
    let pid_file = fixture.root.join("child.pid");
    fixture.plugin(
        "project",
        "email",
        1,
        false,
        &format!(
            "import os, time\n\
             def send(**kw):\n    \
                 child = os.fork()\n    \
                 if child == 0:\n        time.sleep(60)\n        os._exit(0)\n    \
                 open({:?}, 'w').write(str(child))\n    \
                 time.sleep(60)\n",
            pid_file.display().to_string()
        ),
    );

    let started = Instant::now();
    let error = fixture.run(json!({}), None).expect_err("must time out");
    assert_eq!(code(&error), "DOWAI_PLUGIN_TIMEOUT");
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "returned after {:?}",
        started.elapsed()
    );

    let pid: u32 = fs::read_to_string(&pid_file)
        .expect("grandchild pid")
        .parse()
        .expect("pid");
    let deadline = Instant::now() + Duration::from_secs(5);
    while process_alive(pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!process_alive(pid), "orphan process {pid} survived");

    let events = fixture.audit().read_all().expect("audit");
    assert_eq!(
        events[0].error_code.as_deref(),
        Some("DOWAI_PLUGIN_TIMEOUT")
    );
}

#[test]
fn descendants_left_behind_by_a_finished_plugin_are_stopped() {
    let fixture = Fixture::new();
    let pid_file = fixture.root.join("bg.pid");
    fixture.plugin(
        "project",
        "email",
        30,
        false,
        &format!(
            "import os, time\n\
             def send(**kw):\n    \
                 child = os.fork()\n    \
                 if child == 0:\n        time.sleep(60)\n        os._exit(0)\n    \
                 open({:?}, 'w').write(str(child))\n    \
                 return 1\n",
            pid_file.display().to_string()
        ),
    );

    fixture.run(json!({}), None).expect("run");
    let pid: u32 = fs::read_to_string(&pid_file)
        .expect("pid")
        .parse()
        .expect("pid");
    let deadline = Instant::now() + Duration::from_secs(5);
    while process_alive(pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!process_alive(pid), "background process {pid} survived");
}

#[test]
fn invalid_output_becomes_a_stable_error_without_echoing_plugin_content() {
    let fixture = Fixture::new();
    fixture.plugin(
        "project",
        "email",
        30,
        false,
        "def send(**kw):\n    return {1, 2}\n",
    );
    let error = fixture.run(json!({}), None).expect_err("not serializable");
    assert_eq!(code(&error), "DOWAI_PLUGIN_OUTPUT_INVALID");

    // Saída que não é JSON: o plugin escreve no descritor 1 diretamente.
    fixture.plugin(
        "project",
        "email",
        30,
        false,
        "import os\ndef send(**kw):\n    os.write(1, b'SEGREDO-LIVRE nao json')\n    return 1\n",
    );
    let error = fixture.run(json!({}), None).expect_err("not JSON");
    assert_eq!(code(&error), "DOWAI_PLUGIN_OUTPUT_INVALID");
    assert!(!error.message().contains("SEGREDO-LIVRE"));
}

#[test]
fn oversized_output_is_bounded_and_refused() {
    let fixture = Fixture::new();
    fixture.plugin(
        "project",
        "email",
        30,
        false,
        "def send(**kw):\n    return 'x' * (8 * 1024 * 1024)\n",
    );
    let error = fixture.run(json!({}), None).expect_err("oversized");
    assert_eq!(code(&error), "DOWAI_PLUGIN_OUTPUT_INVALID");
}

#[test]
fn input_must_be_a_json_object() {
    let fixture = Fixture::new();
    let sentinel = fixture.effects_path();
    fixture.plugin(
        "project",
        "email",
        30,
        false,
        &format!("open({sentinel:?}, 'a').write('x')\ndef send(**kw):\n    return 1\n"),
    );
    for input in [json!([1]), json!("texto"), Value::Null] {
        let error = fixture.run(input, None).expect_err("refused");
        assert_eq!(code(&error), "DOWAI_PLUGIN_INPUT_INVALID");
    }
    assert_eq!(fixture.counter(), "");
}

#[test]
fn entrypoint_module_escaping_the_plugin_directory_is_refused_without_running() {
    let fixture = Fixture::new();
    let sentinel = fixture.effects_path();
    fixture.plugin("project", "email", 30, false, ECHO);
    let outside = fixture.root.join("outside.py");
    fs::write(
        &outside,
        format!("open({sentinel:?}, 'a').write('x')\ndef send(**kw):\n    return 1\n"),
    )
    .expect("outside module");
    let module = fixture.plugin_dir("project", "email").join("main.py");
    fs::remove_file(&module).expect("remove module");
    std::os::unix::fs::symlink(&outside, &module).expect("symlink");

    let error = fixture.run(json!({}), None).expect_err("escape refused");
    assert_eq!(code(&error), "DOWAI_POLICY_PATH");
    assert_eq!(fixture.counter(), "");
}

#[test]
fn manifest_requiring_a_key_refuses_execution_without_one() {
    let fixture = Fixture::new();
    let sentinel = fixture.effects_path();
    fixture.plugin(
        "project",
        "email",
        30,
        true,
        &format!("open({sentinel:?}, 'a').write('x')\ndef send(**kw):\n    return 1\n"),
    );

    let error = fixture.run(json!({}), None).expect_err("key required");
    assert_eq!(code(&error), "DOWAI_PLUGIN_IDEMPOTENCY_KEY_REQUIRED");
    assert_eq!(error.exit_code(), 2);
    assert_eq!(fixture.counter(), "", "no code runs without the key");
    let events = fixture.audit().read_all().expect("audit");
    assert_eq!(events.len(), 1, "a refused execution is audited too");
    assert_eq!(events[0].result.as_deref(), Some("REFUSED"));

    let bad = fixture
        .run(json!({}), Some("has space"))
        .expect_err("bad key");
    assert_eq!(code(&bad), "DOWAI_PLUGIN_INPUT_INVALID");
    assert_eq!(fixture.counter(), "");
}

#[test]
fn same_key_does_not_repeat_the_external_effect() {
    let fixture = Fixture::new();
    let sentinel = fixture.effects_path();
    fixture.plugin(
        "project",
        "email",
        30,
        true,
        &format!("def send(**kw):\n    open({sentinel:?}, 'a').write('x')\n    return {{'sent': True}}\n"),
    );

    let first = fixture.run(json!({}), Some("alert-1")).expect("first");
    assert_eq!(first["status"], "SUCCEEDED");
    assert_eq!(first["result"], json!({"sent": true}));
    let second = fixture.run(json!({}), Some("alert-1")).expect("second");
    assert_eq!(second["status"], "DUPLICATE");
    assert!(second["result"].is_null());
    assert_eq!(second["idempotency_key"], "alert-1");
    assert_eq!(fixture.counter(), "x", "the effect ran exactly once");

    let other = fixture.run(json!({}), Some("alert-2")).expect("other key");
    assert_eq!(other["status"], "SUCCEEDED");
    assert_eq!(fixture.counter(), "xx");

    let confirmed = fixture
        .store()
        .outbox_list(OutboxStatus::Confirmed)
        .expect("outbox");
    assert_eq!(confirmed.len(), 2);
    let events = fixture.audit().read_all().expect("audit");
    let results: Vec<_> = events.iter().filter_map(|e| e.result.as_deref()).collect();
    assert_eq!(results, ["SUCCEEDED", "DUPLICATE", "SUCCEEDED"]);
}

#[test]
fn failed_and_timed_out_keys_are_never_executed_again() {
    let fixture = Fixture::new();
    let sentinel = fixture.effects_path();
    fixture.plugin(
        "project",
        "email",
        30,
        true,
        &format!("def send(**kw):\n    open({sentinel:?}, 'a').write('x')\n    raise ValueError('boom')\n"),
    );
    let error = fixture.run(json!({}), Some("k-fail")).expect_err("fails");
    assert_eq!(code(&error), "DOWAI_PLUGIN_FAILED");
    assert_eq!(
        fixture
            .store()
            .outbox_list(OutboxStatus::Failed)
            .expect("list")
            .len(),
        1
    );
    let again = fixture.run(json!({}), Some("k-fail")).expect_err("refused");
    assert_eq!(code(&again), "DOWAI_PLUGIN_IDEMPOTENCY_FAILED");
    assert_eq!(fixture.counter(), "x");

    fixture.plugin(
        "project",
        "email",
        1,
        true,
        &format!("import time\ndef send(**kw):\n    open({sentinel:?}, 'a').write('t')\n    time.sleep(60)\n"),
    );
    let error = fixture.run(json!({}), Some("k-slow")).expect_err("timeout");
    assert_eq!(code(&error), "DOWAI_PLUGIN_TIMEOUT");
    assert_eq!(
        fixture
            .store()
            .outbox_list(OutboxStatus::Uncertain)
            .expect("list")
            .len(),
        1,
        "a killed plugin may have applied the effect"
    );
    let again = fixture.run(json!({}), Some("k-slow")).expect_err("refused");
    assert_eq!(code(&again), "DOWAI_PLUGIN_IDEMPOTENCY_UNCERTAIN");
    assert_eq!(fixture.counter(), "xt");
}

#[test]
fn key_is_ignored_by_plugins_that_neither_require_it_nor_have_side_effects() {
    let fixture = Fixture::new();
    let sentinel = fixture.effects_path();
    fixture.plugin(
        "project",
        "email",
        30,
        false,
        &format!("def send(**kw):\n    open({sentinel:?}, 'a').write('x')\n    return 1\n"),
    );
    fixture.run(json!({}), Some("same")).expect("first");
    let second = fixture.run(json!({}), Some("same")).expect("second");
    assert_eq!(second["status"], "SUCCEEDED");
    assert_eq!(fixture.counter(), "xx");
}
