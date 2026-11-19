//! AUDIT trail (plan 12.2, 12.3 item 9, 3.4, 20; P21): a stream separate from logs,
//! recorded for every run without values, SQL, credentials, free text or absolute paths.

use dowai_core::{
    create_project, current_time_ms, default_audit_log_path, default_run_store_path, AuditEvent,
    AuditLog, RunStore,
};
#[cfg(unix)]
use dowai_core::{LogPolicy, OperationalLog};
use dowai_runtime::{run_postgres, RunRequest};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Mutex, MutexGuard};
use uuid::Uuid;

const SENTINEL_VARIABLE: &str = "SENTINEL_VAR_VALUE_77aa";
const SENTINEL_SQL: &str = "SENTINEL_SQL_TABLE_55bb";
const SENTINEL_PASSWORD: &str = "SENTINEL_PASSWORD_99cc";
const SENTINEL_ERROR: &str = "SENTINEL_PRIVATE_DETAIL_33dd";

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
        let python = if cfg!(windows) {
            let output = Command::new("python")
                .args([
                    "-c",
                    "import sys; sys.stdout.buffer.write(sys.executable.encode('utf-8'))",
                ])
                .output()
                .expect("locate Python");
            assert!(output.status.success(), "locate Python failed");
            String::from_utf8(output.stdout)
                .expect("Python path is UTF-8")
                .trim()
                .to_owned()
        } else {
            "python3".to_owned()
        };
        let root = std::env::temp_dir().join(format!("dowai-audit-e2e-{}", Uuid::new_v4()));
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
        restore.push((
            "DOWAI_AUDIT_TEST_PASSWORD",
            std::env::var_os("DOWAI_AUDIT_TEST_PASSWORD"),
        ));
        std::env::set_var("DOWAI_AUDIT_TEST_PASSWORD", SENTINEL_PASSWORD);
        let project = create_project(&root, "audit-fixture").expect("create project");
        let environment = project.join(".venv");
        let created = Command::new(python)
            .args(["-m", "venv", "--without-pip"])
            .arg(&environment)
            .output()
            .expect("create virtual environment");
        assert!(created.status.success(), "venv creation must succeed");
        let manifest_path = project.join("dowai_project.json");
        let mut manifest: Value =
            serde_json::from_slice(&fs::read(&manifest_path).expect("read manifest"))
                .expect("manifest JSON");
        let interpreter = if cfg!(windows) {
            environment.join("Scripts").join("python.exe")
        } else {
            environment.join("bin").join("python")
        };
        manifest["python"] = json!({
            "interpreter": interpreter,
            "timeout_seconds": 30
        });
        manifest["config"]["security"]["allow_external_writes"] = json!(true);
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

    fn run_with(
        &self,
        target: &str,
        session_id: Option<String>,
        variables: BTreeMap<String, Value>,
    ) -> dowai_core::Result<dowai_runtime::RunResult> {
        run_postgres(
            &self.project,
            RunRequest {
                targets: vec![target.to_owned()],
                mode: None,
                profile: None,
                session_id,
                variables,
                confirm_destructive: false,
                full_refresh: false,
            },
        )
    }

    fn run(&self, target: &str) -> dowai_core::Result<dowai_runtime::RunResult> {
        self.run_with(target, None, BTreeMap::new())
    }

    fn events(&self) -> Vec<AuditEvent> {
        AuditLog::for_user_state(default_audit_log_path().expect("isolated path"))
            .read_all()
            .expect("read audit")
    }

    fn raw(&self) -> String {
        fs::read_to_string(default_audit_log_path().expect("isolated path")).unwrap_or_default()
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

fn names(events: &[AuditEvent]) -> Vec<&str> {
    events.iter().map(|event| event.event.as_str()).collect()
}

#[test]
fn successful_run_emits_ordered_events_with_one_run_id() {
    let harness = Harness::new();
    harness.model("a.py", "def model(env):\n    env.setVar('stage', 'x')\n");
    harness.model("b.py", "def model(env):\n    env.depends_on('a')\n");
    let result = harness.run("b.py").expect("run succeeds");

    let events = harness.events();
    assert_eq!(
        names(&events),
        [
            "run.started",
            "node.started",
            "node.finished",
            "node.started",
            "node.finished",
            "variables.published",
            "run.finished"
        ]
    );
    assert!(events.iter().all(|event| event.run_id == result.run_id));
    assert!(events.iter().all(|event| event.schema_version == 1
        && event.engine_version == env!("CARGO_PKG_VERSION")
        && !event.actor.is_empty()
        && event.timestamp.ends_with('Z')));
    let timestamps: Vec<_> = events.iter().map(|e| e.timestamp.clone()).collect();
    let mut sorted = timestamps.clone();
    sorted.sort();
    assert_eq!(timestamps, sorted, "timestamps never go backwards");

    let started = &events[0];
    assert_eq!(started.project.as_deref(), Some("audit-fixture"));
    assert_eq!(started.mode.as_deref(), Some("automatic"));
    assert_eq!(
        started.invocation_id.as_deref(),
        Some(result.invocation_id.as_str())
    );
    assert_eq!(started.config_hash.as_ref().map(String::len), Some(64));
    assert_eq!(started.plan_hash.as_ref().map(String::len), Some(64));
    assert_eq!(
        started.source_profile, None,
        "Python-only runs have no profile"
    );

    // Node events carry node id, model hash and an attempt id (= invocation id).
    let node_started = &events[1];
    assert_eq!(node_started.node_id.as_deref(), Some("a.py"));
    assert_eq!(node_started.script_hash.as_ref().map(String::len), Some(64));
    assert_eq!(node_started.attempt_id, node_started.invocation_id);
    assert_eq!(events[1].invocation_id, events[2].invocation_id);
    assert_eq!(events[2].result.as_deref(), Some("SUCCEEDED"));
    assert_eq!(events[3].node_id.as_deref(), Some("b.py"));

    let published = &events[5];
    assert_eq!(
        published.variables.as_deref(),
        Some(&["stage".to_owned()][..])
    );
    let finished = events.last().expect("last");
    assert_eq!(finished.result.as_deref(), Some("SUCCEEDED"));
    assert_eq!(finished.error_code, None);
    assert_eq!(
        finished.session_id.as_deref(),
        Some(result.session_id.as_str())
    );
}

#[test]
fn failed_run_emits_run_finished_failed_with_error_code() {
    let harness = Harness::new();
    harness.model(
        "boom.py",
        &format!("def model(env):\n    raise RuntimeError('{SENTINEL_ERROR}')\n"),
    );
    let error = harness.run("boom.py").expect_err("must fail");
    assert_eq!(error.code(), "DOWAI_PYTHON_FAILED");

    let events = harness.events();
    assert_eq!(
        names(&events),
        [
            "run.started",
            "node.started",
            "node.finished",
            "run.finished"
        ]
    );
    assert_eq!(events[2].result.as_deref(), Some("FAILED"));
    assert_eq!(events[2].error_code.as_deref(), Some("DOWAI_PYTHON_FAILED"));
    let finished = &events[3];
    assert_eq!(finished.result.as_deref(), Some("FAILED"));
    assert_eq!(finished.error_code.as_deref(), Some("DOWAI_PYTHON_FAILED"));
    assert!(!harness.raw().contains(SENTINEL_ERROR));
}

#[test]
fn failure_before_planning_still_has_started_and_finished() {
    let harness = Harness::new();
    let error = harness.run("missing.py").expect_err("unknown model");
    let events = harness.events();
    assert_eq!(names(&events), ["run.started", "run.finished"]);
    assert_eq!(events[1].error_code.as_deref(), Some(error.code()));
}

#[test]
fn events_share_run_and_session_ids_when_a_session_exists() {
    let harness = Harness::new();
    harness.model("a.py", "def model(env):\n    env.setVar('k', 1)\n");
    let first = harness.run("a.py").expect("first run");
    let second = harness
        .run_with("a.py", Some(first.session_id.clone()), BTreeMap::new())
        .expect("second run");
    let events = harness.events();
    let second_events: Vec<_> = events
        .iter()
        .filter(|e| e.run_id == second.run_id)
        .collect();
    assert_eq!(second_events.len(), 5);
    assert!(second_events
        .iter()
        .all(|event| event.session_id.as_deref() == Some(first.session_id.as_str())));
    assert_ne!(first.run_id, second.run_id);
}

#[test]
fn nested_calls_emit_python_invocation_with_parent_and_call_type() {
    let harness = Harness::new();
    harness.model("child.py", "def model(env):\n    pass\n");
    harness.model("parent.py", "def model(env):\n    env.run('child.py')\n");
    let result = harness.run("parent.py").expect("run");
    let events = harness.events();
    let nested: Vec<_> = events
        .iter()
        .filter(|e| e.event == "python.invocation")
        .collect();
    assert_eq!(nested.len(), 1);
    let nested = nested[0];
    assert_eq!(nested.call_type.as_deref(), Some("imperative"));
    assert_eq!(nested.node_id.as_deref(), Some("child.py"));
    assert!(nested.invocation_id.is_some());
    let parent_started = events
        .iter()
        .find(|e| e.event == "node.started" && e.node_id.as_deref() == Some("parent.py"))
        .expect("parent node");
    assert_eq!(nested.parent_invocation_id, parent_started.invocation_id);
    assert_eq!(nested.run_id, result.run_id);
}

#[test]
fn sentinels_never_reach_the_audit_file() {
    let harness = Harness::new();
    harness.model(
        "a.py",
        &format!(
            "def model(env):\n    env.setVar('{SENTINEL_VARIABLE}_name_free', '{SENTINEL_VARIABLE}')\n    env.setVar('other', env.getVar('input'))\n"
        ),
    );
    harness
        .run_with(
            "a.py",
            None,
            BTreeMap::from([("input".to_owned(), json!(SENTINEL_VARIABLE))]),
        )
        .expect("python run");

    // SQL run denied by policy, with a sentinel in the SQL text and in the profile password.
    let manifest_path = harness.project.join("dowai_project.json");
    let mut manifest: Value =
        serde_json::from_slice(&fs::read(&manifest_path).expect("read")).expect("JSON");
    manifest["config"]["security"]["allow_external_writes"] = json!(false);
    fs::write(
        &manifest_path,
        serde_json::to_vec(&manifest).expect("encode"),
    )
    .expect("write");
    harness.model("q.sql", &format!("select 1 as {SENTINEL_SQL}"));
    fs::write(
        harness.root.join("profiles.json"),
        serde_json::to_vec(&json!({
            "schema_version": 2,
            "profiles": {"wh": {
                "type": "postgres", "host": "127.0.0.1", "port": 5432, "user": "u",
                "database": "d", "password": "{{ env_var('DOWAI_AUDIT_TEST_PASSWORD') }}", "tls": "disabled"
            }}
        }))
        .expect("encode"),
    )
    .expect("workspace");
    let error = harness
        .run_with("q.sql", None, BTreeMap::new())
        .expect_err("external writes are disabled");
    assert_eq!(error.code(), "DOWAI_POLICY_DENIED");

    let raw = harness.raw();
    assert!(!raw.is_empty());
    for sentinel in [SENTINEL_SQL, SENTINEL_PASSWORD] {
        assert!(!raw.contains(sentinel), "{sentinel} leaked into audit");
    }
    // Variable values never appear; only names (the value sentinel is not a name).
    let value_sentinel_hits = raw.matches(SENTINEL_VARIABLE).count();
    let name_hits = raw
        .matches(&format!("{SENTINEL_VARIABLE}_name_free"))
        .count();
    assert_eq!(value_sentinel_hits, name_hits, "value leaked: {raw}");
    let root = harness.root.to_string_lossy().into_owned();
    assert!(!raw.contains(&root), "absolute path leaked into audit");
    assert!(!raw.contains("127.0.0.1"), "host leaked into audit");

    let denied: Vec<_> = harness
        .events()
        .into_iter()
        .filter(|e| e.event == "policy.denied")
        .collect();
    assert_eq!(denied.len(), 1);
    assert_eq!(denied[0].error_code.as_deref(), Some("DOWAI_POLICY_DENIED"));
}

#[test]
fn audit_is_recorded_without_any_log_configuration() {
    // No log level, log file or verbosity is configured anywhere in this harness.
    let harness = Harness::new();
    harness.model("a.py", "def model(env):\n    pass\n");
    assert!(harness.events().is_empty());
    harness.run("a.py").expect("run");
    assert!(!harness.events().is_empty());
}

#[cfg(unix)]
#[test]
fn audit_write_failure_does_not_mask_the_run_outcome() {
    let harness = Harness::new();
    harness.model("a.py", "def model(env):\n    pass\n");
    let path = default_audit_log_path().expect("path");
    fs::create_dir_all(path.parent().expect("parent")).expect("state dir");
    let target = harness.root.join("elsewhere.jsonl");
    fs::write(&target, "").expect("target");
    std::os::unix::fs::symlink(&target, &path).expect("symlink");

    let result = harness.run("a.py").expect("run still succeeds");
    assert_eq!(result.status, "SUCCEEDED");
    assert!(result.warnings.iter().any(|w| w.contains("audit")));
    assert_eq!(fs::read_to_string(&target).expect("target"), "");

    harness.model("boom.py", "def model(env):\n    raise RuntimeError('x')\n");
    let error = harness
        .run("boom.py")
        .expect_err("original error preserved");
    assert_eq!(error.code(), "DOWAI_PYTHON_FAILED");
}

#[test]
fn node_invocation_and_run_events_carry_durations() {
    let harness = Harness::new();
    harness.model("a.py", "def model(env):\n    env.setVar('stage', 'x')\n");
    harness.run("a.py").expect("run succeeds");

    let events = harness.events();
    let node_finished: Vec<&AuditEvent> = events
        .iter()
        .filter(|event| event.event == "node.finished")
        .collect();
    assert_eq!(node_finished.len(), 1, "one static node finished");
    assert!(
        node_finished[0].duration_ms.is_some(),
        "node.finished must carry duration_ms"
    );

    let run_finished = events
        .iter()
        .find(|event| event.event == "run.finished")
        .expect("run.finished present");
    assert!(
        run_finished.duration_ms.is_some(),
        "run.finished must carry duration_ms"
    );
    assert!(run_finished.duration_ms >= node_finished[0].duration_ms);
    assert!(harness.raw().contains("\"duration_ms\""));
}

#[test]
fn finished_run_is_journaled_and_leaves_no_in_flight_marker() {
    // ENT-062: a completed run removes its start marker in the same commit as its record,
    // so startup recovery finds nothing to repair.
    let harness = Harness::new();
    harness.model("a.py", "def model(env):\n    env.setVar('stage', 'x')\n");
    let result = harness.run("a.py").expect("run succeeds");

    let journal = RunStore::for_user_state(default_run_store_path().expect("isolated path"));
    let recovery = journal
        .recover_interrupted(current_time_ms())
        .expect("recover");
    assert!(
        recovery.interrupted_runs.is_empty(),
        "a finished run must not leave an in-flight marker"
    );
    let runs = journal.list(10).expect("list");
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].run_id, result.run_id);
    assert_eq!(runs[0].status, "SUCCEEDED");
}

