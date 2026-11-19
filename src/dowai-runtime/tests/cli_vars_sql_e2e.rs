//! AC036 (plan 4.3, 15.2, 17.2): CLI/API variables select the correct SQL template branch
//! and are never interpolated into the SQL text.
//!
//! The fast tests need no database. The PostgreSQL test is opt-in (`--ignored`) and uses
//! the `DOWAI_TEST_PG_*` variables plus an isolated state directory.

use dowai_core::{create_project, render_sql_model, TemplateOptions};
use dowai_postgres::{quote_identifier, PgProfile, PostgresAdapter, TlsMode};
use dowai_runtime::{run_postgres, RunRequest};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

const INJECTION: &str = "x'; DROP TABLE t; --";
const BRANCH_TEMPLATE: &str = "{% if var('carga_total') == true %}\
SELECT 1 AS id UNION ALL SELECT 2 UNION ALL SELECT 3\
{% else %}SELECT 1 AS id{% endif %}";

static ENVIRONMENT_LOCK: Mutex<()> = Mutex::new(());

fn render(
    source: &str,
    variables: Value,
) -> Result<dowai_core::RenderedSqlModel, dowai_core::DowaiError> {
    let variables: BTreeMap<String, Value> = serde_json::from_value(variables).expect("object");
    render_sql_model(
        source,
        &variables,
        &TemplateOptions::default(),
        &BTreeMap::new(),
    )
}

#[test]
fn boolean_variable_selects_the_matching_branch() {
    let on = render(BRANCH_TEMPLATE, json!({"carga_total": true})).expect("render true");
    let off = render(BRANCH_TEMPLATE, json!({"carga_total": false})).expect("render false");
    assert!(on.sql.contains("UNION ALL SELECT 3"));
    assert_eq!(off.sql, "SELECT 1 AS id");
    // A string "true" is not the boolean true: strict comparison picks the else branch.
    let text = render(BRANCH_TEMPLATE, json!({"carga_total": "true"})).expect("render string");
    assert_eq!(text.sql, "SELECT 1 AS id");
}

#[test]
fn hostile_string_value_is_compared_but_never_interpolated() {
    let source = "{% if var('nome') == \"x'; DROP TABLE t; --\" %}SELECT 'hit' AS r\
                  {% else %}SELECT 'miss' AS r{% endif %}";
    let hit = render(source, json!({ "nome": INJECTION })).expect("render hit");
    assert_eq!(hit.sql, "SELECT 'hit' AS r");
    let miss = render(source, json!({"nome": "other"})).expect("render miss");
    assert_eq!(miss.sql, "SELECT 'miss' AS r");
    for rendered in [&hit.sql, &miss.sql] {
        assert!(!rendered.contains("DROP"), "value leaked into SQL");
    }
    // The value is only reachable through the variable context, not through the SQL.
    assert_eq!(hit.variables["nome"], json!(INJECTION));
}

#[test]
fn using_a_variable_value_as_sql_is_rejected_explicitly() {
    for source in [
        "SELECT '{{ var('nome') }}' AS r",
        "SELECT {{ nome }} AS r",
        "SELECT '{{ var(\"nome\") | safe }}'",
    ] {
        let error = render(source, json!({ "nome": INJECTION })).expect_err(source);
        assert_eq!(error.code(), "DOWAI_TEMPLATE_INVALID", "{source}");
        assert!(!error.to_string().contains("DROP"), "value echoed: {error}");
    }
}

