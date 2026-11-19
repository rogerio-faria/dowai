//! ENT-114: `env.run("x.sql")` from a Python model, executed against PostgreSQL.
//!
//! Opt-in (`--ignored`): needs a disposable PostgreSQL database described by the
//! `DOWAI_TEST_PG_*` variables, plus a local `python3` able to create a venv. The model-free
//! refusals of this feature (paths outside `models/`, missing profile) run without PostgreSQL in
//! `nested_python_e2e.rs`.

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
            std::env::temp_dir().join(format!("dowai-nested-sql-{}-{id}", std::process::id()));
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
                "tls": if std::env::var("DOWAI_TEST_PG_TLS").as_deref() == Ok("disabled") {
                    "disabled" } else { "required" }
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
            tag: format!("nsql_{test}_{id}"),
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

/// SQL that declares a dependency on `dep` (a model stem) in the DAG. The planner ignores the
/// `ref` because it sits in a branch that is never rendered.
fn gated(dep: &str, body: &str) -> String {
    format!("{{% if var('never_rendered', false) %}}{{{{ ref('{dep}') }}}}{{% endif %}}{body}")
}

/// Invocations recorded for one target, in record order.
fn find<'a>(result: &'a RunResult, target: &str) -> Vec<&'a dowai_runtime::InvocationRecord> {
    result
        .invocations
        .iter()
        .filter(|invocation| invocation.target == target)
        .collect()
}

/// The SQL child reads `pre` (set by the parent before the call) and defines `from_sql`.
const CHILD_SQL: &str = "{{ def_var('from_sql', 42) }}\n{% if var('pre') == 7 %}select 5 as v{% else %}select 0 as v{% endif %}";

/// ENT-114 (AC: sucesso): the SQL call runs with the parent's profile, session and pending
/// variables, returns the same result object as `env.run("x.py")`, and leaves no static edge.
#[test]
#[ignore = "opt-in E2E; requires a dedicated disposable PostgreSQL database"]
fn ent114_env_run_sql_shares_profile_session_and_variables_without_a_static_edge() {
    let fx = Fixture::new("ok");
    let (child, parent) = (fx.name("child"), fx.name("parent"));
    let _cleanup = TableCleanup(vec![child.clone()]);
    fx.write_model(&format!("{child}.sql"), CHILD_SQL);
    fx.write_model(
        &format!("{parent}.py"),
        &format!(
            "def model(env):\n    env.setVar('pre', 7)\n    r = env.run('{child}.sql')\n    env.setVar('status', r.status)\n    env.setVar('run_id', r.run_id)\n    env.setVar('mode', r.execution_mode)\n    env.setVar('failed', r.failed)\n    env.setVar('saw_sql_var', env.getVar('from_sql'))\n"
        ),
    );

    let result = fx
        .run(&[&format!("{parent}.py")], None)
        .expect("parent run succeeds");

    assert_eq!(result.status, "SUCCEEDED");
    // The static DAG holds the parent only: the call added no node and no edge.
    assert_eq!(result.nodes.len(), 1);
    assert_eq!(result.nodes[0].id, format!("{parent}.py"));
    assert_eq!(fx.column(&child), [5]);

    let variables = fx.variables(&result.session_id);
    assert_eq!(variables["from_sql"], json!(42));
    assert_eq!(variables["saw_sql_var"], json!(42));
    assert_eq!(variables["pre"], json!(7));
    assert_eq!(variables["status"], json!("SUCCEEDED"));
    assert_eq!(variables["run_id"], json!(result.run_id));
    assert_eq!(variables["mode"], json!("automatic"));
    assert_eq!(variables["failed"], json!(false));

    let call = find(&result, &format!("{child}.sql"));
    assert_eq!(call.len(), 1);
    assert_eq!(call[0].call_type, "imperative");
    assert_eq!(call[0].status, "SUCCEEDED");
    let parent_record = find(&result, &format!("{parent}.py"));
    assert_eq!(call[0].parent_invocation_id, parent_record[0].invocation_id);
}

