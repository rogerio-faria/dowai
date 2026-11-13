//! Token-based cancellation (plan 5.5, 6.4, 12.3 item 6; AC056). Deterministic and
//! cross-platform: no real signals are used here.

use dowai_core::{
    create_project, default_audit_log_path, default_run_store_path, AuditLog, CancellationToken,
    DowaiError, RunStore,
};
use dowai_postgres::{quote_identifier, PgProfile, PostgresAdapter, TlsMode};
use dowai_runtime::{run_postgres_with_cancel, RunRequest};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};
use uuid::Uuid;

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
        let root = std::env::temp_dir().join(format!("dowai-cancel-e2e-{}", Uuid::new_v4()));
        fs::create_dir_all(root.join("config")).expect("config dir");
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
        let project = create_project(&root, "cancel-fixture").expect("project");
        let environment = project.join(".venv");
        let created = std::process::Command::new("python3")
            .args(["-m", "venv", "--without-pip"])
            .arg(&environment)
            .output()
            .expect("venv");
        assert!(created.status.success());
        let manifest_path = project.join("dowai_project.json");
        let mut manifest: Value =
            serde_json::from_slice(&fs::read(&manifest_path).expect("read")).expect("json");
        manifest["python"] = json!({
            "interpreter": environment.join("bin").join("python"),
            "timeout_seconds": 60
        });
        manifest["config"]["security"]["allow_external_writes"] = json!(true);
        fs::write(&manifest_path, serde_json::to_vec(&manifest).expect("enc")).expect("write");
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

    fn request(&self, targets: &[&str]) -> RunRequest {
        RunRequest {
            targets: targets.iter().map(|target| (*target).to_owned()).collect(),
            mode: Some(dowai_core::ExecutionMode::Manual),
            profile: None,
            session_id: None,
            variables: BTreeMap::new(),
            confirm_destructive: true,
            full_refresh: false,
        }
    }

    fn audit_events(&self) -> Vec<dowai_core::AuditEvent> {
        AuditLog::for_user_state(default_audit_log_path().expect("audit path"))
            .read_all()
            .unwrap_or_default()
    }

    fn journal(&self) -> dowai_core::RunDetail {
        let store = RunStore::for_user_state(default_run_store_path().expect("path"));
        let runs = store.list(10).expect("list");
        assert_eq!(runs.len(), 1);
        store.get(&runs[0].run_id).expect("get")
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

fn assert_cancelled_audit(harness: &Harness) {
    let events = harness.audit_events();
    let finished: Vec<_> = events
        .iter()
        .filter(|e| e.event == "run.finished")
        .collect();
    assert_eq!(finished.len(), 1);
    assert_eq!(finished[0].result.as_deref(), Some("CANCELLED"));
    assert_eq!(finished[0].error_code.as_deref(), Some("DOWAI_CANCELLED"));
    assert!(
        events.iter().all(|e| e.event != "variables.published"),
        "cancellation must not publish variables"
    );
}

#[test]
fn token_cancelled_before_start_executes_nothing() {
    let harness = Harness::new();
    let marker = harness.root.join("marker");
    harness.model(
        "a.py",
        &format!(
            "def model(env):\n    open({:?}, 'w').write('x')\n",
            marker.to_string_lossy()
        ),
    );
    let token = CancellationToken::new();
    token.cancel();
    let error = run_postgres_with_cancel(&harness.project, harness.request(&["a.py"]), &token)
        .expect_err("cancelled run must fail");
    assert!(matches!(error, DowaiError::Cancelled(_)));
    assert_eq!(error.exit_code(), 130);
    assert!(!marker.exists(), "no model may execute");
    let detail = harness.journal();
    assert_eq!(detail.run.status, "CANCELLED");
    assert_eq!(detail.run.error_code.as_deref(), Some("DOWAI_CANCELLED"));
    assert_cancelled_audit(&harness);
}

#[test]
fn cancel_during_python_node_stops_worker_skips_later_nodes_and_publishes_nothing() {
    let harness = Harness::new();
    let started = harness.root.join("started");
    let later = harness.root.join("later");
    harness.model(
        "a.py",
        &format!(
            "import time\ndef model(env):\n    open({:?}, 'w').write('x')\n    time.sleep(30)\n    env.setVar('late', 1)\n",
            started.to_string_lossy()
        ),
    );
    harness.model(
        "b.py",
        &format!(
            "def model(env):\n    open({:?}, 'w').write('x')\n",
            later.to_string_lossy()
        ),
    );
    let token = CancellationToken::new();
    let trigger = token.clone();
    let watched = started.clone();
    let canceller = std::thread::spawn(move || {
        while !watched.exists() {
            std::thread::sleep(Duration::from_millis(10));
        }
        std::thread::sleep(Duration::from_millis(200));
        trigger.cancel();
    });
    let begin = Instant::now();
    let error =
        run_postgres_with_cancel(&harness.project, harness.request(&["a.py", "b.py"]), &token)
            .expect_err("cancelled run must fail");
    canceller.join().expect("canceller");
    assert!(begin.elapsed() < Duration::from_secs(10));
    assert!(matches!(error, DowaiError::Cancelled(_)), "{error}");
    assert!(!later.exists(), "later nodes must not run");
    let detail = harness.journal();
    assert_eq!(detail.run.status, "CANCELLED");
    let status_of = |target: &str| {
        detail
            .invocations
            .iter()
            .find(|entry| entry.target == target)
            .map(|entry| entry.status.clone())
    };
    assert_eq!(status_of("a.py").as_deref(), Some("CANCELLED"));
    assert_eq!(status_of("b.py").as_deref(), Some("SKIPPED"));
    assert_cancelled_audit(&harness);
}

// ---- PostgreSQL (opt-in) ----

fn test_profile() -> PgProfile {
    let mut profile = PgProfile::new(
        std::env::var("DOWAI_TEST_PG_HOST").expect("DOWAI_TEST_PG_HOST"),
        std::env::var("DOWAI_TEST_PG_PORT")
            .expect("port")
            .parse()
            .expect("port"),
        std::env::var("DOWAI_TEST_PG_USER").expect("user"),
        std::env::var("DOWAI_TEST_PG_DATABASE").expect("database"),
        "DOWAI_TEST_PG_PASSWORD",
    );
    if std::env::var("DOWAI_TEST_PG_TLS").as_deref() == Ok("disabled") {
        profile.set_tls_mode(TlsMode::Disabled);
    }
    profile
}

fn write_workspace_profile(root: &Path) {
    let config = json!({
        "schema_version": 2,
        "profiles": {"warehouse": {
            "type": "postgres",
            "host": std::env::var("DOWAI_TEST_PG_HOST").expect("host"),
            "port": std::env::var("DOWAI_TEST_PG_PORT").expect("port").parse::<u16>().expect("port"),
            "user": std::env::var("DOWAI_TEST_PG_USER").expect("user"),
            "database": std::env::var("DOWAI_TEST_PG_DATABASE").expect("database"),
            "password_env": "DOWAI_TEST_PG_PASSWORD",
            "tls": if std::env::var("DOWAI_TEST_PG_TLS").as_deref() == Ok("disabled") { "disabled" } else { "required" }
        }},
        "defaults": {"execution": {"default_profile": "warehouse"}}
    });
    fs::write(
        root.join("workspace.json"),
        serde_json::to_vec(&config).expect("enc"),
    )
    .expect("workspace.json");
}

#[test]
#[ignore = "opt-in E2E; requires a dedicated disposable PostgreSQL database"]
fn cancelled_sql_node_rolls_back_replacement_and_skips_later_nodes() {
    let harness = Harness::new();
    write_workspace_profile(&harness.root);
    let nonce = Uuid::new_v4().simple().to_string();
    let existing = format!("cancel_existing_{nonce}");
    let fresh = format!("cancel_fresh_{nonce}");
    let downstream = format!("cancel_down_{nonce}");
    let mut adapter = PostgresAdapter::connect(&test_profile()).expect("connect");
    let quoted = quote_identifier(&existing).expect("quote");
    adapter
        .execute(&format!("CREATE TABLE {quoted} AS SELECT 1 AS value"), &[])
        .expect("seed existing table");

    // Replaces `existing` with a slow query; `fresh` would be created after it.
    harness.model(
        &format!("{existing}.sql"),
        "SELECT pg_sleep(30)::text AS value",
    );
    harness.model(&format!("{fresh}.sql"), "SELECT 2 AS value");
    harness.model(
        &format!("{downstream}.sql"),
        &format!("SELECT * FROM {{{{ ref('{fresh}') }}}}"),
    );
    let token = CancellationToken::new();
    let trigger = token.clone();
    let canceller = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(1500));
        trigger.cancel();
    });
    let mut request = harness.request(&[&format!("{existing}.sql"), &format!("{fresh}.sql")]);
    request.mode = Some(dowai_core::ExecutionMode::Manual);
    let begin = Instant::now();
    let error = run_postgres_with_cancel(&harness.project, request, &token)
        .expect_err("cancelled run must fail");
    canceller.join().expect("canceller");
    assert!(
        begin.elapsed() < Duration::from_secs(10),
        "took {:?}",
        begin.elapsed()
    );
    assert!(matches!(error, DowaiError::Cancelled(_)), "{error}");

    let rows = adapter
        .query(&format!("SELECT value FROM {quoted}"), &[])
        .expect("original table must still exist");
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].get::<_, i32>(0),
        1,
        "replacement must be rolled back"
    );
    let fresh_quoted = quote_identifier(&fresh).expect("quote");
    assert!(
        adapter
            .query(&format!("SELECT 1 FROM {fresh_quoted}"), &[])
            .is_err(),
        "later node must not run"
    );
    let leftover = adapter
        .query(
            "SELECT 1 FROM pg_stat_activity WHERE query LIKE '%pg_sleep(30)%' AND pid <> pg_backend_pid() AND state = 'active'",
            &[],
        )
        .expect("activity");
    assert!(leftover.is_empty(), "server-side query must be cancelled");

    let detail = harness.journal();
    assert_eq!(detail.run.status, "CANCELLED");
    let status_of = |suffix: String| {
        detail
            .invocations
            .iter()
            .find(|entry| entry.target == suffix)
            .map(|entry| entry.status.clone())
    };
    assert_eq!(
        status_of(format!("{existing}.sql")).as_deref(),
        Some("CANCELLED")
    );
    assert_eq!(
        status_of(format!("{fresh}.sql")).as_deref(),
        Some("SKIPPED")
    );
    assert_cancelled_audit(&harness);
    assert!(
        harness
            .audit_events()
            .iter()
            .all(|e| e.event != "external.write"),
        "no external.write may be audited"
    );
    let _ = adapter.drop_relation(&existing, true);
}
