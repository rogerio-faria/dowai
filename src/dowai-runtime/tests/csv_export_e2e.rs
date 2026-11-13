//! ENT-050: `env.generateCsv(data, path, **options)` streams UTF-8 CSV to a destination the host
//! authorizes (inside the project, no `..`, no symbolic link, no reserved entry), writes through
//! a temporary sibling that is renamed only after the last row, and validates its options.

use dowai_core::{create_project, default_session_store_path, SessionStore};
use dowai_runtime::{run_postgres, RunRequest};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static WORKSPACE_SEQUENCE: AtomicUsize = AtomicUsize::new(0);

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
        let sequence = WORKSPACE_SEQUENCE.fetch_add(1, Ordering::SeqCst);
        let root = std::env::temp_dir().join(format!(
            "dowai-ent050-{}-{nonce}-{sequence}",
            std::process::id()
        ));
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
        let interpreter = if cfg!(windows) {
            virtual_environment.join("Scripts").join("python.exe")
        } else {
            virtual_environment.join("bin").join("python")
        };
        let manifest = project.join("dowai_project.json");
        let mut project_config: Value =
            serde_json::from_slice(&fs::read(&manifest).expect("read project config"))
                .expect("project config is valid JSON");
        project_config["python"] = json!({
            "interpreter": interpreter,
            "timeout_seconds": 60
        });
        project_config["config"]["security"]["allow_external_writes"] = json!(true);
        fs::write(
            manifest,
            serde_json::to_vec(&project_config).expect("serialize project config"),
        )
        .expect("write project config");
        Self { root, project }
    }

    fn write_model(&self, name: &str, body: &str) {
        fs::write(self.project.join("models").join(name), body).expect("write Python model");
    }
}

impl Drop for TestWorkspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn python_executable() -> String {
    let executable = if cfg!(windows) { "python" } else { "python3" };
    let output = Command::new(executable)
        .args(["-c", "import sys; print(sys.executable)"])
        .output()
        .expect("Python 3 must be installed for Python runtime integration tests");
    assert!(output.status.success(), "query Python interpreter path");
    String::from_utf8(output.stdout)
        .expect("Python executable path must be UTF-8")
        .trim()
        .to_owned()
}

fn run_model(workspace: &TestWorkspace, body: &str) -> BTreeMap<String, Value> {
    workspace.write_model("probe.py", body);
    let result = run_postgres(
        &workspace.project,
        RunRequest {
            targets: vec!["probe.py".to_owned()],
            mode: None,
            profile: None,
            session_id: None,
            variables: BTreeMap::new(),
            confirm_destructive: false,
            full_refresh: false,
        },
    )
    .expect("Python-only run should succeed when the model handles its outcome");
    assert_eq!(result.status, "SUCCEEDED");
    let store =
        SessionStore::for_user_state(default_session_store_path().expect("isolated state path"));
    store
        .snapshot(&result.session_id)
        .expect("load persisted session variables")
}

