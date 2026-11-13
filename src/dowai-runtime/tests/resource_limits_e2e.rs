//! ENT-038: limites de recursos e de paralelismo, em runs Python-only (sem PostgreSQL).

use dowai_core::{create_project, DowaiError};
use dowai_runtime::{run_postgres, RunRequest, RunResult};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

struct TestWorkspace {
    root: PathBuf,
    project: PathBuf,
}

impl TestWorkspace {
    fn new(system_interpreter: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock is after Unix epoch")
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("dowai-resource-e2e-{}-{nonce}", std::process::id()));
        fs::create_dir_all(&root).expect("create isolated workspace");
        let project = create_project(&root, "fixture").expect("initialize project");
        let virtual_environment = project.join(".venv");
        let creation = Command::new(system_interpreter)
            .args(["-m", "venv", "--without-pip"])
            .arg(&virtual_environment)
            .output()
            .expect("create project virtual environment");
        assert!(
            creation.status.success(),
            "create project virtual environment"
        );
        let interpreter = virtual_environment.join("bin").join("python");
        let manifest = project.join("dowai_project.json");
        let mut config: Value =
            serde_json::from_slice(&fs::read(&manifest).expect("read project config"))
                .expect("generated project config is valid JSON");
        config["python"] = json!({ "interpreter": interpreter, "timeout_seconds": 30 });
        config["config"]["security"]["allow_external_writes"] = json!(true);
        fs::write(
            &manifest,
            serde_json::to_vec(&config).expect("serialize config"),
        )
        .expect("write project config");
        Self { root, project }
    }

    fn write_model(&self, name: &str, body: &str) {
        fs::write(self.project.join("models").join(name), body).expect("write Python model");
    }

    /// Merges `group -> key -> value` overrides into the project's `config` section.
    fn set_config(&self, overrides: Value) {
        let manifest = self.project.join("dowai_project.json");
        let mut config: Value =
            serde_json::from_slice(&fs::read(&manifest).expect("read project config"))
                .expect("project config is valid JSON");
        for (group, values) in overrides.as_object().expect("overrides are an object") {
            for (key, value) in values.as_object().expect("group overrides are an object") {
                config["config"][group.as_str()][key.as_str()] = value.clone();
            }
        }
        fs::write(
            &manifest,
            serde_json::to_vec(&config).expect("serialize config"),
        )
        .expect("write project config");
    }

    fn run(&self, targets: &[&str]) -> Result<RunResult, DowaiError> {
        run_postgres(
            &self.project,
            RunRequest {
                targets: targets.iter().map(|target| (*target).to_owned()).collect(),
                mode: None,
                profile: None,
                session_id: None,
                variables: BTreeMap::new(),
                confirm_destructive: false,
                full_refresh: false,
            },
        )
    }
}

impl Drop for TestWorkspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
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

/// Isolates session and user state for the duration of one test.
fn isolated_state() -> (PathBuf, Vec<EnvironmentRestore>) {
    let state_root = std::env::temp_dir().join(format!(
        "dowai-resource-state-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock is after Unix epoch")
            .as_nanos()
    ));
    let local = state_root.join("local");
    let roaming = state_root.join("roaming");
    let system = state_root.join("system");
    for directory in [&local, &roaming, &system] {
        fs::create_dir_all(directory).expect("create isolated state directory");
    }
    let guards = vec![
        EnvironmentRestore::set("LOCALAPPDATA", &local.into_os_string()),
        EnvironmentRestore::set("APPDATA", &roaming.clone().into_os_string()),
        EnvironmentRestore::set("PROGRAMDATA", &system.into_os_string()),
        EnvironmentRestore::set("XDG_CONFIG_HOME", &roaming.into_os_string()),
        EnvironmentRestore::set("XDG_STATE_HOME", &state_root.clone().into_os_string()),
    ];
    (state_root, guards)
}

fn python_executable() -> String {
    let output = Command::new("python3")
        .args(["-c", "import sys; print(sys.executable)"])
        .output()
        .expect("Python 3 must be installed for resource limit tests");
    assert!(output.status.success(), "query Python interpreter path");
    String::from_utf8(output.stdout)
        .expect("Python executable path is UTF-8")
        .trim()
        .to_owned()
}

