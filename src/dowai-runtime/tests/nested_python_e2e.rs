//! E2E tests for nested imperative calls (`env.run("other.py")`) executed by
//! the supervised Python runtime. Covers plano_mestre.md AC042-AC048.
//!
//! Every test uses a real project virtual environment and an isolated state
//! directory. No PostgreSQL is required.

use dowai_core::{
    admin_config_path, create_project, default_session_store_path, plan, user_config_path,
    validate_project_with_configuration, ExecutionMode, SessionStore,
};
use dowai_runtime::{run_postgres, RunRequest, RunResult};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Mutex, MutexGuard};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

static SERIAL: Mutex<()> = Mutex::new(());

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

struct Harness {
    _restores: Vec<EnvironmentRestore>,
    state_root: PathBuf,
    root: PathBuf,
    project: PathBuf,
    // Declared last so the lock is released only after cleanup guards drop.
    _guard: MutexGuard<'static, ()>,
}

impl Harness {
    fn new() -> Self {
        let guard = SERIAL
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock is after Unix epoch")
            .as_nanos();
        let base =
            std::env::temp_dir().join(format!("dowai-nested-{}-{nonce}", std::process::id()));
        let state_root = base.join("state");
        let root = base.join("workspace");
        for directory in ["local", "roaming", "system"] {
            fs::create_dir_all(state_root.join(directory)).expect("create isolated state");
        }
        fs::create_dir_all(&root).expect("create workspace");
        let restores = vec![
            EnvironmentRestore::set("LOCALAPPDATA", &state_root.join("local").into_os_string()),
            EnvironmentRestore::set("APPDATA", &state_root.join("roaming").into_os_string()),
            EnvironmentRestore::set("PROGRAMDATA", &state_root.join("system").into_os_string()),
            EnvironmentRestore::set(
                "XDG_CONFIG_HOME",
                &state_root.join("roaming").into_os_string(),
            ),
            EnvironmentRestore::set("XDG_STATE_HOME", &state_root.clone().into_os_string()),
        ];
        let project = create_project(&root, "fixture").expect("initialize project");
        let venv = project.join(".venv");
        let created = Command::new(if cfg!(windows) { "python" } else { "python3" })
            .args(["-m", "venv", "--without-pip"])
            .arg(&venv)
            .output()
            .expect("create project virtual environment");
        assert!(created.status.success(), "venv creation must succeed");
        let interpreter = if cfg!(windows) {
            venv.join("Scripts").join("python.exe")
        } else {
            venv.join("bin").join("python")
        };
        let harness = Self {
            _restores: restores,
            state_root,
            root,
            project,
            _guard: guard,
        };
        harness.edit_manifest(|manifest| {
            manifest["python"] = json!({ "interpreter": interpreter, "timeout_seconds": 30 });
            manifest["config"]["security"]["allow_external_writes"] = json!(true);
        });
        harness
    }

    fn edit_manifest(&self, edit: impl FnOnce(&mut Value)) {
        let path = self.project.join("dowai_project.json");
        let mut manifest: Value = serde_json::from_slice(&fs::read(&path).expect("read manifest"))
            .expect("manifest JSON");
        edit(&mut manifest);
        fs::write(path, serde_json::to_vec(&manifest).expect("serialize")).expect("write manifest");
    }

    fn model(&self, name: &str, body: &str) {
        fs::write(self.project.join("models").join(name), body).expect("write model");
    }

    fn run(&self, target: &str) -> dowai_core::Result<RunResult> {
        self.run_with(target, None, None)
    }

    fn run_with(
        &self,
        target: &str,
        mode: Option<ExecutionMode>,
        session_id: Option<String>,
    ) -> dowai_core::Result<RunResult> {
        run_postgres(
            &self.project,
            RunRequest {
                targets: vec![target.to_owned()],
                mode,
                profile: None,
                session_id,
                variables: BTreeMap::new(),
                confirm_destructive: false,
                full_refresh: false,
            },
        )
    }

    fn snapshot(&self, session_id: &str) -> BTreeMap<String, Value> {
        SessionStore::for_user_state(default_session_store_path().expect("state path"))
            .snapshot(session_id)
            .expect("load session variables")
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
        let _ = fs::remove_dir_all(&self.state_root);
    }
}

