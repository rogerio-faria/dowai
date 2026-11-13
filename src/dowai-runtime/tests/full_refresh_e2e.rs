//! E2E de `run --full-refresh` (ENT-033) contra PostgreSQL real, pelo runtime.
//!
//! Opt-in: mesmas variáveis de `postgres_e2e.rs` (`DOWAI_TEST_PG_*`). Use somente um banco de
//! teste descartável.

use dowai_core::{create_project, CancellationToken};
use dowai_postgres::{quote_identifier, PgProfile, PostgresAdapter, TlsMode};
use dowai_runtime::{run_postgres, run_postgres_detailed_with_cancel, RunRequest};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

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
        let root = std::env::temp_dir().join(format!(
            "dowai-full-refresh-e2e-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&root).expect("create isolated workspace");
        let project = create_project(&root, "fixture").expect("initialize project");
        Self { root, project }
    }

    fn write_model(&self, name: &str, body: &str) {
        fs::write(self.project.join("models").join(name), body).expect("write SQL model");
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

fn request(model: &str, confirm: bool, full_refresh: bool) -> RunRequest {
    RunRequest {
        targets: vec![model.to_owned()],
        mode: None,
        profile: None,
        session_id: None,
        variables: BTreeMap::new(),
        confirm_destructive: confirm,
        full_refresh,
    }
}

fn snapshot(adapter: &mut PostgresAdapter, relation: &str) -> Vec<(i32, String)> {
    let quoted = quote_identifier(relation).expect("valid identifier");
    adapter
        .query(&format!("SELECT id, name FROM {quoted} ORDER BY id"), &[])
        .expect("snapshot query")
        .iter()
        .map(|row| (row.get(0), row.get(1)))
        .collect()
}

#[test]
#[ignore = "opt-in E2E; requires a dedicated disposable PostgreSQL database"]
fn run_full_refresh_requires_confirmation_then_swaps_and_refuses_schema_drift() {
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
    let model = format!("fr_e2e_{nonce}");
    let model_file = format!("{model}.sql");
    let relation = model.clone();

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
    fs::write(
        &project_manifest,
        serde_json::to_vec(&project_config).expect("serialize project config"),
    )
    .expect("write project config");

    let mut adapter =
        PostgresAdapter::connect(&test_profile()).expect("connect to disposable PostgreSQL");
    let quoted = quote_identifier(&relation).expect("valid identifier");
    let _ = adapter.execute(&format!("DROP TABLE IF EXISTS {quoted}"), &[]);

    // 1. Sem confirmação: recusado antes de qualquer run, sem escrita.
    workspace.write_model(&model_file, "SELECT 1 AS id, 'v1'::text AS name");
    let refused = run_postgres_detailed_with_cancel(
        &workspace.project,
        request(&model_file, false, true),
        &CancellationToken::new(),
    )
    .expect_err("full refresh without --yes must be refused");
    assert_eq!(refused.error.code(), "DOWAI_POLICY_DENIED");
    assert_eq!(refused.error.exit_code(), 5);
    assert!(refused.run_id.is_none(), "refused before a run starts");

    // 2. Com confirmação: relação inexistente é criada.
    let created = run_postgres(&workspace.project, request(&model_file, true, true))
        .expect("full refresh creates a missing relation");
    assert_eq!(created.status, "SUCCEEDED");
    assert_eq!(
        snapshot(&mut adapter, &relation),
        vec![(1, "v1".to_owned())]
    );

    // 3. Mesmo schema com dados novos: troca atômica.
    workspace.write_model(&model_file, "SELECT 2 AS id, 'v2'::text AS name");
    run_postgres(&workspace.project, request(&model_file, true, true))
        .expect("full refresh with matching schema swaps the relation");
    assert_eq!(
        snapshot(&mut adapter, &relation),
        vec![(2, "v2".to_owned())]
    );

    // 4. Schema divergente: falha e mantém a relação anterior.
    workspace.write_model(
        &model_file,
        "SELECT 3 AS id, 'v3'::text AS name, 1 AS extra",
    );
    let drift = run_postgres(&workspace.project, request(&model_file, true, true))
        .expect_err("schema drift must fail by default");
    assert_eq!(drift.exit_code(), 3);
    assert_eq!(
        snapshot(&mut adapter, &relation),
        vec![(2, "v2".to_owned())]
    );

    let _ = adapter.execute(&format!("DROP TABLE IF EXISTS {quoted}"), &[]);
}
