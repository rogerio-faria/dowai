//! E2E de `env.write` para MySQL e transferência PostgreSQL -> MySQL pelo runtime (ENT-034).
//!
//! Opt-in (`--ignored --test-threads=1`: o estado de sessão usa variáveis de ambiente do
//! processo): exige `DOWAI_TEST_MYSQL_*` (MySQL descartável), `DOWAI_TEST_PG_*`
//! (PostgreSQL descartável, só para os testes de transferência) e um `python3` capaz de criar
//! venv. Cada teste cria tabelas com nome único e as remove ao fim.
//!
//! O que estes testes afirmam sobre atomicidade: cada escrita é atômica no MySQL de destino.
//! A transferência entre dois bancos NÃO é atômica: o teste `transfer_is_not_atomic_across_
//! databases` mostra o MySQL confirmado mesmo quando a execução falha depois.

use dowai_core::{create_project, default_session_store_path, DowaiError, SessionStore};
use dowai_mysql::{quote_identifier as my_ident, MysqlAdapter, MysqlProfile};
use dowai_postgres::{quote_identifier, DatasetCell, PgProfile, PostgresAdapter, TlsMode};
use dowai_runtime::{run_postgres, RunRequest, RunResult};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn env_var(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} is required"))
}

fn mysql_tls_required() -> bool {
    std::env::var("DOWAI_TEST_MYSQL_TLS").as_deref() == Ok("required")
}

fn mysql_profile() -> MysqlProfile {
    let mut profile = MysqlProfile::new(
        env_var("DOWAI_TEST_MYSQL_HOST"),
        env_var("DOWAI_TEST_MYSQL_PORT").parse().expect("port"),
        env_var("DOWAI_TEST_MYSQL_USER"),
        env_var("DOWAI_TEST_MYSQL_DATABASE"),
        "DOWAI_TEST_MYSQL_PASSWORD",
    );
    if !mysql_tls_required() {
        profile.set_tls_mode(TlsMode::Disabled);
    }
    profile
}

fn pg_profile() -> PgProfile {
    let mut profile = PgProfile::new(
        env_var("DOWAI_TEST_PG_HOST"),
        env_var("DOWAI_TEST_PG_PORT").parse().expect("port"),
        env_var("DOWAI_TEST_PG_USER"),
        env_var("DOWAI_TEST_PG_DATABASE"),
        "DOWAI_TEST_PG_PASSWORD",
    );
    if std::env::var("DOWAI_TEST_PG_TLS").as_deref() == Ok("disabled") {
        profile.set_tls_mode(TlsMode::Disabled);
    }
    profile
}

fn nonce() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos()
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

/// Remove as tabelas MySQL e PostgreSQL criadas, mesmo se uma asserção falhar.
struct Cleanup {
    mysql: Vec<String>,
    postgres: Vec<String>,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        if let Ok(mut adapter) = MysqlAdapter::connect(&mysql_profile()) {
            for table in &self.mysql {
                let _ = adapter.execute_sql(
                    &format!(
                        "DROP TABLE IF EXISTS {}",
                        my_ident(table).expect("identifier")
                    ),
                    &[],
                );
            }
        }
        if !self.postgres.is_empty() {
            if let Ok(mut adapter) = PostgresAdapter::connect(&pg_profile()) {
                for table in &self.postgres {
                    let quoted = quote_identifier(table).expect("identifier");
                    let _ = adapter.execute(&format!("DROP TABLE IF EXISTS {quoted}"), &[]);
                    let _ = adapter.execute(&format!("DROP VIEW IF EXISTS {quoted}"), &[]);
                }
            }
        }
    }
}

struct Fixture {
    project: PathBuf,
    _env: Vec<EnvironmentRestore>,
    _cleanup: Cleanup,
}

