//! Opt-in end-to-end coverage against disposable Oracle and SQL Server test databases.

use dowai_core::{create_project, default_session_store_path, SessionStore};
use dowai_databases_oracle::{ConnectionProfile, OracleAdapter, TlsMode};
use dowai_runtime::{run_postgres, RunRequest};
use dowai_sqlserver::{SqlServerAdapter, SqlServerProfile};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

fn setting(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("missing test setting {name}"))
}

fn oracle_profile() -> ConnectionProfile {
    ConnectionProfile::new(
        setting("DOWAI_TEST_ORACLE_USER"),
        setting("DOWAI_TEST_ORACLE_HOST"),
        setting("DOWAI_TEST_ORACLE_PORT").parse().unwrap(),
        setting("DOWAI_TEST_ORACLE_SERVICE"),
        "DOWAI_ORACLE_PASSWORD",
    )
    .with_tls_mode(TlsMode::NativeRequired)
    .with_connect_timeout_secs(30)
    .with_call_timeout_secs(30)
}

fn sqlserver_profile() -> SqlServerProfile {
    let mut profile = SqlServerProfile::new(
        setting("DOWAI_TEST_SQLSERVER_HOST"),
        setting("DOWAI_TEST_SQLSERVER_PORT").parse().unwrap(),
        setting("DOWAI_TEST_SQLSERVER_DATABASE"),
        setting("DOWAI_TEST_SQLSERVER_USER"),
        "DOWAI_SQLSERVER_PASSWORD",
    );
    profile.connect_timeout = Duration::from_secs(30);
    profile
}

struct RestoreEnv {
    name: &'static str,
    previous: Option<OsString>,
}

impl RestoreEnv {
    fn set(name: &'static str, path: &Path) -> Self {
        let previous = std::env::var_os(name);
        std::env::set_var(name, path);
        Self { name, previous }
    }
}

impl Drop for RestoreEnv {
    fn drop(&mut self) {
        match self.previous.take() {
            Some(value) => std::env::set_var(self.name, value),
            None => std::env::remove_var(self.name),
        }
    }
}

struct Fixture {
    root: PathBuf,
    project: PathBuf,
    _environment: Vec<RestoreEnv>,
}

impl Fixture {
    fn new(oracle_schema: &str, oracle_table: &str, sql_table: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "dowai-native-e2e-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let state = root.join("state");
        fs::create_dir_all(&state).unwrap();
        let environment = [
            "LOCALAPPDATA",
            "APPDATA",
            "PROGRAMDATA",
            "XDG_STATE_HOME",
            "XDG_CONFIG_HOME",
        ]
        .into_iter()
        .map(|name| RestoreEnv::set(name, &state))
        .collect();
        let project = create_project(&root, "fixture").unwrap();
        let venv = project.join(".venv");
        let status = Command::new(setting("DOWAI_TEST_PYTHON"))
            .args(["-m", "venv", "--without-pip"])
            .arg(&venv)
            .status()
            .unwrap();
        assert!(
            status.success(),
            "Python virtual environment creation failed"
        );
        let interpreter = if cfg!(windows) {
            venv.join("Scripts").join("python.exe")
        } else {
            venv.join("bin").join("python")
        };
        let manifest_path = project.join("dowai_project.json");
        let mut manifest: Value =
            serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
        manifest["python"] = json!({"interpreter": interpreter, "timeout_seconds": 60});
        manifest["config"]["security"]["allow_external_writes"] = json!(true);
        manifest["sources"] = json!({
            "oracle_source": {"schema": oracle_schema, "tables":{"items":oracle_table}, "profile":"oracle"},
            "sql_source": {"schema":"dbo", "tables":{"items":sql_table}, "profile":"sqlserver"}
        });
        fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        let oracle = oracle_profile();
        let sqlserver = sqlserver_profile();
        let workspace = json!({
            "schema_version":2,
            "profiles":{
                "oracle":{"type":"oracle","host":oracle.host,"port":oracle.port,"user":oracle.user,
                    "database":oracle.service,"password_env":"DOWAI_ORACLE_PASSWORD","tls":"oracle_native"},
                "sqlserver":{"type":"sqlserver","host":sqlserver.host,"port":sqlserver.port,"user":sqlserver.user,
                    "database":sqlserver.database,"password_env":"DOWAI_SQLSERVER_PASSWORD","tls":"required"}
            },
            "defaults":{"execution":{"default_profile":"sqlserver"}}
        });
        fs::write(
            root.join("workspace.json"),
            serde_json::to_vec(&workspace).unwrap(),
        )
        .unwrap();
        Self {
            root,
            project,
            _environment: environment,
        }
    }

