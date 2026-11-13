//! ENT-037: real transient PostgreSQL failure. A second connection holds an `ACCESS EXCLUSIVE`
//! lock on a declared source for 7 s. The materialization waits on that lock until the profile's
//! `statement_timeout` (`database.query_timeout_seconds` = 3 s) fires (SQLSTATE 57014, transient),
//! and the attempt is journaled as `FAILED`. Later attempts keep failing while the lock is held and
//! the first attempt that starts after the release succeeds, journaled as `SUCCEEDED`.

use dowai_core::{create_project, default_run_store_path, RunStore};
use dowai_postgres::{quote_identifier, PgProfile, PostgresAdapter, TlsMode};
use dowai_runtime::{run_postgres, RunRequest};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

struct TestWorkspace {
    root: PathBuf,
    project: PathBuf,
}

impl TestWorkspace {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock is after Unix epoch")
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("dowai-retry-pg-e2e-{}-{nonce}", std::process::id()));
        fs::create_dir_all(&root).expect("create isolated workspace");
        let project = create_project(&root, "fixture").expect("initialize project");
        Self { root, project }
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
    fn set(name: &'static str, value: &Path) -> Self {
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

fn test_profile() -> PgProfile {
    let host = std::env::var("DOWAI_TEST_PG_HOST").expect("DOWAI_TEST_PG_HOST is required");
    let port = std::env::var("DOWAI_TEST_PG_PORT")
        .ok()
        .and_then(|value| value.parse().ok())
        .expect("DOWAI_TEST_PG_PORT must be a valid port");
    let user = std::env::var("DOWAI_TEST_PG_USER").expect("DOWAI_TEST_PG_USER is required");
    let database =
        std::env::var("DOWAI_TEST_PG_DATABASE").expect("DOWAI_TEST_PG_DATABASE is required");
    let mut profile = PgProfile::new(host, port, user, database, "DOWAI_TEST_PG_PASSWORD");
    if std::env::var("DOWAI_TEST_PG_TLS").as_deref() == Ok("disabled") {
        profile.set_tls_mode(TlsMode::Disabled);
    }
    profile
}

#[test]
#[ignore = "opt-in E2E; requires a dedicated disposable PostgreSQL database"]
fn transient_lock_timeout_is_retried_and_each_attempt_is_journaled() {
    let workspace = TestWorkspace::new();
    let appdata = workspace.root.join("appdata");
    let _local_appdata = EnvironmentRestore::set("LOCALAPPDATA", &appdata);
    let _appdata_restore = EnvironmentRestore::set("APPDATA", &appdata);
    let _xdg_state = EnvironmentRestore::set("XDG_STATE_HOME", &appdata.join("state"));
    let _xdg_config = EnvironmentRestore::set("XDG_CONFIG_HOME", &appdata.join("config"));

    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock is after Unix epoch")
        .as_nanos();
    let source_table = format!("dowai_retry_source_{nonce}");
    let model = format!("retry_model_{nonce}");
    let model_file = format!("{model}.sql");
    let source_identifier = quote_identifier(&source_table).expect("valid generated source name");
    let model_identifier = quote_identifier(&model).expect("valid generated model name");

    let mut setup =
        PostgresAdapter::connect(&test_profile()).expect("connect to disposable PostgreSQL");
    setup
        .execute(
            &format!("CREATE TABLE public.{source_identifier} (value integer)"),
            &[],
        )
        .expect("create source relation");
    let seeded = 7_i32;
    setup
        .execute(
            &format!("INSERT INTO public.{source_identifier} (value) VALUES ($1)"),
            &[&seeded],
        )
        .expect("seed source relation");

    let workspace_config = json!({
        "schema_version": 2,
        "profiles": {
            "warehouse": {
                "type": "postgres",
                "host": std::env::var("DOWAI_TEST_PG_HOST").expect("test host"),
                "port": std::env::var("DOWAI_TEST_PG_PORT")
                    .expect("test port")
                    .parse::<u16>()
                    .expect("valid test port"),
                "user": std::env::var("DOWAI_TEST_PG_USER").expect("test user"),
                "database": std::env::var("DOWAI_TEST_PG_DATABASE").expect("test database"),
                "password_env": "DOWAI_TEST_PG_PASSWORD",
                "tls": if std::env::var("DOWAI_TEST_PG_TLS").as_deref() == Ok("disabled") {
                    "disabled"
                } else {
                    "required"
                }
            }
        },
        "defaults": {
            "execution": { "default_profile": "warehouse" }
        }
    });
    fs::write(
        workspace.root.join("workspace.json"),
        serde_json::to_vec(&workspace_config).expect("serialize workspace config"),
    )
    .expect("write workspace config");

    let project_manifest = workspace.project.join("dowai_project.json");
    let mut project_config: Value = serde_json::from_slice(
        &fs::read(&project_manifest).expect("read generated project config"),
    )
    .expect("generated project config is valid JSON");
    project_config["config"]["security"]["allow_external_writes"] = json!(true);
    project_config["config"]["execution"]["retry_max_attempts"] = json!(5);
    project_config["config"]["execution"]["retry_backoff_ms"] = json!(100);
    project_config["config"]["execution"]["retry_jitter"] = json!(false);
    project_config["config"]["database"]["query_timeout_seconds"] = json!(3);
    project_config["sources"] = json!({
        "sales": {
            "schema": "public",
            "tables": { "orders": source_table }
        }
    });
    fs::write(
        &project_manifest,
        serde_json::to_vec(&project_config).expect("serialize project config"),
    )
    .expect("write project config");

    fs::write(
        workspace.project.join("models").join(&model_file),
        "SELECT source.value AS value FROM {{ source('sales', 'orders') }} AS source",
    )
    .expect("write SQL model");

    // Holder connection: takes ACCESS EXCLUSIVE on the source and keeps it for 7 s, longer than the
    // 3 s statement timeout, so the first attempts fail and a later one succeeds after the release.
    let holder_profile = test_profile();
    let holder_relation = format!("public.{source_identifier}");
    let (locked_tx, locked_rx) = mpsc::channel::<()>();
    let holder = thread::spawn(move || {
        let mut holder = PostgresAdapter::connect(&holder_profile).expect("holder connection");
        holder
            .execute("BEGIN", &[])
            .expect("begin holder transaction");
        holder
            .execute(
                &format!("LOCK TABLE {holder_relation} IN ACCESS EXCLUSIVE MODE"),
                &[],
            )
            .expect("holder acquires exclusive lock");
        locked_tx.send(()).expect("signal lock acquired");
        thread::sleep(Duration::from_secs(7));
        holder.execute("COMMIT", &[]).expect("release lock");
    });
    locked_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("holder must acquire the lock before the run starts");

    let request = RunRequest {
        targets: vec![model_file.clone()],
        mode: None,
        profile: None,
        session_id: None,
        variables: BTreeMap::new(),
        confirm_destructive: false,
        full_refresh: false,
    };
    let result = run_postgres(&workspace.project, request)
        .expect("run should succeed on a later attempt after the transient lock timeout");
    holder.join().expect("holder thread finishes");

    assert_eq!(result.status, "SUCCEEDED");
    let journal = RunStore::for_user_state(default_run_store_path().expect("isolated journal"))
        .get(&result.run_id)
        .expect("run is journaled");
    let attempts: Vec<&str> = journal
        .invocations
        .iter()
        .filter(|entry| entry.kind == "sql" && entry.target.contains(&model))
        .map(|entry| entry.status.as_str())
        .collect();
    let (last, earlier) = attempts.split_last().expect("model attempts are journaled");
    assert_eq!(*last, "SUCCEEDED", "the final attempt succeeds");
    assert!(
        attempts.len() >= 2 && earlier.iter().all(|status| *status == "FAILED"),
        "every attempt before the success is journaled as FAILED: {attempts:?}"
    );
    let failed = journal
        .invocations
        .iter()
        .find(|entry| entry.kind == "sql" && entry.status == "FAILED")
        .expect("failed attempt is journaled");
    assert!(
        failed.error_code.is_some(),
        "failed attempt carries an error code"
    );

    let mut verify =
        PostgresAdapter::connect(&test_profile()).expect("reconnect to disposable PostgreSQL");
    let rows = verify
        .query(&format!("SELECT value FROM {model_identifier}"), &[])
        .expect("query materialized model");
    assert_eq!(rows[0].get::<_, i32>(0), seeded);

    // The model may be materialized as a table or a view; the kind-specific drop fails harmlessly
    // for the other one, so only the source drop is required to succeed.
    let _ = verify.execute(&format!("DROP VIEW IF EXISTS {model_identifier}"), &[]);
    let _ = verify.execute(&format!("DROP TABLE IF EXISTS {model_identifier}"), &[]);
    verify
        .execute(
            &format!("DROP TABLE IF EXISTS public.{source_identifier}"),
            &[],
        )
        .expect("drop source relation");
}