impl Fixture {
    /// Projeto com venv Python e os perfis `warehouse` (PostgreSQL, padrão) e `mart` (MySQL).
    fn new(test: &str, allow_external_writes: bool, mysql: &[&str], postgres: &[&str]) -> Self {
        let root = std::env::temp_dir().join(format!(
            "dowai-mysql-e2e-{test}-{}-{}",
            std::process::id(),
            nonce()
        ));
        let state = root.join("state");
        for dir in ["local", "roaming", "system"] {
            fs::create_dir_all(state.join(dir)).expect("create state dir");
        }
        let env = vec![
            EnvironmentRestore::set("LOCALAPPDATA", &state.join("local")),
            EnvironmentRestore::set("APPDATA", &state.join("roaming")),
            EnvironmentRestore::set("PROGRAMDATA", &state.join("system")),
            EnvironmentRestore::set("XDG_CONFIG_HOME", &state.join("roaming")),
            EnvironmentRestore::set("XDG_STATE_HOME", &state),
        ];
        let project = create_project(&root, "fixture").expect("initialize project");
        let venv = project.join(".venv");
        let created = Command::new("python3")
            .args(["-m", "venv", "--without-pip"])
            .arg(&venv)
            .output()
            .expect("create virtual environment");
        assert!(created.status.success(), "venv creation failed");

        let manifest_path = project.join("dowai_project.json");
        let mut manifest: Value =
            serde_json::from_slice(&fs::read(&manifest_path).expect("read manifest"))
                .expect("manifest is JSON");
        manifest["python"] = json!({
            "interpreter": venv.join("bin").join("python"),
            "timeout_seconds": 60
        });
        manifest["config"]["security"]["allow_external_writes"] = json!(allow_external_writes);
        fs::write(&manifest_path, serde_json::to_vec(&manifest).expect("json"))
            .expect("write manifest");

        let pg_tls = if std::env::var("DOWAI_TEST_PG_TLS").as_deref() == Ok("disabled") {
            "disabled"
        } else {
            "required"
        };
        let my_tls = if mysql_tls_required() {
            "required"
        } else {
            "disabled"
        };
        let workspace = json!({
            "schema_version": 2,
            "profiles": {
                "warehouse": {
                    "type": "postgres",
                    "host": env_var("DOWAI_TEST_PG_HOST"),
                    "port": env_var("DOWAI_TEST_PG_PORT").parse::<u16>().expect("port"),
                    "user": env_var("DOWAI_TEST_PG_USER"),
                    "database": env_var("DOWAI_TEST_PG_DATABASE"),
                    "password": "{{ env_var('DOWAI_TEST_PG_PASSWORD') }}",
                    "tls": pg_tls
                },
                "mart": {
                    "type": "mysql",
                    "host": env_var("DOWAI_TEST_MYSQL_HOST"),
                    "port": env_var("DOWAI_TEST_MYSQL_PORT").parse::<u16>().expect("port"),
                    "user": env_var("DOWAI_TEST_MYSQL_USER"),
                    "database": env_var("DOWAI_TEST_MYSQL_DATABASE"),
                    "password": "{{ env_var('DOWAI_TEST_MYSQL_PASSWORD') }}",
                    "tls": my_tls
                }
            },
            "defaults": { "execution": { "default_profile": "warehouse" } }
        });
        fs::write(
            root.join("profiles.json"),
            serde_json::to_vec(&workspace).expect("json"),
        )
        .expect("write workspace");
        Self {
            project,
            _env: env,
            _cleanup: Cleanup {
                mysql: mysql.iter().map(|name| (*name).to_owned()).collect(),
                postgres: postgres.iter().map(|name| (*name).to_owned()).collect(),
            },
        }
    }

    fn write_model(&self, file: &str, body: &str) {
        fs::write(self.project.join("models").join(file), body).expect("write model");
    }

    fn run(&self, target: &str) -> Result<RunResult, DowaiError> {
        self.run_with(target, false)
    }

    /// `confirm_destructive` allows a rerun to replace the PostgreSQL relation of a SQL model.
    fn run_with(&self, target: &str, confirm_destructive: bool) -> Result<RunResult, DowaiError> {
        run_postgres(
            &self.project,
            RunRequest {
                targets: vec![target.to_owned()],
                mode: None,
                profile: None,
                session_id: None,
                variables: BTreeMap::new(),
                confirm_destructive,
                full_refresh: false,
            },
        )
    }

