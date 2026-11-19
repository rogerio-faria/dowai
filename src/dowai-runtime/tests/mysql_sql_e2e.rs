//! Opt-in SQL E2E para execução SQL no MySQL e rejeição de fontes de outro perfil.
//!
//! Exige bancos descartáveis `DOWAI_TEST_MYSQL_*` e `DOWAI_TEST_PG_*`.

use dowai_core::create_project;
use dowai_mysql::{quote_identifier as mysql_ident, MysqlAdapter, MysqlProfile};
use dowai_postgres::TlsMode;
use dowai_runtime::{run_postgres, RunRequest, RunResult};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} is required"))
}

fn nonce() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos()
}

fn mysql_profile() -> MysqlProfile {
    let mut profile = MysqlProfile::new(
        env("DOWAI_TEST_MYSQL_HOST"),
        env("DOWAI_TEST_MYSQL_PORT")
            .parse()
            .expect("valid MySQL port"),
        env("DOWAI_TEST_MYSQL_USER"),
        env("DOWAI_TEST_MYSQL_DATABASE"),
        "DOWAI_TEST_MYSQL_PASSWORD",
    );
    if std::env::var("DOWAI_TEST_MYSQL_TLS").as_deref() != Ok("required") {
        profile.set_tls_mode(TlsMode::Disabled);
    }
    profile
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

struct Workspace {
    root: PathBuf,
    project: PathBuf,
    _environment: Vec<EnvironmentRestore>,
}

impl Workspace {
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "dowai-mysql-sql-{name}-{}-{}",
            std::process::id(),
            nonce()
        ));
        fs::create_dir_all(&root).expect("create isolated workspace");
        let state = root.join("state");
        fs::create_dir_all(&state).expect("create isolated state directory");
        let environment = vec![
            EnvironmentRestore::set("LOCALAPPDATA", &state),
            EnvironmentRestore::set("APPDATA", &state),
            EnvironmentRestore::set("XDG_STATE_HOME", &state),
            EnvironmentRestore::set("XDG_CONFIG_HOME", &state),
        ];
        let project = create_project(&root, "fixture").expect("initialize project");
        Self {
            root,
            project,
            _environment: environment,
        }
    }

    fn configure(&self, include_postgres: bool) {
        let mut profiles = serde_json::Map::new();
        profiles.insert(
            "warehouse".to_owned(),
            json!({
                "type": "mysql",
                "host": env("DOWAI_TEST_MYSQL_HOST"),
                "port": env("DOWAI_TEST_MYSQL_PORT").parse::<u16>().expect("valid MySQL port"),
                "user": env("DOWAI_TEST_MYSQL_USER"),
                "database": env("DOWAI_TEST_MYSQL_DATABASE"),
                "password_env": "DOWAI_TEST_MYSQL_PASSWORD",
                "tls": if std::env::var("DOWAI_TEST_MYSQL_TLS").as_deref() == Ok("required") {
                    "required"
                } else {
                    "disabled"
                }
            }),
        );
        if include_postgres {
            profiles.insert(
                "origin".to_owned(),
                json!({
                    "type": "postgres",
                    "host": env("DOWAI_TEST_PG_HOST"),
                    "port": env("DOWAI_TEST_PG_PORT").parse::<u16>().expect("valid PostgreSQL port"),
                    "user": env("DOWAI_TEST_PG_USER"),
                    "database": env("DOWAI_TEST_PG_DATABASE"),
                    "password_env": "DOWAI_TEST_PG_PASSWORD",
                    "tls": if std::env::var("DOWAI_TEST_PG_TLS").as_deref() == Ok("disabled") {
                        "disabled"
                    } else {
                        "required"
                    }
                }),
            );
        }
        let workspace = json!({
            "schema_version": 2,
            "profiles": profiles,
            "defaults": { "execution": { "default_profile": "warehouse" } }
        });
        fs::write(
            self.root.join("workspace.json"),
            serde_json::to_vec(&workspace).expect("serialize workspace"),
        )
        .expect("write workspace config");

        let manifest_path = self.project.join("dowai_project.json");
        let mut manifest: Value =
            serde_json::from_slice(&fs::read(&manifest_path).expect("read project manifest"))
                .expect("manifest JSON");
        manifest["config"]["security"]["allow_external_writes"] = json!(true);
        fs::write(
            &manifest_path,
            serde_json::to_vec(&manifest).expect("serialize project"),
        )
        .expect("write project config");
    }

    fn run(
        &self,
        model: &str,
        profile: Option<&str>,
        confirm: bool,
        full_refresh: bool,
    ) -> Result<RunResult, dowai_core::DowaiError> {
        let result = run_postgres(
            &self.project,
            RunRequest {
                targets: vec![model.to_owned()],
                mode: None,
                profile: profile.map(str::to_owned),
                session_id: None,
                variables: BTreeMap::new(),
                confirm_destructive: confirm,
                full_refresh,
            },
        )?;
        Ok(result)
    }

    fn write_model(&self, name: &str, query: &str) {
        fs::write(self.project.join("models").join(name), query).expect("write model");
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

struct DatabaseCleanup {
    mysql_tables: Vec<String>,
}

impl Drop for DatabaseCleanup {
    fn drop(&mut self) {
        if let Ok(mut mysql) = MysqlAdapter::connect(&mysql_profile()) {
            for name in &self.mysql_tables {
                if let Ok(table) = mysql_ident(name) {
                    let _ = mysql.execute_sql(&format!("DROP TABLE IF EXISTS {table}"), &[]);
                }
            }
        }
    }
}

#[test]
#[ignore = "opt-in E2E; requires disposable MySQL and PostgreSQL profiles"]
fn mysql_sql_uses_its_connection_and_rejects_a_source_from_another_profile() {
    let workspace = Workspace::new("source");
    workspace.configure(true);

    let suffix = nonce();
    let mysql_source = format!("dowai_cross_mysql_source_{suffix}");
    let mysql_target = format!("dowai_mysql_target_{suffix}");
    let _cleanup = DatabaseCleanup {
        mysql_tables: vec![mysql_source.clone(), mysql_target.clone()],
    };
    let mysql_target_ident = mysql_ident(&mysql_target).expect("generated identifier");
    let mut mysql = MysqlAdapter::connect(&mysql_profile()).expect("connect MySQL");
    let mysql_source_ident = mysql_ident(&mysql_source).expect("generated identifier");
    mysql
        .execute_sql(
            &format!("CREATE TABLE {mysql_source_ident} (value INT NOT NULL)"),
            &[],
        )
        .expect("create MySQL source");
    mysql
        .execute_sql(
            &format!("INSERT INTO {mysql_source_ident} (value) VALUES (41)"),
            &[],
        )
        .expect("seed MySQL source");
    let project_manifest = workspace.project.join("dowai_project.json");
    let mut manifest: Value =
        serde_json::from_slice(&fs::read(&project_manifest).expect("read manifest"))
            .expect("manifest JSON");
    manifest["sources"] = json!({
        "mart": {
            "schema": env("DOWAI_TEST_MYSQL_DATABASE"),
            "profile": "warehouse",
            "tables": { "items": mysql_source }
        }
    });
    fs::write(
        &project_manifest,
        serde_json::to_vec(&manifest).expect("serialize manifest"),
    )
    .expect("write source declaration");

    let mysql_model = format!("{mysql_target}.sql");
    workspace.write_model(
        &mysql_model,
        "SELECT source.value + 1 AS value FROM {{ source('mart', 'items') }} AS source",
    );
    let result = workspace
        .run(&mysql_model, None, false, false)
        .expect("same-profile MySQL SQL run succeeds");
    assert_eq!(result.status, "SUCCEEDED");

    let rows = mysql
        .query_rows(&format!("SELECT value FROM {mysql_target_ident}"), &[])
        .expect("query materialized MySQL model");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], dowai_postgres::DatasetCell::Int(42));

    let mismatch_model = format!("dowai_mismatch_{suffix}.sql");
    workspace.write_model(
        &mismatch_model,
        "SELECT source.value FROM {{ source('legacy', 'items') }} AS source",
    );
    let mut manifest: Value =
        serde_json::from_slice(&fs::read(&project_manifest).expect("read manifest"))
            .expect("manifest JSON");
    manifest["sources"]["legacy"] = json!({
        "schema": "public",
        "profile": "origin",
        "tables": { "items": "does_not_need_to_exist" }
    });
    fs::write(
        &project_manifest,
        serde_json::to_vec(&manifest).expect("serialize manifest"),
    )
    .expect("write cross-profile source declaration");
    let error = workspace
        .run(&mismatch_model, None, false, false)
        .expect_err("cross-profile SQL source must be rejected");
    assert!(
        error
            .to_string()
            .contains("SQL sources must use the SQL execution profile"),
        "unexpected error: {error}"
    );
}
