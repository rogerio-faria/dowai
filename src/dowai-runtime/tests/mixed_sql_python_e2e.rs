//! Mixed SQL + Python execution in one run and one session (AC037, AC038).
//!
//! Opt-in (`--ignored`): needs a disposable PostgreSQL database described by the
//! `DOWAI_TEST_PG_*` variables, plus a local `python3` able to create a venv.
//!
//! Known limitation exercised by `failed_python_node_*`: SQL relations that were
//! already materialized are NOT rolled back when a later node fails. A run is not
//! a distributed transaction; only session variables are all-or-nothing.

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
    root: PathBuf,
    project: PathBuf,
    tag: String,
    _env: Vec<EnvironmentRestore>,
}

impl Fixture {
    fn new(test: &str) -> Self {
        let id = nonce();
        let root =
            std::env::temp_dir().join(format!("dowai-mixed-e2e-{}-{id}", std::process::id()));
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
                "password_env": "DOWAI_TEST_PG_PASSWORD",
                "tls": if std::env::var("DOWAI_TEST_PG_TLS").as_deref() == Ok("disabled") {
                    "disabled" } else { "required" }
            }},
            "defaults": { "execution": { "default_profile": "warehouse" } }
        });
        fs::write(
            root.join("workspace.json"),
            serde_json::to_vec(&workspace).expect("serialize"),
        )
        .expect("write workspace");
        Self {
            root,
            project,
            tag: format!("mx_{test}_{id}"),
            _env: env,
        }
    }

    /// Unique model/relation stem for this test.
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
        session_id: Option<String>,
    ) -> Result<RunResult, dowai_core::DowaiError> {
        run_postgres(
            &self.project,
            RunRequest {
                targets: targets.iter().map(|target| (*target).to_owned()).collect(),
                mode,
                profile: None,
                session_id,
                variables: BTreeMap::new(),
                confirm_destructive: false,
            },
        )
    }

    fn variables(&self, session_id: &str) -> BTreeMap<String, Value> {
        SessionStore::for_user_state(default_session_store_path().expect("state path"))
            .snapshot(session_id)
            .expect("session snapshot")
    }

    fn column(&self, table: &str) -> Vec<i32> {
        let mut adapter = PostgresAdapter::connect(&test_profile()).expect("connect");
        let identifier = quote_identifier(table).expect("identifier");
        adapter
            .query(&format!("SELECT v FROM {identifier} ORDER BY v"), &[])
            .expect("query table")
            .iter()
            .map(|row| row.get::<_, i32>(0))
            .collect()
    }

    fn table_exists(&self, table: &str) -> bool {
        let mut adapter = PostgresAdapter::connect(&test_profile()).expect("connect");
        let rows = adapter
            .query(
                "SELECT 1 FROM information_schema.tables WHERE table_name = $1",
                &[&table],
            )
            .expect("query catalog");
        !rows.is_empty()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// SQL that depends on `dep` (a Python model) in the DAG. The planner ignores SQL
/// comments and string literals, so the `ref` sits in a never-rendered template branch.
fn gated(dep: &str, body: &str) -> String {
    format!("{{% if var('never_rendered', false) %}}{{{{ ref('{dep}') }}}}{{% endif %}}{body}")
}

fn ids(result: &RunResult) -> Vec<&str> {
    result.nodes.iter().map(|node| node.id.as_str()).collect()
}

fn statuses(result: &RunResult) -> Vec<(&str, &str)> {
    result
        .nodes
        .iter()
        .map(|node| (node.materialization.as_str(), node.status.as_str()))
        .collect()
}

/// AC037: `def_var` in an earlier SQL model is visible to `env.getVar` in Python.
#[test]
#[ignore = "opt-in E2E; requires a dedicated disposable PostgreSQL database"]
fn ac037_sql_def_var_is_visible_to_following_python_model() {
    let fx = Fixture::new("ac037");
    let (sql, py) = (fx.name("producer"), fx.name("reader"));
    let _cleanup = TableCleanup(vec![sql.clone()]);
    fx.write_model(
        &format!("{sql}.sql"),
        "{{ def_var('carga_total', true) }}\nselect 1 as v",
    );
    fx.write_model(
        &format!("{py}.py"),
        &format!(
            "def model(env):\n    env.depends_on('{sql}')\n    assert env.getVar('carga_total') is True\n    env.setVar('py_saw_carga', True)\n    return None\n"
        ),
    );

    let result = fx
        .run(&[&format!("{py}.py")], None, None)
        .expect("mixed run");

    assert_eq!(result.status, "SUCCEEDED");
    assert_eq!(ids(&result), [format!("{sql}.sql"), format!("{py}.py")]);
    assert_eq!(
        statuses(&result),
        [("table", "SUCCEEDED"), ("none", "SUCCEEDED")]
    );
    assert_eq!(fx.column(&sql), [1]);
    let variables = fx.variables(&result.session_id);
    assert_eq!(variables["carga_total"], json!(true));
    assert_eq!(variables["py_saw_carga"], json!(true));
}

/// AC038: `env.setVar` in a Python model selects the SQL branch of the next model.
#[test]
#[ignore = "opt-in E2E; requires a dedicated disposable PostgreSQL database"]
fn ac038_python_set_var_selects_sql_branch_of_following_model() {
    let fx = Fixture::new("ac038");
    let (setter, on, off) = (fx.name("setter"), fx.name("on"), fx.name("off"));
    let _cleanup = TableCleanup(vec![on.clone(), off.clone()]);
    fx.write_model(
        &format!("{setter}.py"),
        "def model(env):\n    env.setVar('data_corte', '2026-01-31')\n    env.setVar('modo_off', False)\n    return None\n",
    );
    // The `ref` in a leading SQL comment declares the DAG edge to the Python node.
    fx.write_model(
        &format!("{on}.sql"),
        &gated(&setter, "{% if var('data_corte') %}select 1 as v union all select 2 as v{% else %}select 99 as v{% endif %}"),
    );
    fx.write_model(
        &format!("{off}.sql"),
        &gated(
            &setter,
            "{% if var('modo_off') %}select 7 as v{% else %}select 8 as v{% endif %}",
        ),
    );

    let result = fx
        .run(&[&format!("{on}.sql"), &format!("{off}.sql")], None, None)
        .expect("mixed run");

    assert_eq!(result.status, "SUCCEEDED");
    assert_eq!(ids(&result)[0], format!("{setter}.py"));
    assert_eq!(result.nodes.len(), 3);
    assert_eq!(fx.column(&on), [1, 2]);
    assert_eq!(fx.column(&off), [8]);
    assert_eq!(
        fx.variables(&result.session_id)["data_corte"],
        json!("2026-01-31")
    );
}

#[test]
#[ignore = "opt-in E2E; requires a dedicated disposable PostgreSQL database"]
fn automatic_plan_orders_mixed_dag_topologically() {
    let fx = Fixture::new("dag");
    let (base, mid, top, last) = (
        fx.name("base"),
        fx.name("mid"),
        fx.name("top"),
        fx.name("last"),
    );
    let _cleanup = TableCleanup(vec![base.clone(), top.clone()]);
    fx.write_model(
        &format!("{base}.sql"),
        "{{ def_var('step', 1) }}select 1 as v",
    );
    fx.write_model(
        &format!("{mid}.py"),
        &format!(
            "def model(env):\n    env.depends_on('{base}')\n    assert env.getVar('step') == 1\n    env.setVar('step', 2)\n    return None\n"
        ),
    );
    fx.write_model(
        &format!("{top}.sql"),
        &gated(
            &mid,
            "{% if var('step') %}select 2 as v{% else %}select 0 as v{% endif %}",
        ),
    );
    fx.write_model(
        &format!("{last}.py"),
        &format!(
            "def model(env):\n    env.depends_on('{top}')\n    assert env.getVar('step') == 2\n    return None\n"
        ),
    );

    let result = fx.run(&[&format!("{last}.py")], None, None).expect("run");

    assert_eq!(
        ids(&result),
        [
            format!("{base}.sql"),
            format!("{mid}.py"),
            format!("{top}.sql"),
            format!("{last}.py")
        ]
    );
    assert_eq!(
        statuses(&result),
        [
            ("table", "SUCCEEDED"),
            ("none", "SUCCEEDED"),
            ("table", "SUCCEEDED"),
            ("none", "SUCCEEDED")
        ]
    );
    assert_eq!(fx.column(&top), [2]);
    assert_eq!(fx.variables(&result.session_id)["step"], json!(2));
}

/// A failed Python node stops the run: later SQL does not execute and no variable is
/// published. The earlier SQL table is intentionally NOT rolled back (limitation:
/// external SQL effects are not part of a distributed transaction).
#[test]
#[ignore = "opt-in E2E; requires a dedicated disposable PostgreSQL database"]
fn failed_python_node_stops_run_and_publishes_no_variables_but_keeps_sql_effects() {
    let fx = Fixture::new("fail");
    let (first, boom, after) = (fx.name("first"), fx.name("boom"), fx.name("after"));
    let _cleanup = TableCleanup(vec![first.clone(), after.clone()]);
    let seed = fx.name("seed");
    fx.write_model(
        &format!("{seed}.py"),
        "def model(env):\n    env.setVar('baseline', 1)\n",
    );
    let session = fx
        .run(&[&format!("{seed}.py")], None, None)
        .expect("seed session")
        .session_id;

    fx.write_model(
        &format!("{first}.sql"),
        "{{ def_var('uncommitted_sql', true) }}select 1 as v",
    );
    fx.write_model(
        &format!("{boom}.py"),
        &format!(
            "def model(env):\n    env.depends_on('{first}')\n    env.setVar('uncommitted_py', True)\n    raise RuntimeError('private detail')\n"
        ),
    );
    fx.write_model(&format!("{after}.sql"), &gated(&boom, "select 1 as v"));

    let error = fx
        .run(&[&format!("{after}.sql")], None, Some(session.clone()))
        .expect_err("python failure must fail the run");

    assert_eq!(error.code(), "DOWAI_PYTHON_FAILED");
    assert!(!error.message().contains("private detail"));
    assert!(!fx.table_exists(&after), "later SQL node must not run");
    assert!(
        fx.table_exists(&first),
        "limitation: earlier SQL effects are not rolled back"
    );
    let variables = fx.variables(&session);
    assert_eq!(variables["baseline"], json!(1));
    assert!(!variables.contains_key("uncommitted_sql"));
    assert!(!variables.contains_key("uncommitted_py"));
}

#[test]
#[ignore = "opt-in E2E; requires a dedicated disposable PostgreSQL database"]
fn manual_mode_respects_the_given_order() {
    let fx = Fixture::new("manual");
    let (sql, py) = (fx.name("definer"), fx.name("probe"));
    let _cleanup = TableCleanup(vec![sql.clone()]);
    fx.write_model(
        &format!("{sql}.sql"),
        "{{ def_var('manual_flag', 'x') }}select 1 as v",
    );
    fx.write_model(
        &format!("{py}.py"),
        "def model(env):\n    env.setVar('saw', 'missing' if env.getVar('manual_flag') is None else 'present')\n    return None\n",
    );
    let (sql_file, py_file) = (format!("{sql}.sql"), format!("{py}.py"));

    let probe_first = fx
        .run(&[&py_file, &sql_file], Some(ExecutionMode::Manual), None)
        .expect("manual run python first");
    assert_eq!(ids(&probe_first), [py_file.as_str(), sql_file.as_str()]);
    assert_eq!(
        fx.variables(&probe_first.session_id)["saw"],
        json!("missing")
    );

    let definer_first = fx
        .run(&[&sql_file, &py_file], Some(ExecutionMode::Manual), None)
        .expect_err("replacement of the existing table needs approval");
    assert_eq!(definer_first.code(), "DOWAI_POLICY_DENIED");

    let request = RunRequest {
        targets: vec![sql_file.clone(), py_file.clone()],
        mode: Some(ExecutionMode::Manual),
        profile: None,
        session_id: None,
        variables: BTreeMap::new(),
        confirm_destructive: true,
    };
    let result = run_postgres(&fx.project, request.clone()).expect("manual run sql first");
    assert_eq!(ids(&result), [sql_file.as_str(), py_file.as_str()]);
    assert_eq!(fx.variables(&result.session_id)["saw"], json!("present"));
}

#[test]
#[ignore = "opt-in E2E; requires a dedicated disposable PostgreSQL database"]
fn second_mixed_run_sees_variables_committed_by_the_session() {
    let fx = Fixture::new("session");
    let (sql1, py1) = (fx.name("one"), fx.name("probe"));
    let sql2 = fx.name("two");
    let _cleanup = TableCleanup(vec![sql1.clone(), sql2.clone()]);
    fx.write_model(
        &format!("{sql1}.sql"),
        "{{ def_var('carga_total', true) }}select 1 as v",
    );
    fx.write_model(
        &format!("{py1}.py"),
        &format!("def model(env):\n    env.depends_on('{sql1}')\n    env.setVar('first_run', 1)\n"),
    );
    let first = fx
        .run(&[&format!("{py1}.py")], None, None)
        .expect("first run");

    let reader = fx.name("reader");
    fx.write_model(
        &format!("{reader}.py"),
        "def model(env):\n    assert env.getVar('carga_total') is True\n    assert env.getVar('first_run') == 1\n    env.setVar('second_run', 2)\n",
    );
    fx.write_model(
        &format!("{sql2}.sql"),
        &gated(
            &reader,
            "{% if var('carga_total') %}select 5 as v{% else %}select 6 as v{% endif %}",
        ),
    );
    let second = fx
        .run(
            &[&format!("{sql2}.sql")],
            None,
            Some(first.session_id.clone()),
        )
        .expect("second run");

    assert_eq!(second.session_id, first.session_id);
    assert_eq!(
        ids(&second),
        [format!("{reader}.py"), format!("{sql2}.sql")]
    );
    assert_eq!(fx.column(&sql2), [5]);
    let variables = fx.variables(&second.session_id);
    assert_eq!(variables["first_run"], json!(1));
    assert_eq!(variables["second_run"], json!(2));
}

/// AC002: a Python model reads the schema of a SQL predecessor declared with `env.ref`.
/// The predecessor is committed first (automatic order), so the schema is the real one.
#[test]
#[ignore = "opt-in E2E; requires a dedicated disposable PostgreSQL database"]
fn ac002_python_ref_reads_schema_of_committed_sql_predecessor() {
    let fx = Fixture::new("ac002-schema");
    let (sql, py) = (fx.name("upstream"), fx.name("reader"));
    let _cleanup = TableCleanup(vec![sql.clone()]);
    fx.write_model(
        &format!("{sql}.sql"),
        "select 1::int4 as id, 'x'::text as label",
    );
    fx.write_model(
        &format!("{py}.py"),
        &format!(
            "def model(env):\n    env.depends_on('{sql}')\n    columns = env.ref('{sql}').schema()\n    env.setVar('ref_columns', ','.join(c['name'] for c in columns))\n    env.setVar('ref_types', ','.join(c['type'] for c in columns))\n    return None\n"
        ),
    );

    let result = fx
        .run(&[&format!("{py}.py")], None, None)
        .expect("mixed run");

    assert_eq!(result.status, "SUCCEEDED");
    assert_eq!(ids(&result), [format!("{sql}.sql"), format!("{py}.py")]);
    let variables = fx.variables(&result.session_id);
    assert_eq!(variables["ref_columns"], json!("id,label"));
    assert_eq!(variables["ref_types"], json!("int4,text"));
}

/// AC002: `env.ref` to a Python model is refused with a stable code, not read as a table.
#[test]
#[ignore = "opt-in E2E; requires a dedicated disposable PostgreSQL database"]
fn ac002_python_ref_refuses_a_python_model_with_stable_code() {
    let fx = Fixture::new("ac002-not-sql");
    let (sql, py_dep, py) = (fx.name("filler"), fx.name("helper"), fx.name("reader"));
    let _cleanup = TableCleanup(vec![sql.clone()]);
    fx.write_model(&format!("{sql}.sql"), "select 1 as id");
    fx.write_model(
        &format!("{py_dep}.py"),
        "def model(env):\n    return None\n",
    );
    fx.write_model(
        &format!("{py}.py"),
        &format!(
            "def model(env):\n    env.depends_on('{sql}')\n    try:\n        env.ref('{py_dep}').schema()\n    except Exception as error:\n        env.setVar('ref_code', error.code)\n    return None\n"
        ),
    );

    let result = fx
        .run(&[&format!("{py}.py")], None, None)
        .expect("mixed run");

    assert_eq!(result.status, "SUCCEEDED");
    let variables = fx.variables(&result.session_id);
    assert_eq!(variables["ref_code"], json!("DOWAI_DATASET_NOT_SQL"));
}