    fn variables(&self, result: &RunResult) -> BTreeMap<String, Value> {
        SessionStore::for_user_state(default_session_store_path().expect("state path"))
            .snapshot(&result.session_id)
            .expect("session snapshot")
    }
}

fn mysql_exec(sql: &str) {
    MysqlAdapter::connect(&mysql_profile())
        .expect("connect MySQL")
        .execute_sql(sql, &[])
        .expect("execute MySQL test SQL");
}

/// Linhas `(id, name)` do MySQL, com NULL como `<NULL>`.
fn mysql_rows(table: &str) -> Vec<(i64, String)> {
    let mut adapter = MysqlAdapter::connect(&mysql_profile()).expect("connect MySQL");
    adapter
        .query_rows(
            &format!(
                "SELECT id, COALESCE(name, '<NULL>') FROM {} ORDER BY id",
                my_ident(table).expect("identifier")
            ),
            &[],
        )
        .expect("select MySQL")
        .into_iter()
        .map(|row| match row.as_slice() {
            [DatasetCell::Int(id), DatasetCell::Text(name)] => (*id, name.clone()),
            other => panic!("unexpected row {other:?}"),
        })
        .collect()
}

fn pair(id: i64, name: &str) -> (i64, String) {
    (id, name.to_owned())
}

const MODEL: &str = "writer.py";

#[test]
#[ignore = "opt-in E2E; requires disposable MySQL and PostgreSQL databases"]
/// `dowai run` grava no MySQL por `env.write` (append, upsert, replace) e a leitura confirma.
fn run_writes_to_mysql_append_upsert_replace() {
    let table = format!("em_rw_{}", nonce());
    let fx = Fixture::new("rw", true, &[&table], &[]);
    mysql_exec(&format!(
        "CREATE TABLE {} (id INT PRIMARY KEY, name VARCHAR(30) NULL)",
        my_ident(&table).expect("identifier")
    ));
    fx.write_model(
        MODEL,
        &r#"def model(env):
    a = env.write([{"id": 1, "name": "Ana"}, {"id": 2, "name": None}], target="mart", table="{T}")
    env.setVar("append_inserted", a["inserted"])
    u = env.write([{"id": 2, "name": "Bia"}, {"id": 3, "name": "Caio"}], target="mart", table="{T}", mode="upsert", keys=["id"])
    env.setVar("upsert_inserted", u["inserted"])
    env.setVar("upsert_updated", u["updated"])
    again = env.write([{"id": 2, "name": "Bia"}, {"id": 3, "name": "Caio"}], target="mart", table="{T}", mode="upsert", keys=["id"])
    env.setVar("again_inserted", again["inserted"])
    try:
        env.write([{"id": 9, "name": "x"}], target="mart", table="{T}", mode="replace")
    except ValueError as exc:
        env.setVar("replace_refused", "confirm_replace" in str(exc))
    try:
        env.write([{"id": 9, "nope": "x"}], target="mart", table="{T}")
    except Exception as exc:
        env.setVar("bad_column_code", exc.code)
    return None
"#
        .replace("{T}", &table),
    );
    let result = fx.run(MODEL).expect("run succeeds");
    assert_eq!(result.status, "SUCCEEDED");
    let variables = fx.variables(&result);
    assert_eq!(variables["append_inserted"], json!(2));
    assert_eq!(variables["upsert_inserted"], json!(1));
    assert_eq!(variables["upsert_updated"], json!(1));
    assert_eq!(variables["again_inserted"], json!(0));
    assert_eq!(variables["replace_refused"], json!(true));
    assert_eq!(
        variables["bad_column_code"],
        json!("DOWAI_WRITE_SCHEMA_MISMATCH")
    );
    assert_eq!(
        mysql_rows(&table),
        vec![pair(1, "Ana"), pair(2, "Bia"), pair(3, "Caio")]
    );

    fx.write_model(
        MODEL,
        &r#"def model(env):
    r = env.write([{"id": 7, "name": "fresh"}], target="mart", table="{T}", mode="replace", confirm_replace=True)
    env.setVar("deleted", r["deleted"])
    env.setVar("inserted", r["inserted"])
    return None
"#
        .replace("{T}", &table),
    );
    let replaced = fx.run(MODEL).expect("replace run succeeds");
    let variables = fx.variables(&replaced);
    assert_eq!(variables["deleted"], json!(3));
    assert_eq!(variables["inserted"], json!(1));
    assert_eq!(mysql_rows(&table), vec![pair(7, "fresh")]);
}

