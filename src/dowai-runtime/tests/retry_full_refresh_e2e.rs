//! E2E de `dowai retry` de run `--full-refresh` (ENT-112) contra PostgreSQL real.
//!
//! Opt-in: mesmas variáveis de `full_refresh_e2e.rs` (`DOWAI_TEST_PG_*`). Use somente um banco
//! de teste descartável.
//!
//! Prova que o retry reaplica o modo: a checagem de schema divergente continua valendo, o
//! destino é preservado em falha, e o retry exige confirmação explícita.

use dowai_core::{create_project, default_run_store_path, CancellationToken, RunStore};
use dowai_postgres::{quote_identifier, PgProfile, PostgresAdapter, TlsMode};
use dowai_runtime::{
    retry_run_detailed_with_cancel, run_postgres, run_postgres_detailed_with_cancel, RetryRequest,
    RunRequest,
};
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
            "dowai-retry-full-refresh-e2e-{}-{nonce}",
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

fn request(model: &str, confirm: bool) -> RunRequest {
    RunRequest {
        targets: vec![model.to_owned()],
        mode: None,
        profile: None,
        session_id: None,
        variables: BTreeMap::new(),
        confirm_destructive: confirm,
        full_refresh: true,
    }
}

fn retry_request(confirm: bool) -> RetryRequest {
    RetryRequest {
        failed_only: false,
        variables: BTreeMap::new(),
        confirm_destructive: confirm,
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
fn retry_of_full_refresh_keeps_mode_schema_check_and_confirmation() {
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
    let model = format!("rfr_e2e_{nonce}");
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
                "password": "{{ env_var('DOWAI_TEST_PG_PASSWORD') }}",
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
        workspace.root.join("profiles.json"),
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

    // 1. Carga inicial com confirmação: a relação passa a existir com v1.
    workspace.write_model(&model_file, "SELECT 1 AS id, 'v1'::text AS name");
    run_postgres(&workspace.project, request(&model_file, true))
        .expect("initial full refresh creates the relation");
    assert_eq!(
        snapshot(&mut adapter, &relation),
        vec![(1, "v1".to_owned())]
    );

    // 2. Run de full refresh com schema divergente: falha, o destino fica em v1 e a falha
    //    fica no journal com o identificador da run.
    workspace.write_model(
        &model_file,
        "SELECT 2 AS id, 'v2'::text AS name, 1 AS extra",
    );
    let failed = run_postgres_detailed_with_cancel(
        &workspace.project,
        request(&model_file, true),
        &CancellationToken::new(),
    )
    .expect_err("schema drift must fail the original run");
    assert_eq!(failed.error.exit_code(), 3);
    let failed_run_id = failed.run_id.expect("a started run keeps its identifier");
    assert_eq!(
        snapshot(&mut adapter, &relation),
        vec![(1, "v1".to_owned())]
    );

    let journal = RunStore::for_user_state(default_run_store_path().expect("journal path"));
    assert_eq!(
        journal
            .retry_source(&failed_run_id)
            .expect("journaled source")
            .full_refresh,
        Some(true),
        "the journal must record the full refresh mode"
    );

    // 3. Retry sem confirmação: recusado antes de iniciar qualquer run.
    let refused = retry_run_detailed_with_cancel(
        &workspace.project,
        &failed_run_id,
        retry_request(false),
        &CancellationToken::new(),
    )
    .expect_err("retry of a full refresh without --yes must be refused");
    assert_eq!(refused.error.code(), "DOWAI_POLICY_DENIED");
    assert_eq!(refused.error.exit_code(), 5);
    assert!(refused.run_id.is_none(), "refused before a new run starts");
    assert_eq!(
        snapshot(&mut adapter, &relation),
        vec![(1, "v1".to_owned())]
    );

    // 4. Retry com confirmação: o modo full refresh é reaplicado, então a checagem de schema
    //    divergente falha de novo e o destino continua em v1 (sem substituição comum).
    let retried = retry_run_detailed_with_cancel(
        &workspace.project,
        &failed_run_id,
        retry_request(true),
        &CancellationToken::new(),
    )
    .expect_err("retry must still refuse schema drift");
    assert_eq!(retried.error.exit_code(), 3);
    let retry_run_id = retried.run_id.expect("the retry run keeps its identifier");
    assert_ne!(retry_run_id, failed_run_id);
    assert_eq!(
        journal
            .retry_source(&retry_run_id)
            .expect("journaled retry source")
            .full_refresh,
        Some(true),
        "the retry run must also be a full refresh"
    );
    assert_eq!(
        snapshot(&mut adapter, &relation),
        vec![(1, "v1".to_owned())]
    );

    // 5. Schema corrigido: o retry com confirmação faz a troca atômica com os dados novos.
    workspace.write_model(&model_file, "SELECT 3 AS id, 'v3'::text AS name");
    let swapped = retry_run_detailed_with_cancel(
        &workspace.project,
        &failed_run_id,
        retry_request(true),
        &CancellationToken::new(),
    )
    .expect("retry with matching schema swaps the relation");
    assert_eq!(swapped.status, "SUCCEEDED");
    assert_eq!(
        snapshot(&mut adapter, &relation),
        vec![(3, "v3".to_owned())]
    );

    let _ = adapter.execute(&format!("DROP TABLE IF EXISTS {quoted}"), &[]);
}