fn find<'a>(result: &'a RunResult, target: &str) -> Vec<&'a dowai_runtime::InvocationRecord> {
    result
        .invocations
        .iter()
        .filter(|invocation| invocation.target == target)
        .collect()
}

const CHILD_OK: &str = "def model(env):\n    env.setVar('from_child', 2)\n    env.setVar('saw_pending', env.getVar('pending'))\n";
const SENTINEL: &str = "sentinel-7f3a91c4-private-value";

#[test]
fn ac043_child_runs_inherits_pending_variables_merges_back_and_is_recorded_without_dag_edge() {
    let harness = Harness::new();
    harness.model("child.py", CHILD_OK);
    harness.model(
        "parent.py",
        "def model(env):\n    env.setVar('pending', 1)\n    env.setVar('secret', 'sentinel-7f3a91c4-private-value')\n    env.run('child.py')\n    assert env.getVar('from_child') == 2\n    env.setVar('parent_done', True)\n",
    );

    let result = harness.run("parent.py").expect("nested run succeeds");

    // No static edge: the plan contains only the root target.
    assert_eq!(
        result
            .nodes
            .iter()
            .map(|n| n.id.as_str())
            .collect::<Vec<_>>(),
        ["parent.py"]
    );
    let variables = harness.snapshot(&result.session_id);
    assert_eq!(variables["from_child"], json!(2));
    assert_eq!(variables["saw_pending"], json!(1));
    assert_eq!(variables["parent_done"], json!(true));

    // AC043 / history: parent static invocation and imperative child.
    let parent = find(&result, "parent.py");
    let child = find(&result, "child.py");
    assert_eq!(parent.len(), 1);
    assert_eq!(child.len(), 1);
    assert_eq!(parent[0].parent_invocation_id, result.invocation_id);
    assert_eq!(parent[0].call_type, "static");
    assert_eq!(child[0].call_type, "imperative");
    assert_eq!(child[0].parent_invocation_id, parent[0].invocation_id);
    assert_eq!(child[0].status, "SUCCEEDED");
    assert_ne!(child[0].invocation_id, parent[0].invocation_id);
    assert!(uuid_like(&child[0].invocation_id));

    // Invocation history must never expose variable values.
    let serialized = serde_json::to_string(&result.invocations).expect("serialize history");
    assert_eq!(variables["secret"], json!(SENTINEL));
    assert!(!serialized.contains(SENTINEL));
    assert!(!serialized.contains("saw_pending"));
    assert!(serialized.contains("parent_invocation_id"));
    assert!(serialized.contains("duration_ms"));
}

fn uuid_like(value: &str) -> bool {
    value.len() == 36 && value.matches('-').count() == 4
}

#[test]
fn ac045_uncaught_child_failure_stops_parent_and_publishes_nothing() {
    let harness = Harness::new();
    let marker = harness.root.join("after-child-marker");
    harness.model(
        "bad.py",
        "def model(env):\n    env.setVar('bad_var', 1)\n    raise RuntimeError('private child detail')\n",
    );
    harness.model(
        "parent.py",
        &format!(
            "import pathlib\ndef model(env):\n    env.setVar('before', 1)\n    env.run('bad.py')\n    pathlib.Path({}).write_text('ran')\n",
            serde_json::to_string(&marker.to_string_lossy().to_string()).expect("encode path")
        ),
    );

    let error = harness
        .run("parent.py")
        .expect_err("uncaught child failure must fail the run");

    assert!(error.code().starts_with("DOWAI_"));
    assert_eq!(error.code(), "DOWAI_PYTHON_FAILED");
    assert!(!error.message().contains("private child detail"));
    assert!(
        !marker.exists(),
        "statements after the failing call must not run"
    );
}

#[test]
fn ac045_failed_root_does_not_modify_an_existing_session() {
    let harness = Harness::new();
    harness.model("seed.py", "def model(env):\n    env.setVar('seed', 1)\n");
    let seeded = harness.run("seed.py").expect("seed session");
    harness.model(
        "bad.py",
        "def model(env):\n    env.setVar('bad_var', 1)\n    raise RuntimeError()\n",
    );
    harness.model(
        "parent.py",
        "def model(env):\n    env.setVar('parent_var', 1)\n    env.run('bad.py')\n",
    );

    harness
        .run_with("parent.py", None, Some(seeded.session_id.clone()))
        .expect_err("child failure fails the run");

    let variables = harness.snapshot(&seeded.session_id);
    assert_eq!(
        variables.keys().map(String::as_str).collect::<Vec<_>>(),
        ["seed"]
    );
}

