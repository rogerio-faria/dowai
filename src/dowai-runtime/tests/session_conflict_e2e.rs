//! AC040: concurrent writes to the same session variable are an explicit conflict,
//! never a silent last-writer-wins. Synchronisation uses blocking FIFOs, not sleeps.
#![cfg(unix)]

use dowai_core::{
    create_project, default_audit_log_path, default_session_store_path, SessionStore,
};
use dowai_runtime::{run_postgres, RunRequest};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use uuid::Uuid;

struct Env {
    root: PathBuf,
    project: PathBuf,
    saved: Vec<(&'static str, Option<OsString>)>,
}

impl Env {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("dowai-conflict-e2e-{}", Uuid::new_v4()));
        fs::create_dir_all(root.join("state")).expect("state");
        fs::create_dir_all(root.join("config")).expect("config");
        let mut saved = Vec::new();
        for (name, value) in [
            ("XDG_STATE_HOME", root.join("state")),
            ("XDG_CONFIG_HOME", root.join("config")),
            ("LOCALAPPDATA", root.join("state")),
        ] {
            saved.push((name, std::env::var_os(name)));
            std::env::set_var(name, value);
        }
        let project = create_project(&root, "conflict").expect("project");
        let venv = project.join(".venv");
        let created = Command::new("python3")
            .args(["-m", "venv", "--without-pip"])
            .arg(&venv)
            .output()
            .expect("venv");
        assert!(created.status.success());
        let manifest = project.join("dowai_project.json");
        let mut config: Value =
            serde_json::from_slice(&fs::read(&manifest).expect("read")).expect("manifest json");
        config["python"] = json!({"interpreter": venv.join("bin/python"), "timeout_seconds": 60});
        config["config"]["security"]["allow_external_writes"] = json!(true);
        fs::write(&manifest, serde_json::to_vec(&config).expect("ser")).expect("write");
        Self {
            root,
            project,
            saved,
        }
    }

    fn model(&self, name: &str, body: &str) {
        fs::write(self.project.join("models").join(name), body).expect("model");
    }

    fn fifo(&self, name: &str) -> PathBuf {
        let path = self.root.join(name);
        assert!(Command::new("mkfifo")
            .arg(&path)
            .status()
            .expect("mkfifo")
            .success());
        path
    }

    fn request(&self, target: &str, session: Option<&str>) -> RunRequest {
        RunRequest {
            targets: vec![target.to_owned()],
            mode: None,
            profile: None,
            session_id: session.map(str::to_owned),
            variables: BTreeMap::new(),
            confirm_destructive: false,
        }
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        for (name, value) in self.saved.drain(..) {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn blocking_write(path: &Path) {
    fs::write(path, b"go").expect("release peer through FIFO");
}

fn blocking_read(path: &Path) {
    fs::read(path).expect("wait for peer through FIFO");
}

#[test]
fn concurrent_runs_writing_the_same_variable_conflict_without_last_writer_wins() {
    let env = Env::new();
    let started = env.fifo("b_started");
    let gate = env.fifo("b_gate");
    env.model(
        "seed.py",
        "def model(env):\n    env.setVar('shared', 'SENTINEL-SEED')\n    env.setVar('other', 'SENTINEL-OTHER')\n",
    );
    // B reads its snapshot, writes, then blocks on the FIFO until A has published.
    env.model(
        "b.py",
        &format!(
            "def model(env):\n    env.setVar('shared', 'SENTINEL-B')\n    open({started:?}, 'w').write('x')\n    open({gate:?}).read()\n"
        ),
    );
    env.model(
        "a.py",
        "def model(env):\n    env.setVar('shared', 'SENTINEL-A')\n",
    );
    env.model(
        "c.py",
        "def model(env):\n    env.setVar('unrelated', 'SENTINEL-C')\n",
    );

    let seed = run_postgres(&env.project, env.request("seed.py", None)).expect("seed run");
    let session = seed.session_id.clone();
    let store = SessionStore::for_user_state(default_session_store_path().expect("path"));

    let project = env.project.clone();
    let request_b = env.request("b.py", Some(&session));
    let run_b = thread::spawn(move || run_postgres(&project, request_b));
    blocking_read(&started); // B has its snapshot and has finished its write.

    run_postgres(&env.project, env.request("a.py", Some(&session))).expect("A wins");
    // A run that touches a different key is unaffected by A or B.
    run_postgres(&env.project, env.request("c.py", Some(&session))).expect("C unrelated");
    blocking_write(&gate);

    let error = run_b
        .join()
        .expect("B thread")
        .expect_err("B must conflict");
    assert_eq!(error.code(), "DOWAI_SESSION_CONFLICT");
    assert_eq!(error.exit_code(), 3);
    assert!(error.message().contains("shared"));
    assert!(!error.to_string().contains("SENTINEL"));

    let values = store.snapshot(&session).expect("snapshot");
    assert_eq!(values["shared"], json!("SENTINEL-A"), "winner value kept");
    assert_eq!(values["other"], json!("SENTINEL-OTHER"));
    assert_eq!(values["unrelated"], json!("SENTINEL-C"));

    let audit = fs::read_to_string(default_audit_log_path().expect("audit path")).expect("audit");
    assert!(audit.contains("DOWAI_SESSION_CONFLICT"));
    assert!(!audit.contains("SENTINEL"), "no variable value in audit");
}
