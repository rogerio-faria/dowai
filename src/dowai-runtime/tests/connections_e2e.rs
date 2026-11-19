//! ENT-043: `env.connections.get(name)` returns an opaque handle for an authorized workspace
//! profile. The handle never exposes URL, host, user, database or password, and profiles
//! outside `security.allowed_profiles` are denied by the host.

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

const SECRET_ENV: &str = "DOWAI_ENT043_TEST_PASSWORD";
const SENTINEL: &str = "ent043-sentinel-secret-7f3a";

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
            "dowai-ent043-{}-{nonce}-{sequence}",
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

        // Two profiles: "approved" and "other". Only the first one is allowed by policy.
        let workspace_file = root.join("profiles.json");
        let mut workspace: Value =
            serde_json::from_slice(&fs::read(&workspace_file).expect("read profiles.json"))
                .expect("profiles.json is valid JSON");
        let profile = |database: &str| {
            json!({
                "type": "postgres",
                "host": "db.ent043.test",
                "port": 5432,
                "user": "ent043_user",
                "database": database,
                "password": format!("{{{{ env_var('{SECRET_ENV}') }}}}"),
                "tls": "required"
            })
        };
        workspace["profiles"] = json!({
            "approved": profile("analytics"),
            "other": profile("warehouse")
        });
        fs::write(
            &workspace_file,
            serde_json::to_vec(&workspace).expect("serialize workspace"),
        )
        .expect("write profiles.json");

        let manifest = project.join("dowai_project.json");
        let mut project_config: Value =
            serde_json::from_slice(&fs::read(&manifest).expect("read project config"))
                .expect("project config is valid JSON");
        project_config["python"] = json!({
            "interpreter": interpreter,
            "timeout_seconds": 30
        });
        project_config["config"]["security"]["allow_external_writes"] = json!(true);
        project_config["config"]["security"]["allowed_profiles"] = json!(["approved"]);
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

fn run_model(workspace: &TestWorkspace, body: &str) -> Value {
    serde_json::to_value(run_model_variables(workspace, body)).expect("variables are JSON")
}

fn run_model_variables(workspace: &TestWorkspace, body: &str) -> BTreeMap<String, Value> {
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
/// ENT-043: an authorized handle exposes only name and kind; repr/str carry no secret, the
/// handle is read-only and refuses pickling, and no URL/password attribute exists.
fn authorized_connection_handle_exposes_no_connection_details() {
    let system_interpreter = python_executable();
    let workspace = TestWorkspace::new(&system_interpreter);
    let previous = std::env::var_os(SECRET_ENV);
    std::env::set_var(SECRET_ENV, SENTINEL);

    let variables = run_model(
        &workspace,
        r#"import pickle

def model(env):
    conn = env.connections.get('approved')
    env.setVar('name', conn.name)
    env.setVar('kind', conn.kind)
    env.setVar('text', repr(conn) + ' ' + str(conn))
    env.setVar('leaks', [a for a in ('url', 'password', 'host', 'user', 'database', 'password_env', 'dsn') if hasattr(conn, a)])
    try:
        conn.name = 'other'
        env.setVar('mutable', True)
    except AttributeError:
        env.setVar('mutable', False)
    try:
        pickle.dumps(conn)
        env.setVar('picklable', True)
    except TypeError:
        env.setVar('picklable', False)
    return None
"#,
    );

    match previous {
        Some(value) => std::env::set_var(SECRET_ENV, value),
        None => std::env::remove_var(SECRET_ENV),
    }

    assert_eq!(variables["name"], json!("approved"));
    assert_eq!(variables["kind"], json!("postgres"));
    assert_eq!(variables["leaks"], json!([]));
    assert_eq!(variables["mutable"], json!(false));
    assert_eq!(variables["picklable"], json!(false));
    let text = variables["text"].as_str().expect("repr is text");
    assert!(text.contains("approved"));
    assert!(!text.contains(SENTINEL), "repr must not expose the secret");
    assert!(
        !text.contains("db.ent043.test"),
        "repr must not expose the host"
    );
    let dumped = serde_json::to_string(&variables).expect("serialize variables");
    assert!(
        !dumped.contains(SENTINEL),
        "session variables must not hold the secret"
    );
}

#[test]
/// ENT-043: a profile outside `security.allowed_profiles` is denied with DOWAI_POLICY_DENIED,
/// and an unknown profile is refused without any connection attempt.
fn connection_get_denies_profiles_outside_policy_and_unknown_names() {
    let system_interpreter = python_executable();
    let workspace = TestWorkspace::new(&system_interpreter);

    let variables = run_model(
        &workspace,
        r#"def model(env):
    try:
        env.connections.get('other')
        env.setVar('denied', None)
    except Exception as error:
        env.setVar('denied', error.code)
    try:
        env.connections.get('missing')
        env.setVar('unknown', None)
    except Exception as error:
        env.setVar('unknown', error.code)
    try:
        env.connections.get('')
        env.setVar('empty', None)
    except ValueError:
        env.setVar('empty', 'ValueError')
    return None
"#,
    );

    assert_eq!(variables["denied"], json!("DOWAI_POLICY_DENIED"));
    assert_eq!(variables["unknown"], json!("DOWAI_CONFIG_INVALID"));
    assert_eq!(variables["empty"], json!("ValueError"));
}
