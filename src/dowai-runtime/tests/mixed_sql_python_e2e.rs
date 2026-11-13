//! Mixed SQL + Python execution in one run and one session (AC037, AC038).
//!
//! Opt-in (`--ignored`): needs a disposable PostgreSQL database described by the
//! `DOWAI_TEST_PG_*` variables, plus a local `python3` able to create a venv.
//!
//! Known limitation exercised by `failed_python_node_*`: SQL relations that were
//! already materialized are NOT rolled back when a later node fails. A run is not
//! a distributed transaction; only session variables are all-or-nothing.

use dowai_core::{create_project, default_session_store_path, ExecutionMode, SessionStore};
use dowai_core::{default_run_store_path, RunStore};
use dowai_postgres::{quote_identifier, PgProfile, PostgresAdapter, TlsMode};
use dowai_runtime::{retry_run_with_cancel, run_postgres, RetryRequest, RunRequest, RunResult};
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
                full_refresh: false,
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

impl Fixture {
    /// Replaces the fixture's venv with a copy of the locked venv of ENT-047 (pandas, polars,
    /// pyarrow). The runtime only accepts an interpreter inside the project directory, so the
    /// copy must live in `.venv` of the fixture; `cp -a` keeps the venv's symlinks.
    fn use_dataframe_venv(&self) {
        let source = std::env::var("DOWAI_TEST_DATAFRAME_VENV")
            .expect("DOWAI_TEST_DATAFRAME_VENV is required (locked venv of tests/contracts/python-dataframes)");
        let venv = self.project.join(".venv");
        fs::remove_dir_all(&venv).expect("remove bare venv");
        let copied = Command::new("cp")
            .arg("-a")
            .arg(&source)
            .arg(&venv)
            .output()
            .expect("copy locked venv");
        assert!(copied.status.success(), "copy of the locked venv failed");
        let interpreter = venv.join("bin").join("python");
        let path = self.project.join("dowai_project.json");
        let mut manifest: Value = serde_json::from_slice(&fs::read(&path).expect("read manifest"))
            .expect("manifest is JSON");
        manifest["python"]["interpreter"] = json!(interpreter);
        fs::write(&path, serde_json::to_vec(&manifest).expect("serialize"))
            .expect("write manifest");
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
        full_refresh: false,
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
    fx.write_model(&format!("{sql}.sql"), "select 1 as v");
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

/// AC002 (B-09.2): `iter_rows` reads a SQL predecessor in batches. Covers batch boundaries,
/// exact `numeric` text, booleans, NULLs and the worker-side batch-size validation.
#[test]
#[ignore = "opt-in E2E; requires a dedicated disposable PostgreSQL database"]
fn ac002_iter_rows_reads_batches_with_exact_types() {
    let fx = Fixture::new("ac002-rows");
    let (sql, py) = (fx.name("upstream"), fx.name("reader"));
    let _cleanup = TableCleanup(vec![sql.clone()]);
    fx.write_model(
        &format!("{sql}.sql"),
        "select g::int4 as id, ('v' || g)::text as label, (g * 1.5)::numeric(10,2) as amount, (g % 2 = 0) as even, null::text as empty from generate_series(1, 25) as g",
    );
    fx.write_model(
        &format!("{py}.py"),
        &format!(
            "def model(env):\n    env.depends_on('{sql}')\n    rows = list(env.ref('{sql}').iter_rows(batch_size=10))\n    env.setVar('row_count', len(rows))\n    env.setVar('id_sum', sum(r['id'] for r in rows))\n    first = rows[0]\n    env.setVar('first_label', first['label'])\n    env.setVar('first_amount', first['amount'])\n    env.setVar('first_even', first['even'])\n    env.setVar('first_empty_is_null', first['empty'] is None)\n    try:\n        env.ref('{sql}').iter_rows(batch_size=0)\n    except ValueError:\n        env.setVar('bad_batch_refused', True)\n    return None\n"
        ),
    );

    let result = fx
        .run(&[&format!("{py}.py")], None, None)
        .expect("mixed run");

    assert_eq!(result.status, "SUCCEEDED");
    let variables = fx.variables(&result.session_id);
    assert_eq!(variables["row_count"], json!(25));
    assert_eq!(variables["id_sum"], json!(325));
    assert_eq!(variables["first_label"], json!("v1"));
    assert_eq!(variables["first_amount"], json!("1.50"));
    assert_eq!(variables["first_even"], json!(false));
    assert_eq!(variables["first_empty_is_null"], json!(true));
    assert_eq!(variables["bad_batch_refused"], json!(true));
}

/// AC002 (B-09.2): views have no physical order (`ctid`), so `iter_rows` refuses them with a
/// stable code instead of returning an unordered or partial read. `schema` still works.
#[test]
#[ignore = "opt-in E2E; requires a dedicated disposable PostgreSQL database"]
fn ac002_iter_rows_refuses_view_predecessor_with_stable_code() {
    let fx = Fixture::new("ac002-view");
    let (sql, py) = (fx.name("view_upstream"), fx.name("reader"));
    let _cleanup = TableCleanup(vec![sql.clone()]);
    fx.write_model(
        &format!("{sql}.sql"),
        "{{ config(materialized='view') }}\nselect 1::int4 as id",
    );
    fx.write_model(
        &format!("{py}.py"),
        &format!(
            "def model(env):\n    env.depends_on('{sql}')\n    env.setVar('schema_ok', len(env.ref('{sql}').schema()) == 1)\n    try:\n        list(env.ref('{sql}').iter_rows())\n    except Exception as error:\n        env.setVar('view_code', error.code)\n    return None\n"
        ),
    );

    let result = fx
        .run(&[&format!("{py}.py")], None, None)
        .expect("mixed run");

    assert_eq!(result.status, "SUCCEEDED");
    let variables = fx.variables(&result.session_id);
    assert_eq!(variables["schema_ok"], json!(true));
    assert_eq!(variables["view_code"], json!("DOWAI_DATASET_UNAVAILABLE"));
}

/// B-05c: `--failed-only` re-runs the failed Python model and not the committed SQL
/// predecessor. The SQL relation stays as it was, and the retry commits the Python model.
#[test]
#[ignore = "opt-in E2E; requires a dedicated disposable PostgreSQL database"]
fn retry_failed_only_skips_committed_sql_predecessor() {
    let fx = Fixture::new("retry");
    let (sql, py) = (fx.name("warehouse"), fx.name("consumer"));
    let _cleanup = TableCleanup(vec![sql.clone()]);
    let marker = fx.root.join("consumer-first-attempt-done");
    fx.write_model(&format!("{sql}.sql"), "select 1 as v");
    fx.write_model(
        &format!("{py}.py"),
        &format!(
            "import pathlib\ndef model(env):\n    env.depends_on('{sql}')\n    marker = pathlib.Path({:?})\n    if not marker.exists():\n        marker.write_text('x')\n        raise RuntimeError('first attempt fails')\n    env.setVar('consumer_ok', True)\n",
            marker.to_string_lossy()
        ),
    );

    let failure = fx
        .run(&[&format!("{py}.py")], None, None)
        .expect_err("first attempt must fail");
    assert_eq!(failure.code(), "DOWAI_PYTHON_FAILED");
    assert!(fx.table_exists(&sql), "the SQL predecessor committed");
    let failed_id = RunStore::for_user_state(default_run_store_path().expect("journal path"))
        .list(1)
        .expect("list runs")[0]
        .run_id
        .clone();

    let retried = retry_run_with_cancel(
        &fx.project,
        &failed_id,
        RetryRequest {
            failed_only: true,
            variables: BTreeMap::new(),
            confirm_destructive: false,
        },
        &dowai_core::CancellationToken::new(),
    )
    .expect("selective retry");

    assert_eq!(retried.status, "SUCCEEDED");
    assert_eq!(ids(&retried), [format!("{py}.py")]);
    assert_eq!(fx.column(&sql), [1]);
    assert_eq!(
        fx.variables(&retried.session_id)["consumer_ok"],
        json!(true)
    );
}

/// ENT-041: `select` and `filter` run in SQL (pushdown); `iter_batches` returns only the rows
/// the predicates keep, `is_null` included, and batch boundaries do not drop matches.
#[test]
#[ignore = "opt-in E2E; requires a dedicated disposable PostgreSQL database"]
fn ent041_select_and_filter_are_pushed_and_batches_return_only_matches() {
    let fx = Fixture::new("ent041-push");
    let (sql, py) = (fx.name("upstream"), fx.name("reader"));
    let _cleanup = TableCleanup(vec![sql.clone()]);
    fx.write_model(
        &format!("{sql}.sql"),
        "select g::int4 as id, ('v' || g)::text as label, (g % 2 = 0) as even, null::text as empty from generate_series(1, 25) as g",
    );
    fx.write_model(
        &format!("{py}.py"),
        &format!(
            "def model(env):\n    env.depends_on('{sql}')\n    ds = env.ref('{sql}')\n    view = ds.select('id', 'label').filter('id', '>', 10).filter('even', '=', True)\n    rows = [row for batch in view.iter_batches(batch_size=4) for row in batch]\n    env.setVar('row_count', len(rows))\n    env.setVar('ids', ','.join(str(r['id']) for r in rows))\n    env.setVar('keys', ','.join(sorted(rows[0].keys())))\n    nulls = list(ds.select('id').filter('empty', 'is_null').iter_rows(batch_size=10))\n    env.setVar('null_count', len(nulls))\n    return None\n"
        ),
    );

    let result = fx
        .run(&[&format!("{py}.py")], None, None)
        .expect("mixed run");

    assert_eq!(result.status, "SUCCEEDED");
    assert!(
        !result
            .warnings
            .iter()
            .any(|warning| warning.starts_with("DOWAI_DATASET_FILTER_NOT_PUSHED")),
        "pushed filters must not warn: {:?}",
        result.warnings
    );
    let variables = fx.variables(&result.session_id);
    assert_eq!(variables["row_count"], json!(7));
    assert_eq!(variables["ids"], json!("12,14,16,18,20,22,24"));
    assert_eq!(variables["keys"], json!("id,label"));
    assert_eq!(variables["null_count"], json!(25));
}

/// ENT-041: an equality on a text-read column (`numeric`) is applied by the worker and reported
/// as a warning; ordered comparisons on it, unknown projections and pickling are refused with
/// stable codes.
#[test]
#[ignore = "opt-in E2E; requires a dedicated disposable PostgreSQL database"]
fn ent041_fallback_is_warned_and_refusals_have_stable_codes() {
    let fx = Fixture::new("ent041-fallback");
    let (sql, py) = (fx.name("upstream"), fx.name("reader"));
    let _cleanup = TableCleanup(vec![sql.clone()]);
    fx.write_model(
        &format!("{sql}.sql"),
        "select g::int4 as id, (g * 1.5)::numeric(10,2) as amount from generate_series(1, 5) as g",
    );
    fx.write_model(
        &format!("{py}.py"),
        &format!(
            "def model(env):\n    env.depends_on('{sql}')\n    ds = env.ref('{sql}')\n    matches = list(ds.filter('amount', '=', '1.50').iter_rows(batch_size=10))\n    env.setVar('fallback_count', len(matches))\n    env.setVar('fallback_id', matches[0]['id'])\n    try:\n        list(ds.filter('amount', '>', '1').iter_rows(batch_size=10))\n    except Exception as error:\n        env.setVar('ordered_code', error.code)\n    try:\n        list(ds.select('nope').iter_rows(batch_size=10))\n    except Exception as error:\n        env.setVar('select_code', error.code)\n    try:\n        import pickle\n        pickle.dumps(ds)\n    except TypeError:\n        env.setVar('pickle_refused', True)\n    return None\n"
        ),
    );

    let result = fx
        .run(&[&format!("{py}.py")], None, None)
        .expect("mixed run");

    assert_eq!(result.status, "SUCCEEDED");
    assert!(
        result
            .warnings
            .iter()
            .any(|warning| warning.starts_with("DOWAI_DATASET_FILTER_NOT_PUSHED")),
        "fallback must be observable: {:?}",
        result.warnings
    );
    let variables = fx.variables(&result.session_id);
    assert_eq!(variables["fallback_count"], json!(1));
    assert_eq!(variables["fallback_id"], json!(1));
    assert_eq!(
        variables["ordered_code"],
        json!("DOWAI_DATASET_FILTER_UNSUPPORTED")
    );
    assert_eq!(
        variables["select_code"],
        json!("DOWAI_DATASET_INVALID_QUERY")
    );
    assert_eq!(variables["pickle_refused"], json!(true));
}

/// ENT-042: `env.source(name, table)` reads a table declared by the project, with its schema
/// and in batches, and refuses an undeclared source or table with a stable code.
#[test]
#[ignore = "opt-in E2E; requires a dedicated disposable PostgreSQL database"]
fn ent042_declared_source_is_read_and_undeclared_source_is_refused() {
    let fx = Fixture::new("ent042-source");
    let (table, py, sql) = (fx.name("raw_users"), fx.name("reader"), fx.name("anchor"));
    let _cleanup = TableCleanup(vec![table.clone(), sql.clone()]);
    // A SQL predecessor makes this a mixed run: datasets (ref and source) exist only in mixed runs.
    fx.write_model(&format!("{sql}.sql"), "select 1::int4 as v");
    {
        let mut adapter = PostgresAdapter::connect(&test_profile()).expect("connect");
        let identifier = quote_identifier(&table).expect("identifier");
        adapter
            .execute(
                &format!("CREATE TABLE {identifier} (id int4, label text)"),
                &[],
            )
            .expect("create source table");
        adapter
            .execute(
                &format!("INSERT INTO {identifier} VALUES (1, 'a'), (2, 'b')"),
                &[],
            )
            .expect("seed source table");
    }
    let manifest_path = fx.project.join("dowai_project.json");
    let mut manifest: Value =
        serde_json::from_slice(&fs::read(&manifest_path).expect("read manifest"))
            .expect("manifest is JSON");
    manifest["sources"] = json!({
        "raw": { "schema": "public", "tables": { "users": table } }
    });
    fs::write(
        &manifest_path,
        serde_json::to_vec(&manifest).expect("serialize"),
    )
    .expect("write manifest");
    fx.write_model(
        &format!("{py}.py"),
        &format!(
            "def model(env):\n\
         \x20   env.depends_on('{sql}')\n\
         \x20   columns = env.source('raw', 'users').schema()\n\
         \x20   env.setVar('src_columns', ','.join(c['name'] for c in columns))\n\
         \x20   rows = list(env.source('raw', 'users').iter_rows(batch_size=1))\n\
         \x20   env.setVar('src_rows', len(rows))\n\
         \x20   env.setVar('src_id_sum', sum(r['id'] for r in rows))\n\
         \x20   for name, tab in [('other', 'users'), ('raw', 'missing')]:\n\
         \x20       try:\n\
         \x20           env.source(name, tab).schema()\n\
         \x20       except Exception as error:\n\
         \x20           env.setVar('undeclared_' + name + '_' + tab, error.code)\n\
         \x20   try:\n\
         \x20       env.source('', 'users')\n\
         \x20   except ValueError:\n\
         \x20       env.setVar('empty_name_refused', True)\n\
         \x20   return None\n"
        ),
    );

    let result = fx
        .run(&[&format!("{py}.py")], None, None)
        .expect("source run");

    assert_eq!(result.status, "SUCCEEDED");
    let variables = fx.variables(&result.session_id);
    assert_eq!(variables["src_columns"], json!("id,label"));
    assert_eq!(variables["src_rows"], json!(2));
    assert_eq!(variables["src_id_sum"], json!(3));
    assert_eq!(
        variables["undeclared_other_users"],
        json!("DOWAI_DATASET_SOURCE_UNDECLARED")
    );
    assert_eq!(
        variables["undeclared_raw_missing"],
        json!("DOWAI_DATASET_SOURCE_UNDECLARED")
    );
    assert_eq!(variables["empty_name_refused"], json!(true));
}

const FRAME_SOURCE_SQL: &str = "select 1::int4 as id, 'ação ✓'::text as label, true as flag, 9007199254740993::int8 as big, 0.1::float4 as f4, 0.1::float8 as f8, 1.50::numeric as amount, '2026-11-13 10:00:00'::timestamp as created, '2026-11-13 10:00:00+05:30'::timestamptz as created_tz, '2026-11-13'::date as day union all select 2, null, null, null, null, null, null, null, null, null";

const FRAME_MODEL: &str = r#"import datetime
import decimal
import struct

import pandas as pd
import polars as pl


def model(env):
    env.depends_on('SOURCE')
    ds = env.ref('SOURCE')

    def expect(condition, label):
        if not condition:
            raise RuntimeError('frame check failed: ' + label)

    arrow = ds.to_arrow()
    expect([str(field.type) for field in arrow.schema] == [
        'int32', 'string', 'bool', 'int64', 'float', 'double', 'decimal128(3, 2)',
        'timestamp[us]', 'timestamp[us, tz=UTC]', 'date32[day]'], 'arrow types')
    first, second = arrow.to_pylist()
    expect(first['label'] == 'ação ✓', 'utf-8 text')
    expect(first['big'] == 9007199254740993, 'int8 is exact')
    expect(first['f4'] == struct.unpack('f', struct.pack('f', 0.1))[0], 'float4 is the nearest float32')
    expect(first['f8'] == 0.1, 'float8 is exact')
    expect(first['amount'] == decimal.Decimal('1.50'), 'numeric keeps scale')
    expect(first['created'] == datetime.datetime(2026, 11, 13, 10, 0), 'timestamp without zone')
    expect(first['created_tz'] == datetime.datetime(2026, 11, 13, 4, 30, tzinfo=datetime.timezone.utc), 'timestamptz to UTC')
    expect(first['day'] == datetime.date(2026, 11, 13), 'date')
    expect(second['id'] == 2 and all(value is None for key, value in second.items() if key != 'id'), 'nulls')

    pdf = ds.to_pandas()
    expect([str(dtype) for dtype in pdf.dtypes] == [
        'Int32', 'string', 'boolean', 'Int64', 'Float32', 'Float64', 'object',
        'datetime64[us]', 'datetime64[us, UTC]', 'object'], 'pandas dtypes')
    expect(pdf.loc[0, 'label'] == 'ação ✓', 'pandas utf-8')
    expect(pdf.loc[0, 'amount'] == decimal.Decimal('1.50'), 'pandas numeric')
    expect(pd.isna(pdf.loc[1, 'amount']) and pd.isna(pdf.loc[1, 'created_tz']), 'pandas nulls')

    polars = ds.to_polars()
    expect(polars.schema['amount'] == pl.Decimal(3, 2), 'polars decimal')
    expect(polars.schema['created_tz'] == pl.Datetime('us', 'UTC'), 'polars timestamptz')
    expect(polars.schema['created'] == pl.Datetime('us'), 'polars timestamp')
    expect(polars['created_tz'][0] == datetime.datetime(2026, 11, 13, 4, 30, tzinfo=datetime.timezone.utc), 'polars UTC value')
    expect(polars['f4'][0] == struct.unpack('f', struct.pack('f', 0.1))[0], 'polars float4')
    expect(polars['label'][0] == 'ação ✓', 'polars utf-8')
    env.setVar('frames_ok', True)
    return None
"#;

/// ENT-047: `to_arrow`, `to_pandas` and `to_polars` keep nulls, UTF-8 text, exact integers and
/// numerics, float4 rounding, and timestamptz converted to UTC, with explicit frame types.
#[test]
#[ignore = "opt-in E2E; requires PostgreSQL and DOWAI_TEST_DATAFRAME_PYTHON"]
fn ent047_frames_map_types_explicitly() {
    let fx = Fixture::new("ent047-frames");
    fx.use_dataframe_venv();
    let (sql, py) = (fx.name("upstream"), fx.name("frames"));
    fx.write_model(&format!("{sql}.sql"), FRAME_SOURCE_SQL);
    fx.write_model(&format!("{py}.py"), &FRAME_MODEL.replace("SOURCE", &sql));

    let result = fx
        .run(&[&format!("{py}.py")], None, None)
        .expect("mixed run");

    assert_eq!(result.status, "SUCCEEDED");
    assert_eq!(fx.variables(&result.session_id)["frames_ok"], json!(true));
}

/// ENT-047: a missing frame library is refused with a stable code, and a missing type mapping
/// is refused without guessing, while `max_rows` bounds materialization.
#[test]
#[ignore = "opt-in E2E; requires PostgreSQL and DOWAI_TEST_DATAFRAME_PYTHON"]
fn ent047_refusals_have_stable_codes_and_max_rows_bounds_memory() {
    let fx = Fixture::new("ent047-refusals");
    fx.use_dataframe_venv();
    let (sql, py) = (fx.name("upstream"), fx.name("refusals"));
    fx.write_model(
        &format!("{sql}.sql"),
        "select 1::int4 as id, '{\"a\": 1}'::json as doc union all select 2, null",
    );
    fx.write_model(
        &format!("{py}.py"),
        &format!(
            "def model(env):\n    env.depends_on('{sql}')\n    ds = env.ref('{sql}')\n    env.setVar('at_limit', ds.select('id').to_arrow(max_rows=2).num_rows)\n    try:\n        ds.select('id').to_arrow(max_rows=1)\n    except Exception as error:\n        env.setVar('too_large_code', error.code)\n    try:\n        ds.to_polars(max_rows=0)\n    except ValueError:\n        env.setVar('bad_max_rows', True)\n    try:\n        ds.to_pandas()\n    except Exception as error:\n        env.setVar('json_code', error.code)\n    env.setVar('selected_rows', ds.select('id').to_pandas().shape[0])\n    return None\n"
        ),
    );

    let result = fx
        .run(&[&format!("{py}.py")], None, None)
        .expect("mixed run");

    assert_eq!(result.status, "SUCCEEDED");
    let variables = fx.variables(&result.session_id);
    assert_eq!(variables["at_limit"], json!(2));
    assert_eq!(
        variables["too_large_code"],
        json!("DOWAI_DATASET_TOO_LARGE")
    );
    assert_eq!(variables["bad_max_rows"], json!(true));
    assert_eq!(
        variables["json_code"],
        json!("DOWAI_DATASET_TYPE_UNSUPPORTED")
    );
    assert_eq!(variables["selected_rows"], json!(2));
}

/// ENT-047: without the frame libraries in the project venv, each `to_*` fails with a stable code.
#[test]
#[ignore = "opt-in E2E; requires a dedicated disposable PostgreSQL database"]
fn ent047_missing_frame_library_is_refused_with_stable_code() {
    let fx = Fixture::new("ent047-missing");
    let (sql, py) = (fx.name("upstream"), fx.name("missing"));
    fx.write_model(&format!("{sql}.sql"), "select 1::int4 as id");
    fx.write_model(
        &format!("{py}.py"),
        &format!(
            "def model(env):\n    env.depends_on('{sql}')\n    ds = env.ref('{sql}')\n    for name in ('to_arrow', 'to_pandas', 'to_polars'):\n        try:\n            getattr(ds, name)()\n        except Exception as error:\n            env.setVar(name, error.code)\n    return None\n"
        ),
    );

    let result = fx
        .run(&[&format!("{py}.py")], None, None)
        .expect("mixed run");

    assert_eq!(result.status, "SUCCEEDED");
    let variables = fx.variables(&result.session_id);
    for name in ["to_arrow", "to_pandas", "to_polars"] {
        assert_eq!(
            variables[name],
            json!("DOWAI_DATASET_LIBRARY_UNAVAILABLE"),
            "{name} must refuse with a stable code"
        );
    }
}

/// ENT-111: a Python-only run reads a declared source. There is no SQL model in the plan, so the
/// run has no profile of its own; the dataset connection opens on the first read.
#[test]
#[ignore = "opt-in E2E; requires a dedicated disposable PostgreSQL database"]
fn ent111_python_only_run_reads_declared_source() {
    let fx = Fixture::new("ent111-source");
    let (table, py) = (fx.name("raw_users"), fx.name("reader"));
    let _cleanup = TableCleanup(vec![table.clone()]);
    {
        let mut adapter = PostgresAdapter::connect(&test_profile()).expect("connect");
        let identifier = quote_identifier(&table).expect("identifier");
        adapter
            .execute(
                &format!("CREATE TABLE {identifier} (id int4, label text)"),
                &[],
            )
            .expect("create source table");
        adapter
            .execute(
                &format!("INSERT INTO {identifier} VALUES (1, 'a'), (2, 'b')"),
                &[],
            )
            .expect("seed source table");
    }
    let manifest_path = fx.project.join("dowai_project.json");
    let mut manifest: Value =
        serde_json::from_slice(&fs::read(&manifest_path).expect("read manifest"))
            .expect("manifest is JSON");
    manifest["sources"] = json!({
        "raw": { "schema": "public", "tables": { "users": table } }
    });
    fs::write(
        &manifest_path,
        serde_json::to_vec(&manifest).expect("serialize"),
    )
    .expect("write manifest");
    fx.write_model(
        &format!("{py}.py"),
        "def model(env):\n    rows = list(env.source('raw', 'users').iter_rows(batch_size=1))\n    env.setVar('py_rows', len(rows))\n    env.setVar('py_id_sum', sum(r['id'] for r in rows))\n    return None\n",
    );

    let result = fx
        .run(&[&format!("{py}.py")], None, None)
        .expect("Python-only run with a declared source");

    assert_eq!(result.status, "SUCCEEDED");
    assert_eq!(result.profile, "python");
    let variables = fx.variables(&result.session_id);
    assert_eq!(variables["py_rows"], json!(2));
    assert_eq!(variables["py_id_sum"], json!(3));
}

/// ENT-111: without a dataset profile, a Python-only run that does not read datasets succeeds,
/// and a declared-source read fails with a catchable `DOWAI_DATASET_UNAVAILABLE`, never a
/// connection attempt or a credential in the message.
#[test]
#[ignore = "opt-in E2E; needs python3 and the DOWAI_TEST_PG_* variables for the fixture"]
fn ent111_python_only_without_profile_keeps_plain_runs_and_refuses_source_reads() {
    let fx = Fixture::new("ent111-noprofile");
    let (plain, reader) = (fx.name("plain"), fx.name("reader"));
    // No profile in the workspace: nothing can be resolved for datasets.
    fs::write(fx.root.join("workspace.json"), b"{\"schema_version\":2}").expect("workspace");
    let manifest_path = fx.project.join("dowai_project.json");
    let mut manifest: Value =
        serde_json::from_slice(&fs::read(&manifest_path).expect("read manifest"))
            .expect("manifest is JSON");
    manifest["sources"] = json!({
        "raw": { "schema": "public", "tables": { "users": "unused_table" } }
    });
    fs::write(
        &manifest_path,
        serde_json::to_vec(&manifest).expect("serialize"),
    )
    .expect("write manifest");
    fx.write_model(
        &format!("{plain}.py"),
        "def model(env):\n    env.setVar('plain_ok', True)\n    return None\n",
    );
    fx.write_model(
        &format!("{reader}.py"),
        "def model(env):\n    try:\n        env.source('raw', 'users').schema()\n    except Exception as error:\n        env.setVar('source_code', error.code)\n    return None\n",
    );

    let plain_result = fx
        .run(&[&format!("{plain}.py")], None, None)
        .expect("plain Python-only run needs no dataset profile");
    assert_eq!(plain_result.status, "SUCCEEDED");

    let result = fx
        .run(&[&format!("{reader}.py")], None, None)
        .expect("reader handles the refusal and succeeds");
    assert_eq!(result.status, "SUCCEEDED");
    let variables = fx.variables(&result.session_id);
    assert_eq!(variables["source_code"], json!("DOWAI_DATASET_UNAVAILABLE"));
}