#[test]
#[ignore = "opt-in E2E; requires disposable MySQL and PostgreSQL databases"]
/// Falha no commit do replace desfaz o DELETE; upsert exige chave única; política respeitada.
fn failed_replace_rolls_back_and_upsert_requires_unique_key() {
    let table = format!("em_rb_{}", nonce());
    let plain = format!("em_pl_{}", nonce());
    let fx = Fixture::new("rb", true, &[&table, &plain], &[]);
    mysql_exec(&format!(
        "CREATE TABLE {} (id INT, name VARCHAR(30))",
        my_ident(&plain).expect("identifier")
    ));
    mysql_exec(&format!(
        "CREATE TABLE {} (id INT PRIMARY KEY, name VARCHAR(30))",
        my_ident(&table).expect("identifier")
    ));
    mysql_exec(&format!(
        "INSERT INTO {} VALUES (9, 'old')",
        my_ident(&table).expect("identifier")
    ));
    fx.write_model(
        MODEL,
        &r#"def model(env):
    try:
        env.write([{"id": 1, "name": "a"}, {"id": 1, "name": "b"}], target="mart", table="{T}", mode="replace", confirm_replace=True)
    except Exception as exc:
        env.setVar("replace_code", exc.code)
    try:
        env.write([{"id": 1, "name": "a"}], target="mart", table="{P}", mode="upsert", keys=["id"])
    except Exception as exc:
        env.setVar("upsert_code", exc.code)
    return None
"#
        .replace("{T}", &table)
        .replace("{P}", &plain),
    );
    let result = fx.run(MODEL).expect("run");
    let variables = fx.variables(&result);
    assert!(variables["replace_code"]
        .as_str()
        .is_some_and(|code| code.starts_with("DOWAI_")));
    assert_eq!(variables["upsert_code"], json!("DOWAI_WRITE_PRECONDITION"));
    assert_eq!(mysql_rows(&table), vec![pair(9, "old")]);
    assert_eq!(mysql_rows(&plain), vec![]);
}

#[test]
#[ignore = "opt-in E2E; requires disposable MySQL and PostgreSQL databases"]
/// Sem `security.allow_external_writes` a escrita no MySQL é recusada e nada é gravado.
fn mysql_write_is_denied_by_default_policy() {
    let table = format!("em_pol_{}", nonce());
    let fx = Fixture::new("policy", false, &[&table], &[]);
    mysql_exec(&format!(
        "CREATE TABLE {} (id INT PRIMARY KEY, name VARCHAR(30))",
        my_ident(&table).expect("identifier")
    ));
    fx.write_model(
        MODEL,
        &r#"def model(env):
    try:
        env.write([{"id": 1, "name": "a"}], target="mart", table="{T}")
    except Exception as exc:
        env.setVar("code", exc.code)
    return None
"#
        .replace("{T}", &table),
    );
    let outcome = fx.run(MODEL);
    assert!(
        matches!(&outcome, Err(DowaiError::Policy(message)) if message.contains("External writes are disabled")),
        "{outcome:?}"
    );
    assert_eq!(mysql_rows(&table), vec![]);
}

fn transfer_model(source: &str, table: &str, fail_after: bool) -> String {
    let tail = if fail_after {
        "    raise RuntimeError('failure after the MySQL commit')\n"
    } else {
        "    return None\n"
    };
    format!(
        "def model(env):\n    env.depends_on('{source}')\n    rows = [r for batch in env.ref('{source}').iter_batches(batch_size=2) for r in batch]\n    rows = [{{'id': r['id'], 'name': r['label']}} for r in rows]\n    result = env.write(rows, target='mart', table='{table}', mode='upsert', keys=['id'])\n    env.setVar('inserted', result['inserted'])\n{tail}"
    )
}