#[test]
fn ac046_caught_child_failure_keeps_child_failed_discards_its_variables_and_parent_continues() {
    let harness = Harness::new();
    harness.model(
        "bad.py",
        "def model(env):\n    env.setVar('bad_var', 1)\n    env.setVar('pending', 'overwritten')\n    raise RuntimeError('private child detail')\n",
    );
    harness.model(
        "parent.py",
        "def model(env):\n    env.setVar('pending', 'original')\n    try:\n        env.run('bad.py')\n    except env.DowaiExecutionError as error:\n        env.setVar('caught_code', error.code)\n        env.setVar('caught_message', str(error))\n    env.setVar('parent_done', True)\n",
    );

    let result = harness
        .run("parent.py")
        .expect("parent handles the failure");

    let variables = harness.snapshot(&result.session_id);
    assert_eq!(variables["caught_code"], json!("DOWAI_PYTHON_FAILED"));
    assert!(!variables["caught_message"]
        .as_str()
        .expect("message is text")
        .contains("private child detail"));
    assert_eq!(variables["parent_done"], json!(true));
    assert_eq!(variables["pending"], json!("original"));
    assert!(!variables.contains_key("bad_var"));
    let bad = find(&result, "bad.py");
    assert_eq!(bad.len(), 1);
    assert_eq!(bad[0].status, "FAILED");
    assert_eq!(bad[0].call_type, "imperative");
    assert_eq!(find(&result, "parent.py")[0].status, "SUCCEEDED");
}

#[test]
fn ac046_sdk_import_path_of_the_error_class_is_available_to_models() {
    let harness = Harness::new();
    harness.model("bad.py", "def model(env):\n    raise RuntimeError()\n");
    harness.model(
        "parent.py",
        "from dowai import DowaiExecutionError\ndef model(env):\n    try:\n        env.run('bad.py')\n    except DowaiExecutionError as error:\n        assert error.code.startswith('DOWAI_')\n        env.setVar('ok', True)\n",
    );

    let result = harness
        .run("parent.py")
        .expect("imported class matches raised class");

    assert_eq!(harness.snapshot(&result.session_id)["ok"], json!(true));
}

#[test]
fn ac047_repeated_manual_calls_are_distinct_invocations_and_mode_is_restored() {
    let harness = Harness::new();
    harness.model(
        "child.py",
        "def model(env):\n    env.setVar('count', env.getVar('count', 0) + 1)\n",
    );
    harness.model(
        "parent.py",
        "def model(env):\n    assert env.mode == 'automatic'\n    env.run('child.py', mode='manual')\n    env.run('child.py', mode='manual')\n    with env.manual():\n        assert env.mode == 'manual'\n        env.run('child.py')\n    assert env.mode == 'automatic'\n    try:\n        with env.manual():\n            raise ValueError('boom')\n    except ValueError:\n        pass\n    assert env.mode == 'automatic'\n    env.setVar('mode_ok', True)\n",
    );

    let result = harness.run("parent.py").expect("repeated manual calls");

    assert_eq!(harness.snapshot(&result.session_id)["count"], json!(3));
    let children = find(&result, "child.py");
    assert_eq!(children.len(), 3);
    let ids: std::collections::BTreeSet<_> =
        children.iter().map(|c| c.invocation_id.as_str()).collect();
    assert_eq!(ids.len(), 3, "each call is a distinct invocation");
    assert!(children.iter().all(|c| c.status == "SUCCEEDED"));
}