#[cfg(target_os = "linux")]
#[test]
fn rlimit_as_stops_a_worker_that_exceeds_max_memory_mb() {
    let (state_root, _guards) = isolated_state();
    let workspace = TestWorkspace::new(&python_executable());
    workspace.set_config(json!({ "runtime": { "max_memory_mb": 256 } }));
    workspace.write_model(
        "hog.py",
        "def model(env):\n    data = bytearray(1024 * 1024 * 1024)\n    env.setVar('size', len(data))\n    return None\n",
    );
    workspace.write_model(
        "small.py",
        "def model(env):\n    data = bytearray(16 * 1024 * 1024)\n    env.setVar('size', len(data))\n    return None\n",
    );

    let error = workspace
        .run(&["hog.py"])
        .expect_err("a 1 GiB allocation must fail under a 256 MiB RLIMIT_AS");
    assert_eq!(error.code(), "DOWAI_PYTHON_FAILED");

    let result = workspace
        .run(&["small.py"])
        .expect("a 16 MiB allocation fits under the same limit");
    assert_eq!(result.status, "SUCCEEDED");
    assert!(
        result
            .warnings
            .iter()
            .any(|warning| warning.contains("RLIMIT_AS") && warning.contains("partial")),
        "the memory limit state must be reported: {:?}",
        result.warnings
    );
    let _ = fs::remove_dir_all(state_root);
}

#[test]
fn require_refuses_worker_before_user_code_when_cpu_limit_is_not_enforced() {
    let (state_root, _guards) = isolated_state();
    let workspace = TestWorkspace::new(&python_executable());
    workspace.set_config(json!({
        "runtime": { "max_cpu_percent": 50 },
        "python": { "resource_enforcement": "require" }
    }));
    let marker = workspace.root.join("user-code-ran.txt");
    let marker_literal = serde_json::to_string(&marker.display().to_string())
        .expect("marker path serializes as a Python string");
    workspace.write_model(
        "guarded.py",
        &format!(
            "from pathlib import Path\ndef model(env):\n    Path({marker_literal}).write_text('ran')\n    return None\n"
        ),
    );

    let error = workspace
        .run(&["guarded.py"])
        .expect_err("require must refuse an unenforced cpu limit");
    assert_eq!(error.code(), "DOWAI_RESOURCE_UNSUPPORTED");
    assert!(error.message().contains("runtime.max_cpu_percent=50"));
    assert!(
        !marker.exists(),
        "user code must not run when the limit cannot be enforced"
    );
    let _ = fs::remove_dir_all(state_root);
}

#[test]
fn independent_nodes_never_overlap_and_the_unused_parallel_limit_is_reported() {
    let (state_root, _guards) = isolated_state();
    let workspace = TestWorkspace::new(&python_executable());
    let log = workspace.root.join("timeline.txt");
    let log_literal = serde_json::to_string(&log.display().to_string())
        .expect("log path serializes as a Python string");
    for name in ["a", "b", "c"] {
        workspace.write_model(
            &format!("{name}.py"),
            &format!(
                "import time\nLOG = {log_literal}\ndef model(env):\n    with open(LOG, 'a') as handle:\n        handle.write('{name} start %r\\n' % time.time())\n    time.sleep(0.2)\n    with open(LOG, 'a') as handle:\n        handle.write('{name} end %r\\n' % time.time())\n    return None\n"
            ),
        );
    }

    let result = workspace
        .run(&["a.py", "b.py", "c.py"])
        .expect("three independent Python models must run");
    assert_eq!(result.status, "SUCCEEDED");
    assert!(
        result
            .warnings
            .iter()
            .any(|warning| warning.contains("concorrência efetiva 1")),
        "independent nodes with max_parallel_nodes=4 must report the sequential scheduler: {:?}",
        result.warnings
    );

    // Each model is one interval [start, end]. With at most one node at a time (the limit
    // applied by the scheduler), no two intervals may overlap.
    let text = fs::read_to_string(&log).expect("models wrote their timeline");
    let mut intervals: Vec<(f64, f64)> = Vec::new();
    let mut open: BTreeMap<String, f64> = BTreeMap::new();
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let name = parts.next().expect("timeline line has a name").to_owned();
        let kind = parts.next().expect("timeline line has a kind");
        let at: f64 = parts
            .next()
            .expect("timeline line has a timestamp")
            .parse()
            .expect("timestamp is numeric");
        match kind {
            "start" => {
                open.insert(name, at);
            }
            "end" => {
                let start = open.remove(&name).expect("end follows start");
                intervals.push((start, at));
            }
            other => panic!("unexpected timeline entry {other}"),
        }
    }
    assert_eq!(intervals.len(), 3, "every model must record one interval");
    intervals.sort_by(|left, right| left.0.partial_cmp(&right.0).expect("finite timestamps"));
    for pair in intervals.windows(2) {
        assert!(
            pair[1].0 >= pair[0].1,
            "nodes overlapped: {:?} then {:?}",
            pair[0],
            pair[1]
        );
    }
    let _ = fs::remove_dir_all(state_root);
}