#[test]
/// ENT-050: UTF-8 output with delimiter, null text, quoting of delimiters and line breaks,
/// booleans and numbers; the temporary sibling does not survive a successful export.
fn generate_csv_writes_utf8_with_options_nulls_and_escaping() {
    let system_interpreter = python_executable();
    let workspace = TestWorkspace::new(&system_interpreter);

    let variables = run_model(
        &workspace,
        r#"def model(env):
    rows = [
        {"id": 1, "name": 'Ana; "Silva"', "note": "linha\nDois", "ok": True, "valor": None},
        {"id": 2, "name": "José ção", "note": "", "ok": False, "valor": 10.5},
    ]
    env.setVar("count", env.generateCsv(rows, "exports/people.csv", delimiter=";", null="NULL"))
    return None
"#,
    );

    assert_eq!(variables["count"], json!(2));
    let written =
        fs::read(workspace.project.join("exports").join("people.csv")).expect("export file exists");
    let text = String::from_utf8(written).expect("export is UTF-8");
    assert_eq!(
        text,
        "id;name;note;ok;valor\n1;\"Ana; \"\"Silva\"\"\";\"linha\nDois\";true;NULL\n2;José ção;;false;10.5\n"
    );
    let leftovers: Vec<String> = fs::read_dir(workspace.project.join("exports"))
        .expect("list export directory")
        .map(|entry| {
            entry
                .expect("directory entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    assert_eq!(leftovers, vec!["people.csv".to_owned()]);
}

#[test]
/// ENT-050: 200 000 rows (about 26 MB) are written while the traced Python heap during the
/// export stays far below the file size, i.e. the export is streamed, not materialized.
fn generate_csv_streams_large_export_within_memory_bound() {
    let system_interpreter = python_executable();
    let workspace = TestWorkspace::new(&system_interpreter);

    let variables = run_model(
        &workspace,
        r#"import tracemalloc

def model(env):
    payload = "x" * 120

    def rows():
        for index in range(200000):
            yield {"id": index, "payload": payload}

    tracemalloc.start()
    try:
        count = env.generateCsv(rows(), "big/export.csv")
        peak = tracemalloc.get_traced_memory()[1]
    finally:
        tracemalloc.stop()
    env.setVar("count", count)
    env.setVar("peak", peak)
    env.setVar("peak_below_limit", peak < 8 * 1024 * 1024)
    return None
"#,
    );

    assert_eq!(variables["count"], json!(200000));
    assert_eq!(
        variables["peak_below_limit"],
        json!(true),
        "peak Python heap {} bytes during the export",
        variables["peak"]
    );
    let path = workspace.project.join("big").join("export.csv");
    let size = fs::metadata(&path).expect("large export exists").len();
    // 200 000 rows of about 127 bytes each: the file must be far larger than the traced heap.
    assert!(size > 20 * 1024 * 1024, "export is {size} bytes");
    let head = fs::read(&path).expect("read export head");
    assert!(head.starts_with(b"id,payload\n0,xxx"));
}

#[test]
/// ENT-050: destinations outside the project, with `..`, reserved entries, a symbolic link in
/// the path or NUL bytes are refused, and nothing is written outside the project.
#[cfg(unix)]
fn generate_csv_refuses_destinations_outside_project_or_through_symlinks() {
    let system_interpreter = python_executable();
    let workspace = TestWorkspace::new(&system_interpreter);
    let outside = workspace.root.join("outside");
    fs::create_dir_all(&outside).expect("create directory outside the project");
    std::os::unix::fs::symlink(&outside, workspace.project.join("link"))
        .expect("create symbolic link inside the project");
    let absolute = outside.join("absolute.csv");
    let model = format!(
        r#"def attempt(env, name, path):
    try:
        env.generateCsv([{{"a": 1}}], path)
        env.setVar(name, "ALLOWED")
    except Exception as error:
        env.setVar(name, getattr(error, "code", None) or type(error).__name__)

def model(env):
    attempt(env, "parent", "../escape.csv")
    attempt(env, "absolute", {absolute:?})
    attempt(env, "reserved_models", "models/injected.py")
    attempt(env, "reserved_manifest", "dowai_project.json")
    attempt(env, "reserved_case", "Models/Injected.py")
    attempt(env, "through_link", "link/escaped.csv")
    attempt(env, "empty", "")
    attempt(env, "nul", "a\x00b.csv")
    attempt(env, "dot", ".")
    return None
"#
    );

    let variables = run_model(&workspace, &model);

    assert_eq!(variables["parent"], json!("DOWAI_POLICY_PATH"));
    assert_eq!(variables["absolute"], json!("DOWAI_POLICY_DENIED"));
    assert_eq!(variables["reserved_models"], json!("DOWAI_POLICY_DENIED"));
    assert_eq!(variables["reserved_manifest"], json!("DOWAI_POLICY_DENIED"));
    assert_eq!(variables["reserved_case"], json!("DOWAI_POLICY_DENIED"));
    assert_eq!(variables["through_link"], json!("DOWAI_POLICY_DENIED"));
    assert_eq!(variables["empty"], json!("ValueError"));
    assert_eq!(variables["nul"], json!("ValueError"));
    assert_eq!(variables["dot"], json!("DOWAI_POLICY_PATH"));
    assert!(!workspace.root.join("escape.csv").exists());
    assert!(!absolute.exists());
    assert!(!outside.join("escaped.csv").exists());
    assert!(!workspace
        .project
        .join("models")
        .join("injected.py")
        .exists());
}

#[test]
/// ENT-050: invalid options and inconsistent rows raise ValueError before or during the write;
/// a failed export leaves neither the destination nor a temporary file behind.
fn generate_csv_rejects_invalid_options_and_leaves_no_partial_file() {
    let system_interpreter = python_executable();
    let workspace = TestWorkspace::new(&system_interpreter);

    let variables = run_model(
        &workspace,
        r#"import datetime

def attempt(env, name, call):
    try:
        call()
        env.setVar(name, "ALLOWED")
    except ValueError:
        env.setVar(name, "ValueError")

def model(env):
    attempt(env, "unknown_option", lambda: env.generateCsv([{"a": 1}], "o.csv", quote="x"))
    attempt(env, "quote_delimiter", lambda: env.generateCsv([{"a": 1}], "o.csv", delimiter='"'))
    attempt(env, "long_delimiter", lambda: env.generateCsv([{"a": 1}], "o.csv", delimiter="ab"))
    attempt(env, "bad_header", lambda: env.generateCsv([{"a": 1}], "o.csv", header="yes"))
    attempt(env, "bad_timezone", lambda: env.generateCsv([{"a": 1}], "o.csv", timezone="Mars/Olympus"))
    attempt(env, "ragged_rows", lambda: env.generateCsv([{"a": 1}, {"a": 2, "b": 3}], "o.csv"))
    attempt(env, "not_mapping", lambda: env.generateCsv([[1, 2]], "o.csv"))
    attempt(env, "naive_datetime", lambda: env.generateCsv([{"t": datetime.datetime(2026, 1, 1, 12)}], "o.csv", timezone="UTC"))
    attempt(env, "bytes_value", lambda: env.generateCsv([{"b": b"x"}], "o.csv"))
    attempt(env, "string_data", lambda: env.generateCsv("abc", "o.csv"))
    env.setVar("ok", env.generateCsv([{"a": 1}], "o.csv") == 1)
    return None
"#,
    );

    for name in [
        "unknown_option",
        "quote_delimiter",
        "long_delimiter",
        "bad_header",
        "bad_timezone",
        "ragged_rows",
        "not_mapping",
        "naive_datetime",
        "bytes_value",
        "string_data",
    ] {
        assert_eq!(variables[name], json!("ValueError"), "{name}");
    }
    assert_eq!(variables["ok"], json!(true));
    let names: Vec<String> = fs::read_dir(&workspace.project)
        .expect("list project directory")
        .map(|entry| {
            entry
                .expect("directory entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    assert!(names.contains(&"o.csv".to_owned()));
    assert!(names.iter().all(|name| !name.ends_with(".dowai-tmp")));
}