/// ENT-065: every AUDIT event is mirrored to `logs/dowai.jsonl` in the same order, with
/// owner-only permissions and no variable values or secrets in the file.
#[cfg(unix)]
#[test]
fn operational_log_mirrors_audit_events_privately() {
    use std::os::unix::fs::PermissionsExt;
    let harness = Harness::new();
    harness.model(
        "a.py",
        &format!(
            "def model(env):\n    env.setVar('stage', '{SENTINEL_VARIABLE}')\n    raise RuntimeError('{SENTINEL_ERROR}')\n"
        ),
    );
    let _ = harness.run("a.py");

    let audit_events = harness.events();
    let audit_names = names(&audit_events);
    let directory = default_audit_log_path()
        .expect("isolated path")
        .with_file_name("logs");
    let path = directory.join("dowai.jsonl");
    let records = OperationalLog::new(&path, LogPolicy::default())
        .read_all()
        .expect("read operational log");
    let operational_names: Vec<&str> = records.iter().map(|r| r.event.as_str()).collect();
    assert_eq!(operational_names, audit_names);
    assert!(records.iter().any(|r| r.error_code.is_some()));

    let raw = fs::read_to_string(&path).expect("raw operational log");
    for sentinel in [SENTINEL_VARIABLE, SENTINEL_PASSWORD, SENTINEL_ERROR] {
        assert!(
            !raw.contains(sentinel),
            "{sentinel} leaked into operational log"
        );
    }
    let file_mode = fs::metadata(&path).expect("file").permissions().mode();
    let dir_mode = fs::metadata(&directory).expect("dir").permissions().mode();
    assert_eq!(file_mode & 0o777, 0o600);
    assert_eq!(dir_mode & 0o777, 0o700);
}
