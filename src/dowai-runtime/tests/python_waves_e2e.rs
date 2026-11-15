//! ENT-120c: automatic Python-only runs execute the nodes of a wave concurrently (ADR 0008).
//!
//! Needs a local `python3` able to create a virtual environment. Overlap is measured inside the
//! models: each one appends its start and end instants to a shared timeline file.

use dowai_core::{create_project, default_run_store_path, DowaiError, RunStore};
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
    _env: Vec<EnvironmentRestore>,
}

impl Fixture {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock is after Unix epoch")
            .as_nanos();
        let id = COUNTER.fetch_add(1, Ordering::SeqCst);
        let root = std::env::temp_dir().join(format!(
            "dowai-python-waves-{}-{nonce}-{id}",
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
                .expect("manifest is JSON");
        manifest["config"]["security"]["allow_external_writes"] = json!(true);
        manifest["python"] = json!({
            "interpreter": venv.join("bin").join("python"),
            "timeout_seconds": 60
        });
        fs::write(
            &manifest_path,
            serde_json::to_vec(&manifest).expect("serialize manifest"),
        )
        .expect("write manifest");
        Self {
            root,
            project,
            _env: env,
        }
    }

    fn write_model(&self, name: &str, body: &str) {
        fs::write(self.project.join("models").join(name), body).expect("write Python model");
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

    fn run_with(&self, targets: &[&str], confirm: bool) -> Result<RunResult, DowaiError> {
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
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn literal(path: &Path) -> String {
    serde_json::to_string(&path.display().to_string()).expect("path serializes as a string")
}

/// Model body that appends its own `(start, end)` instants to the timeline file.
fn timed_model(timeline: &Path, name: &str, sleep: f64) -> String {
    format!(
        "import time\nLOG = {log}\ndef model(env):\n    start = time.time()\n    time.sleep({sleep})\n    end = time.time()\n    with open(LOG, 'a') as handle:\n        handle.write('{name} %r %r\\n' % (start, end))\n    return None\n",
        log = literal(timeline),
    )
}

/// Largest number of `(start, end)` intervals overlapping at one instant.
fn peak_overlap(intervals: &[(f64, f64)]) -> usize {
    let mut events: Vec<(f64, i32)> = Vec::new();
    for (start, end) in intervals {
        events.push((*start, 1));
        events.push((*end, -1));
    }
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

fn read_intervals(timeline: &Path) -> Vec<(String, f64, f64)> {
    fs::read_to_string(timeline)
        .expect("models wrote their timeline")
        .lines()
        .map(|line| {
            let mut parts = line.split_whitespace();
            let name = parts.next().expect("name").to_owned();
            let start: f64 = parts.next().expect("start").parse().expect("numeric");
            let end: f64 = parts.next().expect("end").parse().expect("numeric");
            (name, start, end)
        })
        .collect()
}

#[test]
fn independent_python_nodes_overlap_up_to_the_limit() {
    for (limit, expected_peak) in [(4_u64, 4_usize), (2, 2)] {
        let fixture = Fixture::new();
        let timeline = fixture.root.join(format!("timeline-{limit}.txt"));
        let names = ["a", "b", "c", "d"];
        for name in names {
            fixture.write_model(
                &format!("{name}.py"),
                &timed_model(&timeline, name, SLEEP_SECONDS),
            );
        }
        fixture.set_config(json!({ "runtime": {
            "max_parallel_nodes": limit,
            "max_worker_processes": 8,
        }}));
        let targets: Vec<String> = names.iter().map(|name| format!("{name}.py")).collect();
        let targets: Vec<&str> = targets.iter().map(String::as_str).collect();

        let result = fixture
            .run(&targets)
            .expect("independent Python models must run");
        assert_eq!(result.status, "SUCCEEDED");
        assert!(
            !result
                .warnings
                .iter()
                .any(|warning| warning.contains("concorrência efetiva 1")),
            "concurrent waves must not report a sequential scheduler: {:?}",
            result.warnings
        );
        let intervals: Vec<(f64, f64)> = read_intervals(&timeline)
            .into_iter()
            .map(|(_, start, end)| (start, end))
            .collect();
        assert_eq!(intervals.len(), 4, "every model records one interval");
        let peak = peak_overlap(&intervals);
        assert_eq!(
            peak, expected_peak,
            "limit {limit}: measured peak concurrency {peak}, expected {expected_peak}"
        );
    }
}

#[test]
fn two_python_writers_of_one_variable_in_a_wave_conflict() {
    let fixture = Fixture::new();
    fixture.write_model(
        "left.py",
        "def model(env):\n    env.setVar('shared', 1)\n    return None\n",
    );
    fixture.write_model(
        "right.py",
        "def model(env):\n    env.setVar('shared', 2)\n    return None\n",
    );
    fixture
        .set_config(json!({ "runtime": { "max_parallel_nodes": 4, "max_worker_processes": 8 } }));

    let error = fixture
        .run(&["left.py", "right.py"])
        .expect_err("two writers of one variable in a wave must conflict");
    assert_eq!(error.code(), "DOWAI_VARIABLE_CONFLICT");
}

#[test]
fn failed_python_node_halts_the_pending_nodes_of_its_wave() {
    let fixture = Fixture::new();
    let marker_b = fixture.root.join("b-ran.txt");
    let marker_c = fixture.root.join("c-ran.txt");
    fixture.write_model(
        "a_broken.py",
        "def model(env):\n    raise RuntimeError('boom')\n",
    );
    fixture.write_model(
        "b_later.py",
        &format!("from pathlib import Path\ndef model(env):\n    Path({}).write_text('ran')\n    return None\n", literal(&marker_b)),
    );
    fixture.write_model(
        "c_later.py",
        &format!("from pathlib import Path\ndef model(env):\n    Path({}).write_text('ran')\n    return None\n", literal(&marker_c)),
    );
    // One node at a time: the failure is reached before `b` and `c` start.
    fixture
        .set_config(json!({ "runtime": { "max_parallel_nodes": 1, "max_worker_processes": 8 } }));

    fixture
        .run(&["a_broken.py", "b_later.py", "c_later.py"])
        .expect_err("a failing Python node must fail the run");
    assert!(
        !marker_b.exists(),
        "pending node of the failed wave must not run"
    );
    assert!(
        !marker_c.exists(),
        "pending node of the failed wave must not run"
    );
}

#[test]
fn plan_order_is_kept_in_result_and_journal_whatever_the_completion_order() {
    let fixture = Fixture::new();
    let timeline = fixture.root.join("order-timeline.txt");
    // Reversed durations: the plan's first node finishes last.
    let durations = [("a", 1.0), ("b", 0.6), ("c", 0.3), ("d", 0.05)];
    for (name, sleep) in durations {
        fixture.write_model(&format!("{name}.py"), &timed_model(&timeline, name, sleep));
    }
    fixture
        .set_config(json!({ "runtime": { "max_parallel_nodes": 4, "max_worker_processes": 8 } }));
    let targets: Vec<String> = durations
        .iter()
        .map(|(name, _)| format!("{name}.py"))
        .collect();
    let targets: Vec<&str> = targets.iter().map(String::as_str).collect();

    let first = fixture.run(&targets).expect("first run");
    // Second run only re-executes the same models; the timeline is rewritten by appending.
    let second = fixture.run_with(&targets, true).expect("second run");

    let ids = |result: &RunResult| -> Vec<String> {
        result.nodes.iter().map(|node| node.id.clone()).collect()
    };
    assert_eq!(ids(&first), ids(&second), "result order must repeat");
    let positions: Vec<usize> = durations
        .iter()
        .map(|(name, _)| {
            first
                .nodes
                .iter()
                .position(|node| node.id.ends_with(&format!("{name}.py")))
                .expect("node present in result")
        })
        .collect();
    assert!(
        positions.windows(2).all(|pair| pair[0] < pair[1]),
        "nodes must be reported in plan order: {positions:?}"
    );

    let journal = |run_id: &str| -> Vec<String> {
        RunStore::for_user_state(default_run_store_path().expect("state path"))
            .get(run_id)
            .expect("journal entry of the run")
            .invocations
            .iter()
            .map(|invocation| invocation.target.clone())
            .collect()
    };
    let journal_first = journal(&first.run_id);
    assert_eq!(
        journal_first,
        journal(&second.run_id),
        "journal order must repeat"
    );
    let journal_positions: Vec<usize> = durations
        .iter()
        .map(|(name, _)| {
            journal_first
                .iter()
                .position(|target| target.ends_with(&format!("{name}.py")))
                .expect("node in journal")
        })
        .collect();
    assert!(
        journal_positions.windows(2).all(|pair| pair[0] < pair[1]),
        "journal must follow plan order: {journal_positions:?}"
    );
}
