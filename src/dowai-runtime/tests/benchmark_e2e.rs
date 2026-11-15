//! ENT-076: `benchmark` runs a workload repeatedly and reports the host, the runs and the peak
//! memory of the worker processes against `runtime.max_memory_mb`.
//!
//! Needs a local `python3` able to create a virtual environment.

use dowai_core::{create_project, DowaiError};
use dowai_runtime::benchmark::{benchmark, BENCHMARK_SCHEMA_VERSION, MAX_REPEAT};
use serde_json::{json, Value};
use std::ffi::OsString;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

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
            .expect("clock")
            .as_nanos();
        let id = COUNTER.fetch_add(1, Ordering::SeqCst);
        let root = std::env::temp_dir().join(format!(
            "dowai-benchmark-{}-{nonce}-{id}",
            std::process::id()
        ));
        let state = root.join("state");
        for dir in ["local", "roaming", "system"] {
            fs::create_dir_all(state.join(dir)).expect("state dir");
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
            .expect("create venv");
        assert!(created.status.success(), "venv creation failed");
        let manifest_path = project.join("dowai_project.json");
        let mut manifest: Value =
            serde_json::from_slice(&fs::read(&manifest_path).expect("read manifest"))
                .expect("JSON");
        manifest["python"] =
            json!({ "interpreter": venv.join("bin").join("python"), "timeout_seconds": 60 });
        manifest["config"]["runtime"]["max_memory_mb"] = json!(128);
        manifest["config"]["security"]["allow_external_writes"] = json!(true);
        fs::write(
            &manifest_path,
            serde_json::to_vec(&manifest).expect("serialize"),
        )
        .expect("write");
        fs::write(
            project.join("models").join("stream.py"),
            "def model(env):\n    env.setVar('done', True)\n    return None\n",
        )
        .expect("write model");
        Self {
            root,
            project,
            _env: env,
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn repeat_outside_the_allowed_range_is_refused_before_any_run() {
    let fixture = Fixture::new();
    for repeat in [0, MAX_REPEAT + 1] {
        let error = benchmark(&fixture.project, vec!["stream.py".to_owned()], repeat)
            .expect_err("repeat outside 1..=20 must be refused");
        assert!(matches!(error, DowaiError::Config(_)), "got {error:?}");
    }
}

#[test]
fn report_has_every_repetition_the_host_and_the_memory_comparison() {
    let fixture = Fixture::new();
    let report = benchmark(&fixture.project, vec!["stream.py".to_owned()], 2)
        .expect("benchmark of a Python model must run");

    assert_eq!(report.schema_version, BENCHMARK_SCHEMA_VERSION);
    assert_eq!(report.repeat, 2);
    assert_eq!(report.workload, vec!["stream.py".to_owned()]);
    assert_eq!(report.runs.len(), 2);
    assert!(report
        .runs
        .iter()
        .all(|run| run.status == "SUCCEEDED" && run.nodes == 1));
    assert!(report.duration_ms.min <= report.duration_ms.median);
    assert!(report.duration_ms.median <= report.duration_ms.max);
    assert_eq!(report.memory_limit_mb, Some(128));
    assert!(!report.host.os.is_empty());
    assert!(
        !report.notes.is_empty(),
        "limitations are reported with the numbers"
    );
    if cfg!(target_os = "linux") {
        let peak = report
            .peak_rss_kib
            .expect("worker RSS is measured on Linux");
        assert!(peak > 0);
        assert_eq!(report.within_memory_limit, Some(peak <= 128 * 1024));
        assert!(report.host.mem_total_kib.is_some());
    }
}