#[test]
#[ignore = "opt-in E2E; requires disposable MySQL and PostgreSQL databases"]
/// Transferência PostgreSQL -> MySQL: SQL no PostgreSQL, Python lê com `env.ref` e grava no
/// MySQL com upsert. Repetir a execução não duplica linhas.
fn transfer_postgres_to_mysql_is_repeatable() {
    let table = format!("em_xf_{}", nonce());
    let source = format!("em_src_{}", nonce());
    let fx = Fixture::new("xfer", true, &[&table], &[&source]);
    mysql_exec(&format!(
        "CREATE TABLE {} (id INT PRIMARY KEY, name VARCHAR(30) NULL)",
        my_ident(&table).expect("identifier")
    ));
    fx.write_model(
        &format!("{source}.sql"),
        "select g::int4 as id, ('v' || g)::text as label from generate_series(1, 5) as g",
    );
    fx.write_model("mover.py", &transfer_model(&source, &table, false));
    let expected: Vec<(i64, String)> = (1..=5).map(|id| pair(id, &format!("v{id}"))).collect();

    let first = fx.run("mover.py").expect("transfer run");
    assert_eq!(first.status, "SUCCEEDED");
    assert_eq!(fx.variables(&first)["inserted"], json!(5));
    assert_eq!(mysql_rows(&table), expected);

    let second = fx
        .run_with("mover.py", true)
        .expect("repeated transfer run");
    assert_eq!(second.status, "SUCCEEDED");
    assert_eq!(fx.variables(&second)["inserted"], json!(0));
    assert_eq!(mysql_rows(&table), expected);
}

#[test]
#[ignore = "opt-in E2E; requires disposable MySQL and PostgreSQL databases"]
/// Não há atomicidade entre bancos: a escrita no MySQL é confirmada em sua própria transação e
/// uma falha posterior na execução (lado PostgreSQL/Python) não a desfaz.
fn transfer_is_not_atomic_across_databases() {
    let table = format!("em_na_{}", nonce());
    let source = format!("em_nsrc_{}", nonce());
    let fx = Fixture::new("nonatomic", true, &[&table], &[&source]);
    mysql_exec(&format!(
        "CREATE TABLE {} (id INT PRIMARY KEY, name VARCHAR(30) NULL)",
        my_ident(&table).expect("identifier")
    ));
    fx.write_model(
        &format!("{source}.sql"),
        "select g::int4 as id, ('v' || g)::text as label from generate_series(1, 3) as g",
    );
    fx.write_model("mover.py", &transfer_model(&source, &table, true));

    let outcome = fx.run("mover.py");
    let failed = match &outcome {
        Err(_) => true,
        Ok(result) => result.status != "SUCCEEDED",
    };
    assert!(failed, "the run must fail after the MySQL write");
    assert_eq!(
        mysql_rows(&table),
        vec![pair(1, "v1"), pair(2, "v2"), pair(3, "v3")],
        "the MySQL write stays committed although the run failed"
    );
}

#[test]
#[ignore = "opt-in E2E; requires disposable MySQL and PostgreSQL databases"]
/// SQL models can target MySQL using the same profile selector as PostgreSQL.
fn mysql_profile_executes_sql_models() {
    let target = format!("dowai_sql_exec_{}", nonce());
    let fx = Fixture::new("exec", true, &[&target], &[]);
    fx.write_model("q.sql", "select 1 as id");
    let result = run_postgres(
        &fx.project,
        RunRequest {
            targets: vec!["q.sql".to_owned()],
            mode: None,
            profile: Some("mart".to_owned()),
            session_id: None,
            variables: BTreeMap::new(),
            confirm_destructive: false,
            full_refresh: false,
        },
    )
    .expect("MySQL profile runs SQL models");
    assert_eq!(result.status, "SUCCEEDED");
    let mut adapter = MysqlAdapter::connect(&mysql_profile()).expect("connect MySQL");
    let rows = adapter
        .query_rows(
            &format!("SELECT id FROM {}", my_ident(&target).expect("identifier")),
            &[],
        )
        .expect("read SQL model result");
    assert_eq!(rows, vec![vec![DatasetCell::Int(1)]]);
}