/// ENT-114 (AC: falha): an uncaught SQL failure stops the parent; later steps never start.
/// A caught failure keeps the parent running and discards the failed call's variables.
#[test]
#[ignore = "opt-in E2E; requires a dedicated disposable PostgreSQL database"]
fn ent114_failing_sql_call_stops_the_parent_unless_caught() {
    let fx = Fixture::new("fail");
    let (bad, after, caught) = (fx.name("bad"), fx.name("after"), fx.name("caught"));
    let _cleanup = TableCleanup(vec![after.clone(), caught.clone()]);
    fx.write_model(
        &format!("{bad}.sql"),
        "{{ def_var('never_kept', 1) }}\nselect * from ent114_missing_relation_for_failure",
    );
    fx.write_model(&format!("{after}.sql"), "select 1 as v");
    fx.write_model(&format!("{caught}.sql"), "select 2 as v");
    fx.write_model(
        &format!("uncaught_{}.py", fx.tag),
        &format!(
            "def model(env):\n    env.setVar('before', 1)\n    env.run('{bad}.sql')\n    env.run('{after}.sql')\n"
        ),
    );
    fx.write_model(
        &format!("caught_{}.py", fx.tag),
        &format!(
            "def model(env):\n    try:\n        env.run('{bad}.sql')\n    except env.DowaiExecutionError as error:\n        env.setVar('code', error.code)\n    env.run('{caught}.sql')\n"
        ),
    );

    let error = fx
        .run(&[&format!("uncaught_{}.py", fx.tag)], None)
        .expect_err("uncaught SQL failure fails the run");
    assert_eq!(error.code(), "DOWAI_EXECUTION_FAILED");
    assert!(
        !fx.table_exists(&after),
        "the step after a failed call must not start"
    );

    let result = fx
        .run(&[&format!("caught_{}.py", fx.tag)], None)
        .expect("parent may catch the SQL failure");
    let variables = fx.variables(&result.session_id);
    assert_eq!(variables["code"], json!("DOWAI_EXECUTION_FAILED"));
    assert!(!variables.contains_key("never_kept"));
    assert_eq!(fx.column(&caught), [2]);
    let failed = find(&result, &format!("{bad}.sql"));
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0].status, "FAILED");
    assert_eq!(
        failed[0].error_code.as_deref(),
        Some("DOWAI_EXECUTION_FAILED")
    );
}

/// ENT-114 (AC: predecessores e `env.manual`): in automatic mode the SQL predecessors of the called
/// model run first, recorded as `dependency`; inside `env.manual()` they do not run.
#[test]
#[ignore = "opt-in E2E; requires a dedicated disposable PostgreSQL database"]
fn ent114_automatic_call_runs_sql_predecessors_and_manual_scope_does_not() {
    let fx = Fixture::new("pred");
    let (pre, manual_target, auto_target) = (
        fx.name("pre"),
        fx.name("manual_target"),
        fx.name("auto_target"),
    );
    let _cleanup = TableCleanup(vec![
        pre.clone(),
        manual_target.clone(),
        auto_target.clone(),
    ]);
    fx.write_model(&format!("{pre}.sql"), "select 1 as v");
    fx.write_model(
        &format!("{manual_target}.sql"),
        &gated(&pre, "select 2 as v"),
    );
    fx.write_model(&format!("{auto_target}.sql"), &gated(&pre, "select 3 as v"));
    fx.write_model(
        &format!("manual_{}.py", fx.tag),
        &format!(
            "def model(env):\n    with env.manual():\n        env.run('{manual_target}.sql')\n"
        ),
    );
    fx.write_model(
        &format!("auto_{}.py", fx.tag),
        &format!("def model(env):\n    env.run('{auto_target}.sql')\n"),
    );

    let manual = fx
        .run(&[&format!("manual_{}.py", fx.tag)], None)
        .expect("manual scope run");
    assert!(
        !fx.table_exists(&pre),
        "manual scope must not run the predecessor"
    );
    assert_eq!(fx.column(&manual_target), [2]);
    assert!(find(&manual, &format!("{pre}.sql")).is_empty());
    assert_eq!(
        find(&manual, &format!("{manual_target}.sql"))[0].mode,
        "manual"
    );

    let automatic = fx
        .run(&[&format!("auto_{}.py", fx.tag)], None)
        .expect("automatic run");
    assert!(fx.table_exists(&pre), "automatic call runs the predecessor");
    assert_eq!(fx.column(&auto_target), [3]);
    let predecessor = find(&automatic, &format!("{pre}.sql"));
    assert_eq!(predecessor.len(), 1);
    assert_eq!(predecessor[0].call_type, "dependency");
    assert_eq!(predecessor[0].status, "SUCCEEDED");
}

/// ENT-114 in a mixed run: a SQL node of the plan defines a variable that the Python parent
/// reads, and the parent's nested SQL call reads it through the same session.
#[test]
#[ignore = "opt-in E2E; requires a dedicated disposable PostgreSQL database"]
fn ent114_nested_sql_call_inside_a_mixed_run_reads_the_plan_variables() {
    let fx = Fixture::new("mixed");
    let (base, child, parent) = (fx.name("base"), fx.name("child"), fx.name("parent"));
    let _cleanup = TableCleanup(vec![base.clone(), child.clone()]);
    fx.write_model(
        &format!("{base}.sql"),
        "{{ def_var('pre', 7) }}\nselect 1 as v",
    );
    fx.write_model(&format!("{child}.sql"), CHILD_SQL);
    fx.write_model(
        &format!("{parent}.py"),
        &format!(
            "def model(env):\n    env.depends_on('{base}')\n    env.run('{child}.sql')\n    env.setVar('saw_sql_var', env.getVar('from_sql'))\n"
        ),
    );

    let result = fx
        .run(&[&format!("{parent}.py")], None)
        .expect("mixed run with a nested SQL call");

    assert_eq!(result.status, "SUCCEEDED");
    assert_eq!(fx.column(&child), [5]);
    let variables = fx.variables(&result.session_id);
    assert_eq!(variables["pre"], json!(7));
    assert_eq!(variables["from_sql"], json!(42));
    assert_eq!(variables["saw_sql_var"], json!(42));
    assert_eq!(
        find(&result, &format!("{child}.sql"))[0].call_type,
        "imperative"
    );
}