struct EnvironmentRestore(Vec<(&'static str, Option<OsString>)>);

impl EnvironmentRestore {
    fn set(values: &[(&'static str, PathBuf)]) -> Self {
        let mut previous = Vec::new();
        for (name, value) in values {
            previous.push((*name, std::env::var_os(name)));
            std::env::set_var(name, value);
        }
        Self(previous)
    }
}

impl Drop for EnvironmentRestore {
    fn drop(&mut self) {
        for (name, value) in self.0.drain(..) {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
    }
}

struct Workspace {
    _lock: MutexGuard<'static, ()>,
    root: PathBuf,
    project: PathBuf,
    _env: EnvironmentRestore,
}

impl Workspace {
    fn new() -> Self {
        let lock = ENVIRONMENT_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root = std::env::temp_dir().join(format!("dowai-ac036-{}-{nonce}", std::process::id()));
        fs::create_dir_all(&root).expect("create workspace");
        let appdata = root.join("appdata");
        let env = EnvironmentRestore::set(&[
            ("LOCALAPPDATA", appdata.clone()),
            ("APPDATA", appdata.clone()),
            ("XDG_STATE_HOME", appdata.join("state")),
            ("XDG_CONFIG_HOME", appdata.join("config")),
        ]);
        let project = create_project(&root, "fixture").expect("init project");
        Self {
            _lock: lock,
            root,
            project,
            _env: env,
        }
    }

    fn write_model(&self, name: &str, body: &str) {
        fs::write(self.project.join("models").join(name), body).expect("write model");
    }

    fn configure(&self) {
        let pg = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("{name} required"));
        let tls = if std::env::var("DOWAI_TEST_PG_TLS").as_deref() == Ok("disabled") {
            "disabled"
        } else {
            "required"
        };
        let config = json!({
            "schema_version": 2,
            "profiles": {"warehouse": {
                "type": "postgres",
                "host": pg("DOWAI_TEST_PG_HOST"),
                "port": pg("DOWAI_TEST_PG_PORT").parse::<u16>().expect("port"),
                "user": pg("DOWAI_TEST_PG_USER"),
                "database": pg("DOWAI_TEST_PG_DATABASE"),
                "password": "{{ env_var('DOWAI_TEST_PG_PASSWORD') }}",
                "tls": tls
            }},
            "defaults": {"execution": {"default_profile": "warehouse"}}
        });
        fs::write(
            self.root.join("profiles.json"),
            serde_json::to_vec(&config).expect("encode"),
        )
        .expect("write workspace");
        let manifest = self.project.join("dowai_project.json");
        let mut project: Value =
            serde_json::from_slice(&fs::read(&manifest).expect("read")).expect("json");
        project["config"]["security"]["allow_external_writes"] = json!(true);
        fs::write(&manifest, serde_json::to_vec(&project).expect("encode")).expect("write");
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn profile() -> PgProfile {
    let port = std::env::var("DOWAI_TEST_PG_PORT")
        .ok()
        .and_then(|value| value.parse().ok())
        .expect("DOWAI_TEST_PG_PORT");
    let mut profile = PgProfile::new(
        std::env::var("DOWAI_TEST_PG_HOST").expect("host"),
        port,
        std::env::var("DOWAI_TEST_PG_USER").expect("user"),
        std::env::var("DOWAI_TEST_PG_DATABASE").expect("database"),
        "DOWAI_TEST_PG_PASSWORD",
    );
    if std::env::var("DOWAI_TEST_PG_TLS").as_deref() == Ok("disabled") {
        profile.set_tls_mode(TlsMode::Disabled);
    }
    profile
}

fn request(file: &str, variables: Value) -> RunRequest {
    RunRequest {
        targets: vec![file.to_owned()],
        mode: None,
        profile: None,
        session_id: None,
        variables: serde_json::from_value(variables).expect("variables object"),
        confirm_destructive: false,
        full_refresh: false,
    }
}

#[test]
#[ignore = "opt-in E2E; requires a dedicated disposable PostgreSQL database"]
fn cli_variables_choose_the_branch_in_the_database_without_injection() {
    let workspace = Workspace::new();
    workspace.configure();
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let full = format!("ac036_full_{nonce}");
    let partial = format!("ac036_partial_{nonce}");
    let hostile = format!("ac036_hostile_{nonce}");
    let sentinel = format!("ac036_t_{nonce}");
    workspace.write_model(&format!("{full}.sql"), BRANCH_TEMPLATE);
    workspace.write_model(&format!("{partial}.sql"), BRANCH_TEMPLATE);
    // The hostile text appears in the template only as a comparison literal; the sentinel
    // table is named like the statement a successful injection would target.
    workspace.write_model(
        &format!("{hostile}.sql"),
        &format!(
            "{{% if var('nome') == \"x'; DROP TABLE {sentinel}; --\" %}}\
             SELECT 'hit' AS r{{% else %}}SELECT 'miss' AS r{{% endif %}}"
        ),
    );

    let mut adapter = PostgresAdapter::connect(&profile()).expect("connect to PostgreSQL");
    let sentinel_identifier = quote_identifier(&sentinel).expect("identifier");
    adapter
        .execute(
            &format!("CREATE TABLE {sentinel_identifier} (v integer)"),
            &[],
        )
        .expect("create sentinel table");

    let count = |adapter: &mut PostgresAdapter, relation: &str| -> i64 {
        let identifier = quote_identifier(relation).expect("identifier");
        adapter
            .query(&format!("SELECT count(*) FROM {identifier}"), &[])
            .expect("count rows")[0]
            .get::<_, i64>(0)
    };

    let result = run_postgres(
        &workspace.project,
        request(&format!("{full}.sql"), json!({"carga_total": true})),
    )
    .expect("run with carga_total=true");
    assert_eq!(result.status, "SUCCEEDED");
    assert_eq!(count(&mut adapter, &full), 3);

    let result = run_postgres(
        &workspace.project,
        request(&format!("{partial}.sql"), json!({"carga_total": false})),
    )
    .expect("run with carga_total=false");
    assert_eq!(result.status, "SUCCEEDED");
    assert_eq!(count(&mut adapter, &partial), 1);

    let result = run_postgres(
        &workspace.project,
        request(
            &format!("{hostile}.sql"),
            json!({ "nome": format!("x'; DROP TABLE {sentinel}; --") }),
        ),
    )
    .expect("run with hostile string value");
    assert_eq!(result.status, "SUCCEEDED");
    let identifier = quote_identifier(&hostile).expect("identifier");
    let rows = adapter
        .query(&format!("SELECT r FROM {identifier}"), &[])
        .expect("read hostile result");
    assert_eq!(rows[0].get::<_, String>(0), "hit");
    // The sentinel table survived: the value was compared, not executed.
    assert_eq!(count(&mut adapter, &sentinel), 0);

    // Using a variable value as SQL text is refused before anything reaches the database.
    workspace.write_model("ac036_value_as_sql.sql", "SELECT '{{ var('nome') }}' AS r");
    let error = run_postgres(
        &workspace.project,
        request("ac036_value_as_sql.sql", json!({ "nome": INJECTION })),
    )
    .expect_err("variable interpolation must be rejected");
    assert_eq!(error.code(), "DOWAI_TEMPLATE_INVALID");

    for relation in [&full, &partial, &hostile, &sentinel] {
        adapter
            .drop_relation(relation, true)
            .expect("clean up relation");
    }
}