#[test]
fn ac047_manual_scope_is_inherited_by_nested_calls_without_an_explicit_mode() {
    let harness = Harness::new();
    harness.model(
        "dep.py",
        "def model(env):\n    env.setVar('dep_runs', env.getVar('dep_runs', 0) + 1)\n",
    );
    harness.model(
        "child.py",
        "def model(env):\n    env.depends_on('dep')\n    env.setVar('child_runs', env.getVar('child_runs', 0) + 1)\n",
    );
    harness.model(
        "parent.py",
        "def model(env):\n    with env.manual():\n        env.run('child.py')\n    env.setVar('dep_inside_manual', env.getVar('dep_runs', 0))\n    env.run('child.py')\n    env.setVar('dep_after_scope', env.getVar('dep_runs', 0))\n",
    );

    let result = harness.run("parent.py").expect("inherited manual mode");

    let variables = harness.snapshot(&result.session_id);
    assert_eq!(
        variables["dep_inside_manual"],
        json!(0),
        "manual scope must not run dependencies"
    );
    assert_eq!(variables["dep_after_scope"], json!(1));
    let children = find(&result, "child.py");
    assert_eq!(children.len(), 2);
    assert_eq!(children[0].mode, "manual");
    assert_eq!(children[1].mode, "automatic");
    assert_eq!(find(&result, "dep.py").len(), 1);
}

#[test]
fn ac044_automatic_child_runs_static_predecessors_and_manual_child_does_not() {
    let harness = Harness::new();
    harness.model(
        "dep.py",
        "def model(env):\n    env.setVar('dep_runs', env.getVar('dep_runs', 0) + 1)\n",
    );
    harness.model(
        "child.py",
        "def model(env):\n    env.depends_on('dep')\n    env.setVar('child_runs', env.getVar('child_runs', 0) + 1)\n",
    );
    harness.model(
        "parent.py",
        "def model(env):\n    env.run('child.py')\n    env.setVar('dep_after_auto', env.getVar('dep_runs', 0))\n    env.run('child.py', mode='manual')\n    env.setVar('dep_after_manual', env.getVar('dep_runs', 0))\n",
    );

    let result = harness.run("parent.py").expect("mixed-mode nested calls");

    let variables = harness.snapshot(&result.session_id);
    assert_eq!(variables["dep_after_auto"], json!(1));
    assert_eq!(
        variables["dep_after_manual"],
        json!(1),
        "manual must not rerun dependencies"
    );
    assert_eq!(variables["child_runs"], json!(2));
    // The dependency of the automatic call is part of the history, nested under the call.
    let deps = find(&result, "dep.py");
    assert_eq!(deps.len(), 1);
    let first_child = find(&result, "child.py")[0];
    assert_eq!(deps[0].parent_invocation_id, first_child.invocation_id);
    assert_ne!(deps[0].call_type, "imperative");
    // Static plan stays untouched.
    assert_eq!(result.nodes.len(), 1);
}

#[test]
fn ac048_imperative_cycle_fails_with_a_readable_chain_before_looping() {
    let harness = Harness::new();
    harness.model("a.py", "def model(env):\n    env.run('b.py')\n");
    harness.model("b.py", "def model(env):\n    env.run('a.py')\n");
    let started = Instant::now();

    let error = harness.run("a.py").expect_err("cycles must be rejected");

    assert!(started.elapsed().as_secs() < 20, "must not loop");
    assert!(error.code().starts_with("DOWAI_"));
    assert_eq!(error.code(), "DOWAI_PLAN_CYCLE");
    assert!(
        error.message().contains("a.py -> b.py -> a.py"),
        "unexpected message: {}",
        error.message()
    );
}

#[test]
fn ac048_self_recursion_is_a_cycle() {
    let harness = Harness::new();
    harness.model(
        "loop.py",
        "def model(env):\n    env.run('loop.py', mode='manual')\n",
    );

    let error = harness.run("loop.py").expect_err("self recursion");

    assert_eq!(error.code(), "DOWAI_PLAN_CYCLE");
    assert!(error.message().contains("loop.py -> loop.py"));
}

#[test]
fn ac048_call_depth_above_the_configured_limit_fails_and_equal_depth_passes() {
    let harness = Harness::new();
    harness.model("c0.py", "def model(env):\n    env.run('c1.py')\n");
    harness.model("c1.py", "def model(env):\n    env.run('c2.py')\n");
    harness.model("c2.py", "def model(env):\n    env.run('c3.py')\n");
    harness.model(
        "c3.py",
        "def model(env):\n    env.setVar('reached', True)\n",
    );

    harness.edit_manifest(|m| m["config"]["execution"]["max_call_depth"] = json!(3));
    let ok = harness
        .run("c0.py")
        .expect("depth equal to the limit is allowed");
    assert_eq!(harness.snapshot(&ok.session_id)["reached"], json!(true));

    harness.edit_manifest(|m| m["config"]["execution"]["max_call_depth"] = json!(2));
    let error = harness
        .run("c0.py")
        .expect_err("depth above the limit must fail");
    assert!(error.code().starts_with("DOWAI_"));
    assert!(
        error.message().contains("call depth"),
        "unexpected message: {}",
        error.message()
    );
}

