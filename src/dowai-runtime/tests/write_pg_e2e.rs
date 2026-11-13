//! E2E de `env.write` (ENT-048) contra PostgreSQL real, pelo runtime.
//!
//! Opt-in (`--ignored`): usa as variáveis `DOWAI_TEST_PG_*` (banco de teste descartável) e um
//! `python3` capaz de criar um venv. Cada teste cria tabelas próprias e as remove ao fim.
//! Os modelos usam `{T}` como marcador do nome da tabela de destino.

use dowai_core::{create_project, default_session_store_path, DowaiError, SessionStore};
use dowai_postgres::{quote_identifier, PgProfile, PostgresAdapter, TlsMode};
use dowai_runtime::{run_postgres, RunRequest};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn pg_env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} is required"))
}

fn test_profile() -> PgProfile {
    let mut profile = PgProfile::new(
        pg_env("DOWAI_TEST_PG_HOST"),
        pg_env("DOWAI_TEST_PG_PORT")
            .parse()
            .expect("valid test port"),
        pg_env("DOWAI_TEST_PG_USER"),
        pg_env("DOWAI_TEST_PG_DATABASE"),
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
        .expect("clock is after Unix epoch")
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

/// Drops the tables a test created, even when an assertion panics.
struct TableCleanup(Vec<String>);

impl Drop for TableCleanup {
    fn drop(&mut self) {
        if let Ok(mut adapter) = PostgresAdapter::connect(&test_profile()) {
            for table in &self.0 {
                if let Ok(identifier) = quote_identifier(table) {
                    let _ = adapter.execute(&format!("DROP TABLE IF EXISTS {identifier}"), &[]);
                }
            }
        }
    }
}

struct Fixture {
    project: PathBuf,
    _env: Vec<EnvironmentRestore>,
    _tables: TableCleanup,
}

impl Fixture {
    /// Project with a Python venv, a `warehouse` profile and `allow_external_writes` as given.
    fn new(test: &str, allow_external_writes: bool, tables: &[&str]) -> Self {
        let id = nonce();
        let root = std::env::temp_dir().join(format!(
            "dowai-write-e2e-{test}-{}-{id}",
            std::process::id()
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
            .expect("create project virtual environment");
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
        fs::write(
            &manifest_path,
            serde_json::to_vec(&manifest).expect("serialize"),
        )
        .expect("write manifest");

        let tls = if std::env::var("DOWAI_TEST_PG_TLS").as_deref() == Ok("disabled") {
            "disabled"
        } else {
            "required"
        };
        let workspace = json!({
            "schema_version": 2,
            "profiles": { "warehouse": {
                "type": "postgres",
                "host": pg_env("DOWAI_TEST_PG_HOST"),
                "port": pg_env("DOWAI_TEST_PG_PORT").parse::<u16>().expect("port"),
                "user": pg_env("DOWAI_TEST_PG_USER"),
                "database": pg_env("DOWAI_TEST_PG_DATABASE"),
                "password_env": "DOWAI_TEST_PG_PASSWORD",
                "tls": tls
            }},
            "defaults": { "execution": { "default_profile": "warehouse" } }
        });
        fs::write(
            root.join("workspace.json"),
            serde_json::to_vec(&workspace).expect("serialize"),
        )
        .expect("write workspace");

        Self {
            project,
            _env: env,
            _tables: TableCleanup(tables.iter().map(|table| (*table).to_owned()).collect()),
        }
    }

    /// Creates a destination table, dropping any leftover with the same name first.
    fn table(&self, name: &str, ddl: &str) {
        exec(&format!("DROP TABLE IF EXISTS {}", ident(name)));
        exec(&format!("CREATE TABLE {} ({ddl})", ident(name)));
    }

    /// Runs `body` (with `{T}` replaced by `table`) as the only Python model of a run.
    fn run_model(&self, body: &str, table: &str) -> Result<BTreeMap<String, Value>, DowaiError> {
        fs::write(
            self.project.join("models").join("writer.py"),
            body.replace("{T}", table),
        )
        .expect("write model");
        let result = run_postgres(
            &self.project,
            RunRequest {
                targets: vec!["writer.py".to_owned()],
                mode: None,
                profile: None,
                session_id: None,
                variables: BTreeMap::new(),
                confirm_destructive: false,
                full_refresh: false,
            },
        )?;
        assert_eq!(result.status, "SUCCEEDED");
        SessionStore::for_user_state(default_session_store_path().expect("state path"))
            .snapshot(&result.session_id)
            .map_err(|_| DowaiError::State("session snapshot failed".to_owned()))
    }
}

fn ident(name: &str) -> String {
    quote_identifier(name).expect("identifier")
}

fn exec(sql: &str) {
    let mut adapter = PostgresAdapter::connect(&test_profile()).expect("connect");
    adapter.execute(sql, &[]).expect("execute test SQL");
}

/// Text values of `select_list` for every row of `table`, in `order`.
fn rows_of(table: &str, select_list: &str, order: &str) -> Vec<Vec<String>> {
    let mut adapter = PostgresAdapter::connect(&test_profile()).expect("connect");
    let sql = format!(
        "SELECT {select_list} FROM {} ORDER BY {order}",
        ident(table)
    );
    let rows = adapter.query(&sql, &[]).expect("select destination");
    rows.iter()
        .map(|row| {
            (0..row.len())
                .map(|index| row.get::<_, String>(index))
                .collect()
        })
        .collect()
}

/// `(id, name)` of a destination with NULLs shown as `<NULL>`.
fn id_name(table: &str) -> Vec<Vec<String>> {
    rows_of(table, "id::text, COALESCE(name, '<NULL>')", "id, name")
}

fn row(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

#[test]
#[ignore = "opt-in E2E; requires a dedicated disposable PostgreSQL database"]
/// Append adds rows and keeps NULLs; the default mode is append, so a second call adds again.
fn append_writes_rows_with_nulls_and_reports_counts() {
    let table = format!("ew_app_{}", nonce());
    let fixture = Fixture::new("append", true, &[&table]);
    fixture.table(&table, "id integer, name text");

    let variables = fixture
        .run_model(
            r#"def model(env):
    rows = [{"id": 1, "name": "Ana"}, {"id": 2, "name": None}]
    result = env.write(rows, target="warehouse", table="{T}", mode="append")
    env.setVar("inserted", result["inserted"])
    env.setVar("mode", result["mode"])
    env.write(rows, target="warehouse", table="{T}")
    return None
"#,
            &table,
        )
        .expect("append run succeeds");

    assert_eq!(variables["inserted"], json!(2));
    assert_eq!(variables["mode"], json!("append"));
    assert_eq!(
        id_name(&table),
        vec![
            row(&["1", "Ana"]),
            row(&["1", "Ana"]),
            row(&["2", "<NULL>"]),
            row(&["2", "<NULL>"]),
        ]
    );
}

#[test]
#[ignore = "opt-in E2E; requires a dedicated disposable PostgreSQL database"]
/// Replace needs `confirm_replace=True`, and a failure at commit keeps the old rows: the
/// DELETE of the destination is rolled back with the rest of the transaction.
fn replace_needs_confirmation_and_failed_replace_keeps_destination() {
    let table = format!("ew_rep_{}", nonce());
    let fixture = Fixture::new("replace", true, &[&table]);
    fixture.table(&table, "id integer PRIMARY KEY, name text");
    exec(&format!("INSERT INTO {} VALUES (9, 'old')", ident(&table)));

    let variables = fixture
        .run_model(
            r#"def model(env):
    try:
        env.write([{"id": 1, "name": "new"}], target="warehouse", table="{T}", mode="replace")
    except ValueError as exc:
        env.setVar("unconfirmed", str(exc))
    try:
        # Duplicate ids pass the staging, but the INSERT into the destination violates the
        # primary key after the DELETE: the whole replace must roll back.
        env.write(
            [{"id": 1, "name": "a"}, {"id": 1, "name": "b"}],
            target="warehouse",
            table="{T}",
            mode="replace",
            confirm_replace=True,
        )
    except Exception as exc:
        env.setVar("failed_code", exc.code)
"#,
            &table,
        )
        .expect("refusal and failure are handled by the model");
    assert!(variables["unconfirmed"]
        .as_str()
        .is_some_and(|text| text.contains("confirm_replace")));
    assert!(variables["failed_code"]
        .as_str()
        .is_some_and(|code| code.starts_with("DOWAI_")));
    assert_eq!(id_name(&table), vec![row(&["9", "old"])]);

    let confirmed = fixture
        .run_model(
            r#"def model(env):
    result = env.write([{"id": 2, "name": "fresh"}], target="warehouse", table="{T}", mode="replace", confirm_replace=True)
    env.setVar("deleted", result["deleted"])
    env.setVar("inserted", result["inserted"])
"#,
            &table,
        )
        .expect("confirmed replace succeeds");
    assert_eq!(confirmed["deleted"], json!(1));
    assert_eq!(confirmed["inserted"], json!(1));
    assert_eq!(id_name(&table), vec![row(&["2", "fresh"])]);
}

#[test]
#[ignore = "opt-in E2E; requires a dedicated disposable PostgreSQL database"]
/// Upsert needs a unique key on exactly the keys; rerunning the same rows does not duplicate.
fn upsert_needs_unique_key_and_is_repeatable() {
    let plain = format!("ew_ups_plain_{}", nonce());
    let keyed = format!("ew_ups_key_{}", nonce());
    let fixture = Fixture::new("upsert", true, &[&plain, &keyed]);
    fixture.table(&plain, "id integer, name text");
    fixture.table(&keyed, "id integer PRIMARY KEY, name text");
    exec(&format!("INSERT INTO {} VALUES (1, 'old')", ident(&keyed)));

    let refused = fixture
        .run_model(
            r#"def model(env):
    try:
        env.write([{"id": 1, "name": "x"}], target="warehouse", table="{T}", mode="upsert", keys=["id"])
    except Exception as exc:
        env.setVar("code", exc.code)
"#,
            &plain,
        )
        .expect("refusal is handled by the model");
    assert_eq!(refused["code"], json!("DOWAI_WRITE_PRECONDITION"));
    assert!(rows_of(&plain, "id", "id").is_empty());

    let body = r#"def model(env):
    rows = [{"id": 1, "name": "new"}, {"id": 2, "name": "two"}]
    first = env.write(rows, target="warehouse", table="{T}", mode="upsert", keys=["id"])
    env.setVar("ins1", first["inserted"])
    env.setVar("upd1", first["updated"])
    second = env.write(rows, target="warehouse", table="{T}", mode="upsert", keys=["id"])
    env.setVar("ins2", second["inserted"])
    env.setVar("upd2", second["updated"])
"#;
    let variables = fixture
        .run_model(body, &keyed)
        .expect("upsert run succeeds");
    assert_eq!(variables["ins1"], json!(1));
    assert_eq!(variables["upd1"], json!(1));
    assert_eq!(variables["ins2"], json!(0));
    assert_eq!(variables["upd2"], json!(2));
    assert_eq!(
        id_name(&keyed),
        vec![row(&["1", "new"]), row(&["2", "two"])]
    );
}

#[test]
#[ignore = "opt-in E2E; requires a dedicated disposable PostgreSQL database"]
/// Without `security.allow_external_writes` the run is refused before any model executes
/// (existing run-level policy, exit 5), so nothing is written. The host check in `env.write`
/// is a second barrier for the same policy.
fn write_is_refused_without_external_write_policy() {
    let table = format!("ew_pol_{}", nonce());
    let fixture = Fixture::new("policy", false, &[&table]);
    fixture.table(&table, "id integer, name text");

    let error = fixture
        .run_model(
            r#"def model(env):
    env.write([{"id": 1, "name": "x"}], target="warehouse", table="{T}", mode="append")
"#,
            &table,
        )
        .expect_err("run must be refused without external write policy");
    assert_eq!(error.code(), "DOWAI_POLICY_DENIED");
    assert_eq!(error.exit_code(), 5);
    assert!(rows_of(&table, "id", "id").is_empty());
}

#[test]
#[ignore = "opt-in E2E; requires a dedicated disposable PostgreSQL database"]
/// A refused batch (missing column) and a database error both abort the session, so the next
/// write in the same model opens and commits normally.
fn failed_writes_abort_session_and_a_later_write_succeeds() {
    let table = format!("ew_inv_{}", nonce());
    let fixture = Fixture::new("invalid", true, &[&table]);
    fixture.table(&table, "id integer, name text");

    let variables = fixture
        .run_model(
            r#"def model(env):
    try:
        env.write([{"id": 1, "name": "ok"}, {"id": 2}], target="warehouse", table="{T}", mode="append")
    except ValueError as exc:
        env.setVar("missing_column", str(exc))
    try:
        env.write([{"id": "abc", "name": "bad"}], target="warehouse", table="{T}", mode="append")
    except Exception as exc:
        env.setVar("db_code", exc.code)
    result = env.write([{"id": 3, "name": "third"}], target="warehouse", table="{T}", mode="append")
    env.setVar("inserted", result["inserted"])
"#,
            &table,
        )
        .expect("failed writes are handled and the later write commits");
    assert!(variables["missing_column"]
        .as_str()
        .is_some_and(|text| text.contains("same columns")));
    assert!(variables["db_code"]
        .as_str()
        .is_some_and(|code| code.starts_with("DOWAI_")));
    assert_eq!(variables["inserted"], json!(1));
    assert_eq!(id_name(&table), vec![row(&["3", "third"])]);
}

#[test]
#[ignore = "opt-in E2E; requires a dedicated disposable PostgreSQL database"]
/// Typed values map to the destination types: exact decimal, aware datetime stored as the same
/// instant, booleans and NULL. A naive datetime is refused before anything is written.
fn typed_values_map_to_destination_types() {
    let table = format!("ew_typ_{}", nonce());
    let fixture = Fixture::new("types", true, &[&table]);
    fixture.table(
        &table,
        "id integer, amount numeric(10,2), at timestamptz, flag boolean, note text",
    );

    let variables = fixture
        .run_model(
            r#"import datetime
import decimal

def model(env):
    utc = datetime.timezone.utc
    env.write(
        [{"id": 1, "amount": decimal.Decimal("12.50"), "at": datetime.datetime(2026, 1, 1, 12, 0, tzinfo=utc), "flag": True, "note": None}],
        target="warehouse",
        table="{T}",
        mode="append",
    )
    try:
        env.write(
            [{"id": 2, "amount": decimal.Decimal("1"), "at": datetime.datetime(2026, 1, 1, 12, 0), "flag": False, "note": "x"}],
            target="warehouse",
            table="{T}",
            mode="append",
        )
    except ValueError as exc:
        env.setVar("naive", str(exc))
"#,
            &table,
        )
        .expect("typed write run succeeds");
    assert!(variables["naive"]
        .as_str()
        .is_some_and(|text| text.contains("naive datetime")));
    assert_eq!(
        rows_of(
            &table,
            "id::text, amount::text, to_char(at AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS'), flag::text, COALESCE(note, '<NULL>')",
            "id",
        ),
        vec![row(&["1", "12.50", "2026-01-01 12:00:00", "true", "<NULL>"])]
    );
}
