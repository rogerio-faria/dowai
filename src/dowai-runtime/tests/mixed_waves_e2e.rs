//! ENT-120c2: automatic mixed SQL + Python runs execute the nodes of a wave concurrently.
//!
//! Opt-in (`--ignored`): needs the disposable PostgreSQL database (`DOWAI_TEST_PG_*`, D-091) and a
//! local `python3` able to create a virtual environment. SQL intervals are read from the tables
//! the SQL models create; Python intervals from a shared timeline file. Both use the host clock.

use dowai_core::{create_project, DowaiError};
use dowai_postgres::{quote_identifier, PgProfile, PostgresAdapter, TlsMode};
use dowai_runtime::{run_postgres, RunRequest, RunResult};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

const SLEEP_SECONDS: f64 = 1.0;

static COUNTER: AtomicU64 = AtomicU64::new(0);

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

struct Fixture {
    root: PathBuf,
    project: PathBuf,
    tag: String,
    tables: Vec<String>,
    _env: Vec<EnvironmentRestore>,
}

impl Fixture {
    fn new(test: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock is after Unix epoch")
            .as_nanos();
        let id = COUNTER.fetch_add(1, Ordering::SeqCst);
        let root = std::env::temp_dir().join(format!(
            "dowai-mixed-waves-{}-{nonce}-{id}",
            std::process::id()
        ));
        let state = root.join("state");
        for dir in ["local", "roaming", "system"] {
            fs::create_dir_all(state.join(dir)).expect("create state directory");
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
        let created = Command::new("python3")
            .args(["-m", "venv", "--without-pip"])
            .arg(&venv)
            .output()
            .expect("create project virtual environment");
        assert!(created.status.success(), "venv creation failed");
        let manifest_path = project.join("dowai_project.json");
        let mut manifest: Value =
            serde_json::from_slice(&fs::read(&manifest_path).expect("read manifest"))
                .expect("JSON");
        manifest["config"]["security"]["allow_external_writes"] = json!(true);
        manifest["python"] =
            json!({ "interpreter": venv.join("bin").join("python"), "timeout_seconds": 60 });
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
                "password": "{{ env_var('DOWAI_TEST_PG_PASSWORD') }}",
                "tls": tls
            }},
            "defaults": { "execution": { "default_profile": "warehouse" } }
        });
        fs::write(
            root.join("profiles.json"),
            serde_json::to_vec(&workspace).expect("serialize"),
        )
        .expect("write workspace");
        Self {
            tag: format!("mxw_{test}_{nonce}_{id}"),
            root,
            project,
            tables: Vec::new(),
            _env: env,
        }
    }

    fn relation(&self, stem: &str) -> String {
        format!("{}_{stem}", self.tag)
    }

    /// Target of an SQL model: the file is named after its relation, unique to this test.
    fn sql_target(&self, stem: &str) -> String {
        format!("{}.sql", self.relation(stem))
    }

    fn write_sql(&mut self, stem: &str, body: &str) {
        self.tables.push(self.relation(stem));
        fs::write(
            self.project.join("models").join(self.sql_target(stem)),
            body,
        )
        .expect("write SQL model");
    }

    fn py_target(&self, stem: &str) -> String {
        format!("{}.py", self.relation(stem))
    }

    fn write_py(&self, stem: &str, body: &str) {
        fs::write(self.project.join("models").join(self.py_target(stem)), body)
            .expect("write Python model");
    }

    fn set_config(&self, overrides: Value) {
        let manifest = self.project.join("dowai_project.json");
        let mut config: Value =
            serde_json::from_slice(&fs::read(&manifest).expect("read")).expect("JSON");
        for (group, values) in overrides.as_object().expect("object") {
            for (key, value) in values.as_object().expect("object") {
                config["config"][group.as_str()][key.as_str()] = value.clone();
            }
        }
        fs::write(&manifest, serde_json::to_vec(&config).expect("serialize"))
            .expect("write manifest");
    }

    fn run(&self, targets: &[&str], confirm: bool) -> Result<RunResult, DowaiError> {
        run_postgres(
            &self.project,
            RunRequest {
                targets: targets.iter().map(|target| (*target).to_owned()).collect(),
                mode: None,
                profile: None,
                session_id: None,
                variables: BTreeMap::new(),
                confirm_destructive: confirm,
                full_refresh: false,
            },
        )
    }

    fn table_interval(&self, stem: &str) -> (f64, f64) {
        let mut adapter = PostgresAdapter::connect(&test_profile()).expect("connect");
        let identifier = quote_identifier(&self.relation(stem)).expect("identifier");
        let rows = adapter
            .query(&format!("SELECT extract(epoch from t0)::float8, extract(epoch from t1)::float8 FROM {identifier}"), &[])
            .expect("read interval");
        let row = rows.first().expect("one row");
        (row.get::<_, f64>(0), row.get::<_, f64>(1))
    }

    fn table_exists(&self, stem: &str) -> bool {
        let mut adapter = PostgresAdapter::connect(&test_profile()).expect("connect");
        let rows = adapter
            .query(
                "SELECT 1 FROM information_schema.tables WHERE table_name = $1",
                &[&self.relation(stem)],
            )
            .expect("catalog");
        !rows.is_empty()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Ok(mut adapter) = PostgresAdapter::connect(&test_profile()) {
            for table in &self.tables {
                if let Ok(identifier) = quote_identifier(table) {
                    let _ = adapter.execute(&format!("DROP TABLE IF EXISTS {identifier}"), &[]);
                }
            }
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn literal(path: &Path) -> String {
    serde_json::to_string(&path.display().to_string()).expect("path serializes")
}

fn timed_python(timeline: &Path, name: &str) -> String {
    format!(
        "import time\nLOG = {log}\ndef model(env):\n    start = time.time()\n    time.sleep({SLEEP_SECONDS})\n    end = time.time()\n    with open(LOG, 'a') as handle:\n        handle.write('{name} %r %r\\n' % (start, end))\n    return None\n",
        log = literal(timeline),
    )
}

fn timed_sql() -> String {
    format!("select clock_timestamp() as t0, (select pg_sleep({SLEEP_SECONDS}) is null) as slept, clock_timestamp() as t1")
}

fn peak_overlap(intervals: &[(f64, f64)]) -> usize {
    let mut events: Vec<(f64, i32)> = Vec::new();
    for (start, end) in intervals {
        events.push((*start, 1));
        events.push((*end, -1));
    }
    events.sort_by(|left, right| {
        left.0
            .partial_cmp(&right.0)
            .expect("finite")
            .then(left.1.cmp(&right.1))
    });
    let (mut current, mut peak) = (0_i32, 0_i32);
    for (_, delta) in events {
        current += delta;
        peak = peak.max(current);
    }
    usize::try_from(peak).expect("non-negative")
}

fn python_intervals(timeline: &Path) -> Vec<(f64, f64)> {
    fs::read_to_string(timeline)
        .expect("Python models wrote their timeline")
        .lines()
        .map(|line| {
            let mut parts = line.split_whitespace();
            let _name = parts.next();
            let start: f64 = parts.next().expect("start").parse().expect("numeric");
            let end: f64 = parts.next().expect("end").parse().expect("numeric");
            (start, end)
        })
        .collect()
}

#[test]
#[ignore = "requires the DOWAI_TEST_PG_* disposable PostgreSQL database (D-091)"]
fn sql_and_python_nodes_of_one_wave_overlap_up_to_the_limit() {
    for (limit, expected_peak) in [(4_u64, 4_usize), (2, 2)] {
        let mut fixture = Fixture::new(&format!("overlap{limit}"));
        let timeline = fixture.root.join(format!("timeline-{limit}.txt"));
        fixture.write_sql("a", &timed_sql());
        fixture.write_sql("b", &timed_sql());
        fixture.write_py("c", &timed_python(&timeline, "c"));
        fixture.write_py("d", &timed_python(&timeline, "d"));
        fixture.set_config(
            json!({ "runtime": { "max_parallel_nodes": limit, "max_worker_processes": 8 } }),
        );
        let (a, b, c, d) = (
            fixture.sql_target("a"),
            fixture.sql_target("b"),
            fixture.py_target("c"),
            fixture.py_target("d"),
        );
        let result = fixture
            .run(&[&a, &b, &c, &d], false)
            .expect("mixed independent nodes must run");
        assert_eq!(result.status, "SUCCEEDED");
        let mut intervals: Vec<(f64, f64)> =
            vec![fixture.table_interval("a"), fixture.table_interval("b")];
        intervals.extend(python_intervals(&timeline));
        assert_eq!(intervals.len(), 4);
        let peak = peak_overlap(&intervals);
        assert_eq!(peak, expected_peak, "limit {limit}: measured peak {peak}");
    }
}

#[test]
#[ignore = "requires the DOWAI_TEST_PG_* disposable PostgreSQL database (D-091)"]
fn sql_and_python_writers_of_one_variable_in_a_wave_conflict() {
    let mut fixture = Fixture::new("conflict");
    fixture.write_sql("sql_writer", "{{ def_var('shared', 1) }}select 1 as v");
    fixture.write_py(
        "py_writer",
        "def model(env):\n    env.setVar('shared', 2)\n    return None\n",
    );
    fixture
        .set_config(json!({ "runtime": { "max_parallel_nodes": 4, "max_worker_processes": 8 } }));
    let error = fixture
        .run(
            &[
                &fixture.sql_target("sql_writer"),
                &fixture.py_target("py_writer"),
            ],
            false,
        )
        .expect_err("two writers of one variable in a wave must conflict");
    assert_eq!(error.code(), "DOWAI_VARIABLE_CONFLICT");
}

#[test]
#[ignore = "requires the DOWAI_TEST_PG_* disposable PostgreSQL database (D-091)"]
fn failed_sql_node_halts_the_pending_python_node_of_its_wave() {
    let mut fixture = Fixture::new("failure");
    let marker = fixture.root.join("later-ran.txt");
    fixture.write_sql("a_broken", "select 1 / 0 as v");
    fixture.write_py(
        "b_later",
        &format!("from pathlib import Path\ndef model(env):\n    Path({}).write_text('ran')\n    return None\n", literal(&marker)),
    );
    fixture
        .set_config(json!({ "runtime": { "max_parallel_nodes": 1, "max_worker_processes": 8 } }));
    fixture
        .run(
            &[
                &fixture.sql_target("a_broken"),
                &fixture.py_target("b_later"),
            ],
            false,
        )
        .expect_err("a failing SQL node must fail the run");
    assert!(
        !marker.exists(),
        "pending Python node of the failed wave must not run"
    );
    assert!(
        !fixture.table_exists("a_broken"),
        "failed SQL node creates no relation"
    );
}