    fn model(&self, name: &str, body: &str) {
        fs::write(self.project.join("models").join(name), body).unwrap();
    }

    fn run(&self, name: &str, profile: Option<&str>) -> BTreeMap<String, Value> {
        let result = run_postgres(
            &self.project,
            RunRequest {
                targets: vec![name.to_owned()],
                mode: None,
                profile: profile.map(str::to_owned),
                session_id: None,
                variables: BTreeMap::new(),
                confirm_destructive: false,
                full_refresh: false,
            },
        )
        .unwrap_or_else(|error| {
            panic!(
                "runtime model failed: {} ({})",
                error.message(),
                error.code()
            )
        });
        assert_eq!(result.status, "SUCCEEDED");
        SessionStore::for_user_state(default_session_store_path().unwrap())
            .snapshot(&result.session_id)
            .unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.root) {
            eprintln!("Test workspace cleanup failed: {error}");
        }
    }
}

struct Tables {
    oracle: Vec<String>,
    sqlserver: Vec<String>,
}

impl Drop for Tables {
    fn drop(&mut self) {
        if let Ok(oracle) = OracleAdapter::connect(&oracle_profile()) {
            for name in &self.oracle {
                let relation = dowai_databases_oracle::quote_identifier(name).unwrap();
                if let Err(error) = oracle.execute(&format!("DROP TABLE {relation} PURGE"), &[]) {
                    if !matches!(
                        error,
                        dowai_databases_oracle::AdapterError::Oracle {
                            kind: dowai_databases_oracle::OracleErrorKind::ObjectNotFound,
                            ..
                        }
                    ) {
                        eprintln!("Oracle test cleanup failed: {error}");
                    }
                }
            }
        } else {
            eprintln!("Oracle test cleanup connection failed");
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        match runtime.block_on(SqlServerAdapter::connect(&sqlserver_profile())) {
            Ok(mut connection) => {
                for name in &self.sqlserver {
                    let relation = dowai_sqlserver::quote_identifier(name).unwrap();
                    if let Err(error) = runtime.block_on(
                        connection.execute_sql(&format!("DROP TABLE IF EXISTS {relation}")),
                    ) {
                        eprintln!("SQL Server test cleanup failed: {error}");
                    }
                }
            }
            Err(error) => eprintln!("SQL Server test cleanup connection failed: {error}"),
        }
    }
}

#[test]
#[ignore = "requires both encrypted test databases, Oracle Client and an installed Python"]
fn models_sources_and_cross_database_writes() {
    let suffix = &uuid::Uuid::new_v4().simple().to_string()[..12];
    let ora_target = format!("DOWAI_O_{suffix}");
    let sql_target = format!("DOWAI_S_{suffix}");
    let ora_model = format!("ora_{suffix}");
    let sql_model = format!("sql_{suffix}");
    let child_model = format!("child_{suffix}");
    let parent_model = format!("parent_{suffix}");
    let chained_model = format!("chained_{suffix}");
    let oracle_parent = format!("op_{suffix}");
    let oracle_child = format!("oc_{suffix}");
    let cycle_a = format!("ca_{suffix}");
    let cycle_b = format!("cb_{suffix}");
    let no_python_parent = format!("np_{suffix}");
    let no_python_child = format!("nc_{suffix}");
    let _tables = Tables {
        oracle: vec![
            ora_target.clone(),
            ora_model.clone(),
            oracle_parent.clone(),
            oracle_child.clone(),
        ],
        sqlserver: vec![
            sql_target.clone(),
            sql_model.clone(),
            child_model.clone(),
            parent_model.clone(),
            chained_model.clone(),
            cycle_a.clone(),
            cycle_b.clone(),
            no_python_parent.clone(),
            no_python_child.clone(),
        ],
    };
    let oracle = OracleAdapter::connect(&oracle_profile()).unwrap();
    let ora_quoted = dowai_databases_oracle::quote_identifier(&ora_target).unwrap();
    oracle
        .execute(
            &format!(
                "CREATE TABLE {ora_quoted} (\"id\" NUMBER PRIMARY KEY, \"label\" VARCHAR2(40))"
            ),
            &[],
        )
        .unwrap();
    let oracle_schema = oracle
        .query_json(
            "SELECT SYS_CONTEXT('USERENV','CURRENT_SCHEMA') AS SCHEMA_NAME FROM DUAL",
            &[],
        )
        .unwrap()[0]["SCHEMA_NAME"]
        .as_str()
        .unwrap()
        .to_owned();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut sqlserver = runtime
        .block_on(SqlServerAdapter::connect(&sqlserver_profile()))
        .unwrap();
    let sql_quoted = dowai_sqlserver::quote_identifier(&sql_target).unwrap();
    runtime
        .block_on(sqlserver.execute_sql(&format!(
            "CREATE TABLE {sql_quoted} ([id] int PRIMARY KEY, [label] nvarchar(40))"
        )))
        .unwrap();

    let fixture = Fixture::new(&oracle_schema, &ora_target, &sql_target);
    fixture.model("to_sql.py", &format!(
        "def model(env, origem=None):\n    rows = [{{'id': int(r['id']), 'label': r['label']}} for r in origem.iter_rows(batch_size=1)]\n    env.write(rows, target='sqlserver', table='{sql_target}', mode='append')\n    env.setVar('oracle_rows', len(rows))\n"
    ));
    fixture.model(&format!("{ora_model}.sql"), "SELECT 1 AS \"id\", 'one' AS \"label\" FROM DUAL UNION ALL SELECT 2 AS \"id\", 'two' AS \"label\" FROM DUAL\n{{ run('to_sql.py') }}");
    assert_eq!(
        fixture.run(&format!("{ora_model}.sql"), Some("oracle"))["oracle_rows"],
        json!(2)
    );

    fixture.model("to_oracle.py", &format!(
        "def model(env, origem=None):\n    rows = [{{'id': int(r['id']), 'label': r['label']}} for r in origem.iter_rows(batch_size=1)]\n    env.write(rows, target='oracle', table='{ora_target}', mode='append')\n    env.setVar('sql_rows', len(rows))\n"
    ));
    fixture.model(&format!("{sql_model}.sql"), "SELECT 3 AS [id], 'three' AS [label] UNION ALL SELECT 4 AS [id], 'four' AS [label]\n{{ run('to_oracle.py') }}");
    assert_eq!(
        fixture.run(&format!("{sql_model}.sql"), Some("sqlserver"))["sql_rows"],
        json!(2)
    );

    fixture.model("reader.py", "def model(env):\n    a = list(env.source('oracle_source', 'items', profile='oracle').iter_rows(batch_size=1))\n    b = list(env.source('sql_source', 'items', profile='sqlserver').select('id').iter_rows(batch_size=1))\n    env.setVar('ids', [int(x['id']) for x in a] + [int(x['id']) for x in b])\n");
    assert_eq!(fixture.run("reader.py", None)["ids"], json!([3, 4, 1, 2]));
    fixture.model("modes.py", &format!(
        "def model(env):\n    env.write([{{'id': 3, 'label': 'changed'}}, {{'id': 5, 'label': 'added'}}], target='oracle', table='{ora_target}', mode='upsert', keys=['id'])\n    env.write([{{'id': 8, 'label': 'replaced'}}], target='sqlserver', table='{sql_target}', mode='replace', confirm_replace=True)\n"
    ));
    fixture.run("modes.py", None);
    let oracle_rows = oracle
        .query_json(
            &format!("SELECT \"id\", \"label\" FROM {ora_quoted} ORDER BY \"id\""),
            &[],
        )
        .unwrap();
    assert_eq!(
        oracle_rows
            .iter()
            .map(|row| row["id"].clone())
            .collect::<Vec<_>>(),
        vec![json!("3"), json!("4"), json!("5")]
    );
    assert_eq!(oracle_rows[0]["label"], json!("changed"));
    let sql_rows = runtime
        .block_on(sqlserver.query_json_rows(&format!("SELECT [id] FROM {sql_quoted}"), &[]))
        .unwrap();
    assert_eq!(sql_rows[0]["id"], json!("8"));

    fixture.model(
        "capture.py",
        "def model(env, origem=None):\n    env.setVar('chained_count', int(next(origem.iter_rows())['n']))\n",
    );
    fixture.model(
        &format!("{chained_model}.sql"),
        "SELECT COUNT(*) AS [n] FROM {{ source('sql_source', 'items') }}\n{{ run('capture.py') }}",
    );
    fixture.model(&format!("{parent_model}.sql"), &format!("SELECT [id] FROM {{{{ source('sql_source', 'items') }}}}\n{{{{ run('{chained_model}.sql') }}}}"));
    assert_eq!(
        fixture.run(&format!("{parent_model}.sql"), Some("sqlserver"))["chained_count"],
        json!(1)
    );
    let chained_relation = dowai_sqlserver::quote_identifier(&chained_model).unwrap();
    let chained = runtime
        .block_on(sqlserver.query_json_rows(&format!("SELECT [n] FROM {chained_relation}"), &[]))
        .unwrap();
    assert_eq!(chained[0]["n"], json!("1"));

    fixture.model(
        "cross_profile.sql",
        "SELECT id FROM {{ source('oracle_source', 'items') }}",
    );
    let cross_profile = run_postgres(
        &fixture.project,
        RunRequest {
            targets: vec!["cross_profile.sql".to_owned()],
            mode: None,
            profile: Some("sqlserver".to_owned()),
            session_id: None,
            variables: BTreeMap::new(),
            confirm_destructive: false,
            full_refresh: false,
        },
    )
    .expect_err("SQL source on a different profile must be rejected");
    assert!(cross_profile
        .message()
        .contains("SQL sources must use the SQL execution profile"));
    fixture.model(&format!("{child_model}.sql"), "SELECT 42 AS [id]");
    fixture.model(
        "nested.py",
        &format!("def model(env):\n    env.run('{child_model}.sql')\n"),
    );
    fixture.run("nested.py", None);
    let child_relation = dowai_sqlserver::quote_identifier(&child_model).unwrap();
    let rows = runtime
        .block_on(sqlserver.query_json_rows(&format!("SELECT [id] FROM {child_relation}"), &[]))
        .unwrap();
    assert_eq!(rows[0]["id"], json!("42"));

    let manifest_path = fixture.project.join("dowai_project.json");
    let mut manifest: Value = serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    manifest.as_object_mut().unwrap().remove("python");
    fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
    fixture.model(
        &format!("{oracle_child}.sql"),
        "SELECT 42 AS \"id\" FROM DUAL",
    );
    fixture.model(
        &format!("{oracle_parent}.sql"),
        &format!("SELECT 1 AS \"id\" FROM DUAL\n{{{{ run('{oracle_child}.sql') }}}}"),
    );
    fixture.run(&format!("{oracle_parent}.sql"), Some("oracle"));
    let child_relation = dowai_databases_oracle::quote_identifier(&oracle_child).unwrap();
    assert_eq!(
        oracle
            .query_json(&format!("SELECT \"id\" FROM {child_relation}"), &[])
            .unwrap()[0]["id"],
        json!("42")
    );
    fixture.model(&format!("{no_python_child}.sql"), "SELECT 42 AS [id]");
    fixture.model(
        &format!("{no_python_parent}.sql"),
        &format!("SELECT 1 AS [id]\n{{{{ run('{no_python_child}.sql') }}}}"),
    );
    fixture.run(&format!("{no_python_parent}.sql"), Some("sqlserver"));
    let child_relation = dowai_sqlserver::quote_identifier(&no_python_child).unwrap();
    assert_eq!(
        runtime
            .block_on(sqlserver.query_json_rows(&format!("SELECT [id] FROM {child_relation}"), &[]))
            .unwrap()[0]["id"],
        json!("42")
    );
    fixture.model(
        &format!("{cycle_a}.sql"),
        &format!("SELECT 1 AS [id]\n{{{{ run('{cycle_b}.sql') }}}}"),
    );
    fixture.model(
        &format!("{cycle_b}.sql"),
        &format!("SELECT 2 AS [id]\n{{{{ run('{cycle_a}.sql') }}}}"),
    );
    let cycle = run_postgres(
        &fixture.project,
        RunRequest {
            targets: vec![format!("{cycle_a}.sql")],
            mode: None,
            profile: Some("sqlserver".to_owned()),
            session_id: None,
            variables: BTreeMap::new(),
            confirm_destructive: false,
            full_refresh: false,
        },
    )
    .expect_err("nested SQL cycle must be rejected");
    assert!(cycle.message().contains("cycle"));
}