#[test]
fn ac048_invocation_count_limit_is_enforced_with_an_exact_count() {
    let harness = Harness::new();
    harness.model("child.py", "def model(env):\n    pass\n");
    harness.model(
        "parent.py",
        "def model(env):\n    ok = 0\n    codes = []\n    for _ in range(10):\n        try:\n            env.run('child.py', mode='manual')\n            ok += 1\n        except env.DowaiExecutionError as error:\n            codes.append(error.code)\n    env.setVar('ok_calls', ok)\n    env.setVar('codes', codes)\n",
    );
    harness.edit_manifest(|m| m["config"]["execution"]["max_invocations_per_run"] = json!(4));

    let result = harness.run("parent.py").expect("parent catches the limit");

    let variables = harness.snapshot(&result.session_id);
    // Budget of 4: the root parent plus exactly three nested calls.
    assert_eq!(variables["ok_calls"], json!(3));
    assert_eq!(variables["codes"], json!(vec!["DOWAI_EXECUTION_FAILED"; 7]));
    assert_eq!(result.invocations.len(), 4);
    assert_eq!(find(&result, "child.py").len(), 3);
}

#[test]
fn ac048_invocation_count_limit_uncaught_reports_the_limit_name() {
    let harness = Harness::new();
    harness.model("child.py", "def model(env):\n    pass\n");
    harness.model(
        "parent.py",
        "def model(env):\n    for _ in range(10):\n        env.run('child.py', mode='manual')\n",
    );
    harness.edit_manifest(|m| m["config"]["execution"]["max_invocations_per_run"] = json!(4));

    let error = harness.run("parent.py").expect_err("limit reached");

    assert_eq!(error.code(), "DOWAI_EXECUTION_FAILED");
    assert!(error.message().contains("max_invocations_per_run"));
}

#[test]
fn nested_sql_call_fails_explicitly_without_effect_and_can_be_caught() {
    let harness = Harness::new();
    harness.model("x.sql", "select 1 as id");
    harness.model(
        "caught.py",
        "def model(env):\n    try:\n        env.run('x.sql')\n    except env.DowaiExecutionError as error:\n        env.setVar('code', error.code)\n",
    );
    harness.model(
        "uncaught.py",
        "def model(env):\n    env.setVar('x', 1)\n    env.run('x.sql')\n",
    );

    let caught = harness.run("caught.py").expect("parent may catch");
    let variables = harness.snapshot(&caught.session_id);
    assert_eq!(variables["code"], json!("DOWAI_NOT_IMPLEMENTED"));
    let sql = find(&caught, "x.sql");
    assert_eq!(sql.len(), 1);
    assert_eq!(sql[0].status, "FAILED");

    let error = harness
        .run("uncaught.py")
        .expect_err("SQL in env.run is unsupported");
    assert_eq!(error.code(), "DOWAI_NOT_IMPLEMENTED");
}

#[test]
fn nested_python_with_sql_predecessor_fails_before_running_the_child() {
    let harness = Harness::new();
    let marker = harness.root.join("child-ran");
    harness.model("warehouse.sql", "select 1");
    harness.model(
        "child.py",
        &format!(
            "import pathlib\ndef model(env):\n    env.depends_on('warehouse')\n    pathlib.Path({}).write_text('x')\n",
            serde_json::to_string(&marker.to_string_lossy().to_string()).expect("encode")
        ),
    );
    harness.model("parent.py", "def model(env):\n    env.run('child.py')\n");

    let error = harness
        .run("parent.py")
        .expect_err("mixed SQL is unsupported");

    assert_eq!(error.code(), "DOWAI_NOT_IMPLEMENTED");
    assert!(!marker.exists());
}

