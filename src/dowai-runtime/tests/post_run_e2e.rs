//! ENT-126 (opt-in E2E, PostgreSQL de teste): `{{ run("x.py") }}` at the end of a SQL model runs
//! the Python file after the materialization, with the model as `origem`.
//!
//! Requires `DOWAI_TEST_PG_*` (dedicated disposable database) and a local `python3` able to
//! create a venv. Run with `--ignored --test-threads=1`.

use dowai_core::{create_project, default_session_store_path, ExecutionMode, SessionStore};
use dowai_postgres::{quote_identifier, PgProfile, PostgresAdapter, TlsMode};
use dowai_runtime::{run_postgres, RunRequest, RunResult};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

struct EnvironmentRestore {
    name: &'static str,
    previous: Option<OsString>,
}

impl EnvironmentRestore {
    fn set(name: &'static str, value: &OsString) -> Self {
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

fn pg_env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} is required"))
}

fn tls_disabled() -> bool {
    std::env::var("DOWAI_TEST_PG_TLS").as_deref() == Ok("disabled")
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
    if tls_disabled() {
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

struct Fixture {
    root: PathBuf,
    project: PathBuf,
    tag: String,
    _env: Vec<EnvironmentRestore>,
}

impl Fixture {
    fn new(test: &str) -> Self {
        let id = nonce();
        let root = std::env::temp_dir().join(format!("dowai-post-run-{}-{id}", std::process::id()));
        let state = root.join("state");
        for dir in ["local", "roaming", "system"] {
            fs::create_dir_all(state.join(dir)).expect("create state dir");
        }
        let env = vec![
            EnvironmentRestore::set("LOCALAPPDATA", &state.join("local").into_os_string()),
            EnvironmentRestore::set("APPDATA", &state.join("roaming").into_os_string()),
            EnvironmentRestore::set("PROGRAMDATA", &state.join("system").into_os_string()),
            EnvironmentRestore::set("XDG_CONFIG_HOME", &state.join("roaming").into_os_string()),
            EnvironmentRestore::set("XDG_STATE_HOME", &state.clone().into_os_string()),
        ];
        let project = create_project(&root, "fixture").expect("initialize project");
        let venv = project.join(".venv");
        let created = Command::new(if cfg!(windows) { "python" } else { "python3" })
            .args(["-m", "venv", "--without-pip"])
            .arg(&venv)
            .output()
            .expect("create project virtual environment");
        assert!(created.status.success(), "venv creation failed");
        let interpreter = if cfg!(windows) {
            venv.join("Scripts").join("python.exe")
        } else {
            venv.join("bin").join("python")
        };
        let manifest_path = project.join("dowai_project.json");
        let mut manifest: Value =
            serde_json::from_slice(&fs::read(&manifest_path).expect("read manifest"))
                .expect("manifest is JSON");
        manifest["python"] = json!({ "interpreter": interpreter, "timeout_seconds": 30 });
        manifest["config"]["security"]["allow_external_writes"] = json!(true);
        fs::write(
            &manifest_path,
            serde_json::to_vec(&manifest).expect("serialize"),
        )
        .expect("write manifest");
        let workspace = json!({
            "schema_version": 2,
            "profiles": { "warehouse": {
                "type": "postgres",
                "host": pg_env("DOWAI_TEST_PG_HOST"),
                "port": pg_env("DOWAI_TEST_PG_PORT").parse::<u16>().expect("port"),
                "user": pg_env("DOWAI_TEST_PG_USER"),
                "database": pg_env("DOWAI_TEST_PG_DATABASE"),
                "password": "{{ env_var('DOWAI_TEST_PG_PASSWORD') }}",
                "tls": if tls_disabled() { "disabled" } else { "required" }
            }},
            "defaults": { "execution": { "default_profile": "warehouse" } }
        });
        fs::write(
            root.join("profiles.json"),
            serde_json::to_vec(&workspace).expect("serialize"),
        )
        .expect("write workspace");
        Self {
            root,
            project,
            tag: format!("post_{test}_{id}"),
            _env: env,
        }
    }

    fn name(&self, suffix: &str) -> String {
        format!("{}_{suffix}", self.tag)
    }

    fn write_model(&self, file: &str, body: &str) {
        fs::write(self.project.join("models").join(file), body).expect("write model");
    }

    fn run(
        &self,
        targets: &[&str],
        mode: Option<ExecutionMode>,
    ) -> Result<RunResult, dowai_core::DowaiError> {
        run_postgres(
            &self.project,
            RunRequest {
                targets: targets.iter().map(|target| (*target).to_owned()).collect(),
                mode,
                profile: None,
                session_id: None,
                variables: BTreeMap::new(),
                confirm_destructive: false,
                full_refresh: false,
            },
        )
    }

    fn variables(&self, session_id: &str) -> BTreeMap<String, Value> {
        SessionStore::for_user_state(default_session_store_path().expect("state path"))
            .snapshot(session_id)
            .expect("session snapshot")
    }

    fn table_rows(&self, table: &str) -> Vec<i32> {
        let mut adapter = PostgresAdapter::connect(&test_profile()).expect("connect");
        let identifier = quote_identifier(table).expect("identifier");
        adapter
            .query(&format!("SELECT v FROM {identifier} ORDER BY v"), &[])
            .expect("query table")
            .iter()
            .map(|row| row.get::<_, i32>(0))
            .collect()
    }

    fn drop_table(&self, table: &str) {
        if let Ok(mut adapter) = PostgresAdapter::connect(&test_profile()) {
            if let Ok(identifier) = quote_identifier(table) {
                let _ = adapter.execute(&format!("DROP TABLE IF EXISTS {identifier}"), &[]);
            }
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// The post-run model reads its origin: the number of rows of the materialized SQL model.
const COUNT_ORIGIN: &str = "def model(env, origem=None):\n    if origem is None:\n        raise RuntimeError('origem missing')\n    total = sum(len(batch) for batch in origem.iter_batches(100))\n    env.setVar('seen', total)\n";

#[test]
#[ignore = "opt-in E2E; requires a dedicated disposable PostgreSQL database"]
fn run_executes_after_materialization_and_receives_the_sql_model_as_origem() {
    let fixture = Fixture::new("origin");
    let table = fixture.name("sales");
    fixture.write_model(
        &format!("{table}.sql"),
        "select g::int4 as v from generate_series(1, 4) as g\n{{ run(\"post.py\") }}\n",
    );
    fixture.write_model("post.py", COUNT_ORIGIN);

    let result = fixture
        .run(&[&format!("{table}.sql")], Some(ExecutionMode::Automatic))
        .expect("SQL model with run() should execute");

    assert_eq!(result.status, "SUCCEEDED");
    assert_eq!(
        fixture.table_rows(&table),
        vec![1, 2, 3, 4],
        "materialized before the call"
    );
    assert_eq!(fixture.variables(&result.session_id)["seen"], json!(4));
    let calls: Vec<_> = result
        .invocations
        .iter()
        .filter(|invocation| invocation.target == "post.py")
        .collect();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].call_type, "imperative");
    assert_eq!(calls[0].status, "SUCCEEDED");
    fixture.drop_table(&table);
}

#[test]
#[ignore = "opt-in E2E; requires a dedicated disposable PostgreSQL database"]
fn failing_post_run_keeps_the_materialization_and_publishes_no_variables() {
    let fixture = Fixture::new("failure");
    let table = fixture.name("kept");
    fixture.write_model(
        &format!("{table}.sql"),
        "select g::int4 as v from generate_series(1, 2) as g\n{{ run(\"boom.py\") }}\n",
    );
    fixture.write_model(
        "boom.py",
        "def model(env, origem=None):\n    env.setVar('should_not_publish', 1)\n    raise RuntimeError('post-run failed')\n",
    );

    let outcome = fixture.run(&[&format!("{table}.sql")], Some(ExecutionMode::Automatic));

    assert!(
        outcome.is_err(),
        "a failing post-run call must fail the run"
    );
    assert_eq!(
        fixture.table_rows(&table),
        vec![1, 2],
        "the materialization is not rolled back by a failed post-run call"
    );
    fixture.drop_table(&table);
}

#[test]
#[ignore = "opt-in E2E; requires a dedicated disposable PostgreSQL database"]
fn post_run_targets_execute_in_declared_order() {
    let fixture = Fixture::new("order");
    let table = fixture.name("ordered");
    fixture.write_model(
        &format!("{table}.sql"),
        "select 1 as v\n{{ run(\"first.py\") }}\n{{ run(\"second.py\") }}\n",
    );
    fixture.write_model(
        "first.py",
        "def model(env, origem=None):\n    env.setVar('order', 1)\n",
    );
    fixture.write_model(
        "second.py",
        "def model(env, origem=None):\n    assert env.getVar('order') == 1\n    env.setVar('order', 2)\n",
    );

    let result = fixture
        .run(&[&format!("{table}.sql")], Some(ExecutionMode::Manual))
        .expect("ordered post-run calls should execute");

    assert_eq!(result.status, "SUCCEEDED");
    assert_eq!(fixture.variables(&result.session_id)["order"], json!(2));
    fixture.drop_table(&table);
}

#[test]
#[ignore = "opt-in E2E; requires a dedicated disposable PostgreSQL database"]
fn legacy_post_run_signature_is_refused_when_an_origin_is_passed() {
    let fixture = Fixture::new("legacy");
    let table = fixture.name("legacy");
    fixture.write_model(
        &format!("{table}.sql"),
        "select 1 as v\n{{ run(\"legacy.py\") }}\n",
    );
    fixture.write_model("legacy.py", "def model(env):\n    pass\n");

    let error = fixture
        .run(&[&format!("{table}.sql")], Some(ExecutionMode::Automatic))
        .expect_err("a model without origem must not receive a SQL parent silently");

    assert_eq!(error.code(), "DOWAI_MODEL_SIGNATURE_INVALID");
    fixture.drop_table(&table);
}

/// ENT-127: the `examples/sql-run-post` model, as shipped, executed with its origin.
#[test]
#[ignore = "opt-in E2E; requires a dedicated disposable PostgreSQL database"]
fn example_sql_run_post_hands_the_materialized_model_to_the_python_file() {
    let fixture = Fixture::new("example");
    let table = fixture.name("carga");
    fixture.write_model(
        &format!("{table}.sql"),
        include_str!("../../../examples/sql-run-post/models/carga.sql"),
    );
    fixture.write_model(
        "transforma.py",
        include_str!("../../../examples/sql-run-post/models/transforma.py"),
    );

    let result = fixture
        .run(&[&format!("{table}.sql")], Some(ExecutionMode::Automatic))
        .expect("the shipped example should execute");

    assert_eq!(result.status, "SUCCEEDED");
    assert_eq!(fixture.table_rows(&table), vec![1, 2, 3]);
    let variables = fixture.variables(&result.session_id);
    assert_eq!(variables["origem_recebida"], json!(true));
    assert_eq!(variables["linhas_origem"], json!(3));
    fixture.drop_table(&table);
}

/// ENT-127: the same Python file run directly (`dowai run transforma.py`) receives `origem=None`
/// and handles it without error.
#[test]
#[ignore = "opt-in E2E; requires a dedicated disposable PostgreSQL database"]
fn example_python_treats_origem_none_when_run_directly() {
    let fixture = Fixture::new("example_none");
    fixture.write_model(
        "transforma.py",
        include_str!("../../../examples/sql-run-post/models/transforma.py"),
    );

    let result = fixture
        .run(&["transforma.py"], Some(ExecutionMode::Automatic))
        .expect("origem=None must be handled by the shipped example");

    assert_eq!(result.status, "SUCCEEDED");
    let variables = fixture.variables(&result.session_id);
    assert_eq!(variables["origem_recebida"], json!(false));
    assert_eq!(variables["linhas_origem"], json!(0));
}
