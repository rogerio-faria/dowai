use dowai_core::{
    create_project, default_run_store_path, default_session_store_path, RunStore, SessionStore,
};
use dowai_postgres::{quote_identifier, PgProfile, PostgresAdapter, TlsMode};
use dowai_runtime::{run_postgres, RunRequest};
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
        let root =
            std::env::temp_dir().join(format!("dowai-postgres-e2e-{}-{nonce}", std::process::id()));
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

#[test]
#[ignore = "opt-in E2E; requires a dedicated disposable PostgreSQL database"]
fn run_executes_sources_and_refs_and_requires_approval_to_replace_relations() {
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
    let source_table = format!("dowai_source_{nonce}");
    let upstream_model = format!("upstream_{nonce}");
    let report_model = format!("report_{nonce}");
    let upstream_file = format!("{upstream_model}.sql");
    let report_file = format!("{report_model}.sql");
    let source_identifier = quote_identifier(&source_table).expect("valid generated source name");

    let mut adapter =
        PostgresAdapter::connect(&test_profile()).expect("connect to disposable PostgreSQL");
    adapter
        .execute(
            &format!("CREATE TABLE public.{source_identifier} (value integer)"),
            &[],
        )
        .expect("create external source relation");
    let source_value = 7_i32;
    adapter
        .execute(
            &format!("INSERT INTO public.{source_identifier} (value) VALUES ($1)"),
            &[&source_value],
        )
        .expect("seed external source relation");

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

    workspace.write_model(&upstream_file, "SELECT 11 AS value");
    workspace.write_model(
        &report_file,
        &format!(
            "SELECT source.value + upstream.value AS value \
             FROM {{{{ source('sales', 'orders') }}}} AS source \
             CROSS JOIN {{{{ ref('{upstream_model}') }}}} AS upstream"
        ),
    );

    let request = RunRequest {
        targets: vec![report_file],
        mode: None,
        profile: None,
        session_id: None,
        variables: BTreeMap::from([("e2e_marker".to_owned(), json!("persisted"))]),
        confirm_destructive: false,
    };
    let result = run_postgres(&workspace.project, request.clone())
        .expect("run should execute SQL models and declared sources");
    assert_eq!(result.status, "SUCCEEDED");
    assert!(uuid::Uuid::parse_str(&result.invocation_id).is_ok());
    assert_ne!(result.invocation_id, result.run_id);
    assert_eq!(result.nodes.len(), 2);

    let journal = RunStore::for_user_state(default_run_store_path().expect("isolated journal"))
        .get(&result.run_id)
        .expect("successful SQL run is journaled");
    assert_eq!(journal.run.status, "SUCCEEDED");
    assert_eq!(journal.invocations.len(), 2);
    assert!(journal
        .invocations
        .iter()
        .all(|entry| entry.kind == "sql" && entry.status == "SUCCEEDED"));

    let audit = dowai_core::AuditLog::for_user_state(
        dowai_core::default_audit_log_path().expect("isolated audit path"),
    );
    let audit_events = audit.read_all().expect("read audit trail");
    let writes: Vec<_> = audit_events
        .iter()
        .filter(|event| event.event == "external.write" && event.run_id == result.run_id)
        .collect();
    assert_eq!(writes.len(), 2, "one external.write per SQL node");
    assert!(writes
        .iter()
        .all(|event| event.target_profile.as_deref() == Some("warehouse")));
    assert!(writes.iter().any(|event| event
        .relation
        .as_deref()
        .is_some_and(|relation| relation.contains(&upstream_model))));
    let audit_text =
        fs::read_to_string(dowai_core::default_audit_log_path().expect("isolated audit path"))
            .expect("audit file");
    assert!(!audit_text.contains("SELECT"), "SQL must not be audited");
    assert!(
        !audit_text.contains("persisted"),
        "variable values must not be audited"
    );

    let session_store = SessionStore::for_user_state(
        default_session_store_path().expect("isolated local app data path"),
    );
    let variables = session_store
        .snapshot(&result.session_id)
        .expect("load committed run variables");
    assert_eq!(variables["e2e_marker"], json!("persisted"));

    let report_relation = quote_identifier(&report_model).expect("valid generated report relation");
    let rows = adapter
        .query(&format!("SELECT value FROM {report_relation}"), &[])
        .expect("query materialized report relation");
    assert_eq!(rows[0].get::<_, i32>(0), 18);

    let mut blocked_request = request.clone();
    blocked_request.session_id = Some(result.session_id.clone());
    let approval_error = run_postgres(&workspace.project, blocked_request)
        .expect_err("replacing existing model relations must require approval");
    assert_eq!(approval_error.code(), "DOWAI_POLICY_DENIED");
    let denied = RunStore::for_user_state(default_run_store_path().expect("isolated journal"))
        .list(1)
        .expect("list journal");
    assert_eq!(denied[0].status, "FAILED");
    assert_eq!(denied[0].error_code.as_deref(), Some("DOWAI_POLICY_DENIED"));
    let unchanged_rows = adapter
        .query(&format!("SELECT value FROM {report_relation}"), &[])
        .expect("denied replacement must preserve the existing relation");
    assert_eq!(unchanged_rows[0].get::<_, i32>(0), 18);

    let mut approved_request = request;
    approved_request.session_id = Some(result.session_id);
    approved_request.confirm_destructive = true;
    let approved_result = run_postgres(&workspace.project, approved_request)
        .expect("explicitly approved replacement should succeed");
    assert_eq!(approved_result.status, "SUCCEEDED");

    for relation in [&report_model, &upstream_model, &source_table] {
        adapter
            .drop_relation(relation, true)
            .expect("clean up smoke-test relation");
    }
}