#[test]
fn nested_targets_outside_the_models_directory_are_rejected() {
    let harness = Harness::new();
    let outside = harness.project.join("outside.py");
    let marker = harness.root.join("outside-ran");
    fs::write(
        &outside,
        format!(
            "import pathlib\ndef model(env):\n    pathlib.Path({}).write_text('x')\n",
            serde_json::to_string(&marker.to_string_lossy().to_string()).expect("encode")
        ),
    )
    .expect("write outside model");
    harness.model(
        "parent.py",
        "def model(env):\n    for target in ['../outside.py', '/etc/passwd', 'all', 'missing.py']:\n        try:\n            env.run(target)\n        except env.DowaiExecutionError as error:\n            assert error.code.startswith('DOWAI_'), error.code\n        else:\n            raise AssertionError('target must be rejected: ' + target)\n    env.setVar('rejected', True)\n",
    );

    let result = harness.run("parent.py").expect("all escapes are rejected");

    assert_eq!(
        harness.snapshot(&result.session_id)["rejected"],
        json!(true)
    );
    assert!(!marker.exists());
}

#[test]
fn nested_child_deadline_is_capped_by_the_remaining_parent_deadline() {
    let harness = Harness::new();
    harness.edit_manifest(|m| m["python"]["timeout_seconds"] = json!(2));
    // The child alone would finish (1.5 s < 2 s timeout) but only after the
    // parent's deadline; the parent supervisor is blocked while the child runs,
    // so without the cap the whole run would last about 3 s.
    harness.model(
        "slow.py",
        "import time\ndef model(env):\n    time.sleep(1.5)\n",
    );
    harness.model(
        "parent.py",
        "import time\ndef model(env):\n    time.sleep(1.5)\n    try:\n        env.run('slow.py')\n    except Exception:\n        pass\n    env.setVar('after_timeout', True)\n",
    );
    harness.model("seed.py", "def model(env):\n    env.setVar('seed', 1)\n");
    let seeded = harness.run("seed.py").expect("seed session");
    let started = Instant::now();

    let error = harness
        .run_with("parent.py", None, Some(seeded.session_id.clone()))
        .expect_err("timeout applies to children");

    let elapsed = started.elapsed().as_secs_f64();
    assert_eq!(error.code(), "DOWAI_PYTHON_TIMEOUT");
    assert!(elapsed >= 1.9, "the child must have started: {elapsed}");
    assert!(
        elapsed < 2.8,
        "child must be capped by the parent deadline: {elapsed}"
    );
    assert!(!harness
        .snapshot(&seeded.session_id)
        .contains_key("after_timeout"));
}

#[test]
fn invalid_env_run_arguments_are_rejected_inside_the_model() {
    let harness = Harness::new();
    harness.model(
        "parent.py",
        "def model(env):\n    for args, kwargs in [((), {}), ((1,), {}), (('',), {}), (('child.py',), {'mode': 'turbo'})]:\n        try:\n            env.run(*args, **kwargs)\n        except (ValueError, TypeError):\n            pass\n        else:\n            raise AssertionError('invalid arguments accepted')\n    env.setVar('validated', True)\n",
    );
    harness.model("child.py", "def model(env):\n    pass\n");

    let result = harness.run("parent.py").expect("argument validation");

    assert_eq!(
        harness.snapshot(&result.session_id)["validated"],
        json!(true)
    );
}

#[test]
fn nested_error_messages_seen_by_the_model_do_not_leak_paths() {
    let harness = Harness::new();
    harness.model(
        "parent.py",
        "def model(env):\n    messages = []\n    for target in ['../../etc/passwd', 'nonexistent_target.py']:\n        try:\n            env.run(target)\n        except env.DowaiExecutionError as error:\n            messages.append([error.code, str(error)])\n    env.setVar('messages', messages)\n",
    );

    let result = harness.run("parent.py").expect("parent catches both");

    let messages = harness.snapshot(&result.session_id)["messages"].clone();
    let messages = messages.as_array().expect("list");
    assert_eq!(messages.len(), 2);
    let texts: Vec<&str> = messages
        .iter()
        .map(|entry| entry[1].as_str().expect("text"))
        .collect();
    assert_eq!(texts[0], texts[1], "no existence oracle: {texts:?}");
    for text in texts {
        assert!(!text.contains("passwd"), "{text}");
        assert!(!text.contains("nonexistent_target"), "{text}");
        assert!(!text.contains(&harness.root.to_string_lossy().to_string()));
        assert!(!text.contains('/'), "{text}");
    }
}

