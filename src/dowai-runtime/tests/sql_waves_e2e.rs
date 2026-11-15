//! ENT-120b: automatic SQL runs execute the nodes of a wave concurrently (ADR 0008).
//!
//! Opt-in (`--ignored`): needs the disposable PostgreSQL database described by the
//! `DOWAI_TEST_PG_*` variables (D-091). Each test creates its own project and drops its tables.
//!
//! Overlap is measured inside PostgreSQL: every model records `clock_timestamp()` before and
//! after a `pg_sleep`, and the test reads those intervals back from the tables the models create.

use dowai_core::{create_project, default_run_store_path, DowaiError, RunStore};
use dowai_postgres::{quote_identifier, PgProfile, PostgresAdapter, TlsMode};
use dowai_runtime::{run_postgres, RunRequest, RunResult};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

const SLEEP_SECONDS: f64 = 0.8;

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

/// A project with its own state directory, a PostgreSQL profile and a unique table prefix.
/// Tables created by the test are dropped on drop, even when an assertion panics.
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
            "dowai-sql-waves-{}-{nonce}-{id}",
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
        let manifest_path = project.join("dowai_project.json");
        let mut manifest: Value =
            serde_json::from_slice(&fs::read(&manifest_path).expect("read manifest"))
                .expect("manifest is JSON");
        manifest["config"]["security"]["allow_external_writes"] = json!(true);
        fs::write(
            &manifest_path,
            serde_json::to_vec(&manifest).expect("serialize manifest"),
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
            serde_json::to_vec(&workspace).expect("serialize workspace"),
        )
        .expect("write workspace");
        Self {
            root,
            project,
            tag: format!("sqlw_{test}_{nonce}_{id}"),
            tables: Vec::new(),
            _env: env,
        }
    }

    /// Relation name of a model of this fixture.
    fn relation(&self, stem: &str) -> String {
        format!("{}_{stem}", self.tag)
    }

    /// Target of a model of this fixture: the model file is named after its relation, so the
    /// relation (and the table the test drops) is unique to this test.
    fn target(&self, stem: &str) -> String {
        format!("{}.sql", self.relation(stem))
    }

    fn write_model(&mut self, stem: &str, body: &str) {
        self.tables.push(self.relation(stem));
        fs::write(self.project.join("models").join(self.target(stem)), body)
            .expect("write SQL model");
    }

    fn set_config(&self, overrides: Value) {
        let manifest = self.project.join("dowai_project.json");
        let mut config: Value =
            serde_json::from_slice(&fs::read(&manifest).expect("read manifest"))
                .expect("manifest is JSON");
        for (group, values) in overrides.as_object().expect("overrides are an object") {
            for (key, value) in values.as_object().expect("group overrides are an object") {
                config["config"][group.as_str()][key.as_str()] = value.clone();
            }
        }
        fs::write(
            &manifest,
            serde_json::to_vec(&config).expect("serialize manifest"),
        )
        .expect("write manifest");
    }

    fn run(&self, targets: &[&str]) -> Result<RunResult, DowaiError> {
        self.run_with(targets, false)
    }

    /// Runs with `confirm_destructive` set, which approves replacing relations created by an
    /// earlier run of the same test.
    fn run_with(
        &self,
        targets: &[&str],
        confirm_destructive: bool,
    ) -> Result<RunResult, DowaiError> {
        run_postgres(
            &self.project,
            RunRequest {
                targets: targets.iter().map(|target| (*target).to_owned()).collect(),
                mode: None,
                profile: None,
                session_id: None,
                variables: BTreeMap::new(),
                confirm_destructive,
                full_refresh: false,
            },
        )
    }

    fn table_exists(&self, stem: &str) -> bool {
        let mut adapter = PostgresAdapter::connect(&test_profile()).expect("connect");
        let relation = self.relation(stem);
        let rows = adapter
            .query(
                "SELECT 1 FROM information_schema.tables WHERE table_name = $1",
                &[&relation],
            )
            .expect("query information_schema");
        !rows.is_empty()
    }

    /// Reads the `(start, end)` interval recorded by a model's table, in seconds.
    fn interval(&self, stem: &str) -> (f64, f64) {
        let mut adapter = PostgresAdapter::connect(&test_profile()).expect("connect");
        let identifier = quote_identifier(&self.relation(stem)).expect("identifier");
        let rows = adapter
            .query(
                &format!("SELECT extract(epoch from t0)::float8, extract(epoch from t1)::float8 FROM {identifier}"),
                &[],
            )
            .expect("read interval");
        let row = rows.first().expect("model table has one row");
        (row.get::<_, f64>(0), row.get::<_, f64>(1))
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

/// Model body recording its own execution interval inside PostgreSQL.
fn timed_model(sleep: f64) -> String {
    format!(
        "select clock_timestamp() as t0, (select pg_sleep({sleep}) is null) as slept, \
         clock_timestamp() as t1"
    )
}

/// Largest number of intervals that overlap at one instant.
fn peak_overlap(intervals: &[(f64, f64)]) -> usize {
    let mut events: Vec<(f64, i32)> = Vec::new();
    for (start, end) in intervals {
        events.push((*start, 1));
        events.push((*end, -1));
    }
    // Ends sort before starts at the same instant: touching intervals do not overlap.
    events.sort_by(|left, right| {
        left.0
            .partial_cmp(&right.0)
            .expect("finite timestamps")
            .then(left.1.cmp(&right.1))
    });
    let mut current = 0_i32;
    let mut peak = 0_i32;
    for (_, delta) in events {
        current += delta;
        peak = peak.max(current);
    }
    usize::try_from(peak).expect("peak is non-negative")
}

#[test]
#[ignore = "requires the DOWAI_TEST_PG_* disposable PostgreSQL database (D-091)"]
fn four_independent_sql_nodes_overlap_up_to_the_limit() {
    for (limit, expected_peak) in [(4_u64, 4_usize), (2, 2)] {
        let mut fixture = Fixture::new(&format!("overlap{limit}"));
        let stems = ["a", "b", "c", "d"];
        for stem in stems {
            fixture.write_model(stem, &timed_model(SLEEP_SECONDS));
        }
        fixture.set_config(json!({ "runtime": {
            "max_parallel_nodes": limit,
            "max_worker_processes": 8,
        }}));
        let targets: Vec<String> = stems.iter().map(|stem| fixture.target(stem)).collect();
        let targets: Vec<&str> = targets.iter().map(String::as_str).collect();

        let result = fixture
            .run(&targets)
            .expect("independent SQL nodes must run");
        assert_eq!(result.status, "SUCCEEDED");

        let intervals: Vec<(f64, f64)> = stems.iter().map(|stem| fixture.interval(stem)).collect();
        let peak = peak_overlap(&intervals);
        assert_eq!(
            peak, expected_peak,
            "limit {limit}: measured peak concurrency {peak}, expected {expected_peak}"
        );
    }
}

#[test]
#[ignore = "requires the DOWAI_TEST_PG_* disposable PostgreSQL database (D-091)"]
fn same_variable_written_by_two_nodes_of_a_wave_fails_without_publishing() {
    let mut fixture = Fixture::new("conflict");
    fixture.write_model("left", "{{ def_var('shared', 1) }}select 1 as v");
    fixture.write_model("right", "{{ def_var('shared', 2) }}select 2 as v");
    fixture
        .set_config(json!({ "runtime": { "max_parallel_nodes": 4, "max_worker_processes": 8 } }));

    let error = fixture
        .run(&[
            fixture.target("left").as_str(),
            fixture.target("right").as_str(),
        ])
        .expect_err("two writers of one variable in a wave must conflict");
    assert_eq!(error.code(), "DOWAI_VARIABLE_CONFLICT");
}

#[test]
#[ignore = "requires the DOWAI_TEST_PG_* disposable PostgreSQL database (D-091)"]
fn failed_node_halts_the_pending_nodes_of_its_wave() {
    let mut fixture = Fixture::new("failure");
    fixture.write_model("a_broken", "select 1 / 0 as v");
    fixture.write_model("b_later", "select 2 as v");
    fixture.write_model("c_later", "select 3 as v");
    // One node at a time: the failure is reached before `b` and `c` start.
    fixture
        .set_config(json!({ "runtime": { "max_parallel_nodes": 1, "max_worker_processes": 8 } }));

    fixture
        .run(&[
            fixture.target("a_broken").as_str(),
            fixture.target("b_later").as_str(),
            fixture.target("c_later").as_str(),
        ])
        .expect_err("a failing SQL node must fail the run");
    assert!(
        !fixture.table_exists("b_later"),
        "pending node of the failed wave must not run"
    );
    assert!(
        !fixture.table_exists("c_later"),
        "pending node of the failed wave must not run"
    );
}

#[test]
#[ignore = "requires the DOWAI_TEST_PG_* disposable PostgreSQL database (D-091)"]
fn invocation_order_follows_the_plan_whatever_the_completion_order() {
    let mut fixture = Fixture::new("order");
    // Reversed durations: the plan's first node finishes last.
    let durations = [("a", 0.6), ("b", 0.4), ("c", 0.2), ("d", 0.0)];
    for (stem, sleep) in durations {
        fixture.write_model(stem, &timed_model(sleep));
    }
    fixture
        .set_config(json!({ "runtime": { "max_parallel_nodes": 4, "max_worker_processes": 8 } }));
    let targets: Vec<String> = durations
        .iter()
        .map(|(stem, _)| fixture.target(stem))
        .collect();
    let targets: Vec<&str> = targets.iter().map(String::as_str).collect();

    let order_of = |result: &RunResult| -> Vec<String> {
        result.nodes.iter().map(|node| node.id.clone()).collect()
    };
    let first = fixture.run(&targets).expect("first run");
    let second = fixture.run_with(&targets, true).expect("second run");
    assert_eq!(order_of(&first), order_of(&second));
    // Plan order is the alphabetical order of the models: `a` before `b`, `c` and `d`.
    let positions: Vec<usize> = durations
        .iter()
        .map(|(stem, _)| {
            first
                .nodes
                .iter()
                .position(|node| node.relation == fixture.relation(stem))
                .expect("node present in result")
        })
        .collect();
    assert!(
        positions.windows(2).all(|pair| pair[0] < pair[1]),
        "nodes must be reported in plan order: {positions:?}"
    );

    // The run journal records the nodes in plan order, in both runs.
    let journal_of = |run_id: &str| -> Vec<String> {
        RunStore::for_user_state(default_run_store_path().expect("state path"))
            .get(run_id)
            .expect("journal entry of the run")
            .invocations
            .iter()
            .map(|invocation| invocation.target.clone())
            .collect()
    };
    let first_journal = journal_of(&first.run_id);
    assert_eq!(
        first_journal,
        journal_of(&second.run_id),
        "journal order must repeat"
    );
    let stems_in_journal: Vec<usize> = durations
        .iter()
        .map(|(stem, _)| {
            first_journal
                .iter()
                .position(|target| target.contains(&fixture.relation(stem)))
                .expect("node in journal")
        })
        .collect();
    assert!(
        stems_in_journal.windows(2).all(|pair| pair[0] < pair[1]),
        "journal must follow plan order: {stems_in_journal:?}"
    );
}