#[test]
fn nested_rejected_targets_are_recorded_as_invalid_without_model_text() {
    let harness = Harness::new();
    harness.model("child.py", "def model(env):\n    pass\n");
    harness.model(
        "parent.py",
        "def model(env):\n    for target in ['../../etc/passwd', 'x\\u0007y.py', './child.py']:\n        try:\n            env.run(target)\n        except env.DowaiExecutionError:\n            pass\n",
    );

    let result = harness.run("parent.py").expect("parent catches");

    let imperative: Vec<_> = result
        .invocations
        .iter()
        .filter(|entry| entry.call_type == "imperative")
        .collect();
    assert_eq!(imperative.len(), 3);
    assert_eq!(imperative[0].target, "<invalid>");
    assert_eq!(imperative[1].target, "<invalid>");
    assert_eq!(imperative[2].target, "child.py", "canonical model id");
    assert_eq!(imperative[2].status, "SUCCEEDED");
    let serialized = serde_json::to_string(&result.invocations).expect("serialize");
    assert!(!serialized.contains("passwd"));
}

#[test]
fn nested_error_code_is_propagated_only_for_the_call_that_failed() {
    let harness = Harness::new();
    harness.model("x.sql", "select 1 as id");
    harness.model(
        "parent.py",
        "def model(env):\n    errors = []\n    for target in ['x.sql', 'missing_model.py']:\n        try:\n            env.run(target)\n        except env.DowaiExecutionError as error:\n            errors.append(error)\n    raise errors[0]\n",
    );

    let error = harness
        .run("parent.py")
        .expect_err("re-raised error fails the run");

    // The first captured error (SQL unsupported) is re-raised, not the last one.
    assert_eq!(error.code(), "DOWAI_NOT_IMPLEMENTED");
}

#[test]
fn model_fabricated_error_codes_are_not_trusted() {
    let harness = Harness::new();
    harness.model("x.sql", "select 1 as id");
    harness.model(
        "parent.py",
        "def model(env):\n    try:\n        env.run('x.sql')\n    except env.DowaiExecutionError:\n        pass\n    raise env.DowaiExecutionError('DOWAI_NOT_IMPLEMENTED', 'forged')\n",
    );

    let error = harness.run("parent.py").expect_err("forged code");

    assert_eq!(error.code(), "DOWAI_PYTHON_FAILED");
}

#[test]
fn nested_call_with_invalid_caller_variables_is_rejected_before_the_child_runs() {
    let harness = Harness::new();
    let marker = harness.root.join("child-ran-oversized");
    harness.model(
        "child.py",
        &format!(
            "import pathlib\ndef model(env):\n    pathlib.Path({}).write_text('x')\n",
            serde_json::to_string(&marker.to_string_lossy().to_string()).expect("encode")
        ),
    );
    harness.model(
        "parent.py",
        "def model(env):\n    env.setVar('big', 'x' * 70000)\n    env.run('child.py')\n",
    );

    let error = harness
        .run("parent.py")
        .expect_err("oversized overlay is rejected");

    assert_eq!(error.code(), "DOWAI_SESSION_INVALID");
    assert!(!marker.exists(), "child must not run");
}

#[test]
fn failing_predecessor_marks_the_call_and_the_predecessor_failed_and_skips_the_child() {
    let harness = Harness::new();
    let marker = harness.root.join("child-ran-after-dep");
    harness.model(
        "dep.py",
        "def model(env):\n    raise RuntimeError('dep detail')\n",
    );
    harness.model(
        "child.py",
        &format!(
            "import pathlib\ndef model(env):\n    env.depends_on('dep')\n    pathlib.Path({}).write_text('x')\n",
            serde_json::to_string(&marker.to_string_lossy().to_string()).expect("encode")
        ),
    );
    harness.model(
        "parent.py",
        "def model(env):\n    try:\n        env.run('child.py')\n    except env.DowaiExecutionError as error:\n        env.setVar('code', error.code)\n",
    );

    let result = harness.run("parent.py").expect("parent catches");

    assert_eq!(
        harness.snapshot(&result.session_id)["code"],
        json!("DOWAI_PYTHON_FAILED")
    );
    assert!(!marker.exists());
    let call = find(&result, "child.py");
    let dependency = find(&result, "dep.py");
    assert_eq!((call.len(), dependency.len()), (1, 1));
    assert_eq!(call[0].status, "FAILED");
    assert_eq!(call[0].error_code.as_deref(), Some("DOWAI_PYTHON_FAILED"));
    assert_eq!(dependency[0].status, "FAILED");
    assert_eq!(dependency[0].call_type, "dependency");
    assert_eq!(dependency[0].parent_invocation_id, call[0].invocation_id);
    assert!(result
        .invocations
        .iter()
        .all(|entry| entry.status != "SKIPPED"));
}

#[test]
fn a_worker_flooding_stdout_fails_cleanly_without_hanging_the_host() {
    let harness = Harness::new();
    harness.model(
        "parent.py",
        "import sys\ndef model(env):\n    for _ in range(2000):\n        sys.__stdout__.write('{\"junk\":\"' + 'x' * 4096 + '\"}\\n')\n    sys.__stdout__.flush()\n",
    );
    let started = Instant::now();

    let error = harness
        .run("parent.py")
        .expect_err("flood is a protocol error");

    assert_eq!(error.code(), "DOWAI_PYTHON_PROTOCOL");
    assert!(started.elapsed().as_secs() < 20);
}

// ENT-040: `env.run` returns the §6.3 fields with the run_id of the parent run.
#[test]
fn ac040_env_run_returns_the_contract_fields_with_the_parent_run_id() {
    let harness = Harness::new();
    harness.model("child.py", CHILD_OK);
    harness.model(
        "parent.py",
        "def model(env):\n    r = env.run('child.py')\n    env.setVar('child_run_id', r.run_id)\n    env.setVar('child_invocation_id', r.invocation_id)\n    env.setVar('child_status', r.status)\n    env.setVar('child_failed', r.failed)\n    env.setVar('child_mode', r.execution_mode)\n",
    );

    let result = harness.run("parent.py").expect("nested run succeeds");

    let variables = harness.snapshot(&result.session_id);
    assert_eq!(variables["child_run_id"], json!(result.run_id));
    assert_eq!(variables["child_status"], json!("SUCCEEDED"));
    assert_eq!(variables["child_failed"], json!(false));
    assert_eq!(variables["child_mode"], json!("automatic"));
    let child = find(&result, "child.py");
    assert_eq!(
        variables["child_invocation_id"],
        json!(child[0].invocation_id)
    );
}

// ENT-040: `env.plan` and `env.validate` return the same payloads as the engine functions
// that back `dowai plan --json` and `dowai validate --json`.
#[test]
fn ac040_env_plan_and_validate_return_the_engine_payloads() {
    let harness = Harness::new();
    harness.model("child.py", CHILD_OK);
    harness.model(
        "parent.py",
        "def model(env):\n    env.setVar('plan', env.plan('child.py'))\n    env.setVar('validation', env.validate())\n",
    );

    let result = harness.run("parent.py").expect("nested run succeeds");

    let variables = harness.snapshot(&result.session_id);
    let root = harness.project.canonicalize().expect("canonical project");
    let expected_plan =
        plan(&root, &["child.py".to_owned()], ExecutionMode::Automatic).expect("engine plan");
    assert_eq!(
        variables["plan"],
        serde_json::to_value(expected_plan).expect("plan json")
    );
    let expected_validation = validate_project_with_configuration(
        &root,
        user_config_path().as_deref(),
        admin_config_path().as_deref(),
    );
    assert_eq!(
        variables["validation"],
        serde_json::to_value(expected_validation).expect("validation json")
    );
    // Inspection commands record no invocation: only the parent is in the history.
    assert_eq!(result.invocations.len(), 1);
}

// ENT-040: a rejected engine command is a catchable error with a stable code only.
#[test]
fn ac040_env_plan_of_missing_target_fails_catchably_with_a_stable_code() {
    let harness = Harness::new();
    harness.model(
        "parent.py",
        "def model(env):\n    try:\n        env.plan('missing.py')\n    except Exception as error:\n        env.setVar('plan_error', error.code)\n",
    );

    let result = harness
        .run("parent.py")
        .expect("parent catches the rejection");

    let variables = harness.snapshot(&result.session_id);
    let code = variables["plan_error"].as_str().expect("code is a string");
    assert!(code.starts_with("DOWAI_"), "stable code: {code}");
    let root = harness.project.canonicalize().expect("canonical project");
    let root_text = root.display().to_string();
    assert!(!code.contains(&root_text));
}
