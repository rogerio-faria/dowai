//! `sessions.conflict_policy = "wait_process"`: runs on the same session are serialised by an
//! exclusive per-session lock held for the whole run. Synchronisation between runs uses
//! blocking FIFOs (no sleeps); the only timing-dependent assertions are lower bounds.
#![cfg(unix)]

use dowai_core::{
    create_project, default_audit_log_path, default_run_store_path, default_session_store_path,
    AuditLog, CancellationToken, RunStore, SessionStore,
};
use dowai_runtime::{run_postgres, run_postgres_with_cancel, RunRequest};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{mpsc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};
use uuid::Uuid;

static ENVIRONMENT_LOCK: Mutex<()> = Mutex::new(());

struct Env {
    _lock: MutexGuard<'static, ()>,
    root: PathBuf,
    project: PathBuf,
    saved: Vec<(&'static str, Option<OsString>)>,
}

impl Env {
    fn new(sessions: Value) -> Self {
        let lock = ENVIRONMENT_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let root = std::env::temp_dir().join(format!("dowai-wait-e2e-{}", Uuid::new_v4()));
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
        let project = create_project(&root, "wait").expect("project");
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
        config["config"]["sessions"] = sessions;
        fs::write(&manifest, serde_json::to_vec(&config).expect("ser")).expect("write");
        Self {
            _lock: lock,
            root,
            project,
            saved,
        }
    }

    fn waiting(timeout: u64) -> Self {
        Self::new(
            json!({"conflict_policy": "wait_process", "wait_process_timeout_seconds": timeout}),
        )
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

    /// Creates a session through a first run and returns its id.
    fn seed(&self) -> String {
        self.model(
            "seed.py",
            "def model(env):\n    env.setVar('order', '-')\n    env.setVar('secret', 'SENTINEL-SEED')\n",
        );
        run_postgres(&self.project, self.request("seed.py", None))
            .expect("seed run")
            .session_id
    }

    /// Model that announces it holds the session, then blocks until the gate FIFO is written.
    fn holder(&self, name: &str, started: &Path, gate: &Path, tag: &str) {
        self.model(
            name,
            &format!(
                "def model(env):\n    open({started:?}, 'w').write('x')\n    open({gate:?}).read()\n    env.setVar('order', env.getVar('order') + {tag:?})\n"
            ),
        );
    }

    fn finished_events(&self) -> Vec<dowai_core::AuditEvent> {
        AuditLog::for_user_state(default_audit_log_path().expect("audit path"))
            .read_all()
            .unwrap_or_default()
            .into_iter()
            .filter(|event| event.event == "run.finished")
            .collect()
    }

    fn order(&self, session: &str) -> Value {
        SessionStore::for_user_state(default_session_store_path().expect("path"))
            .snapshot(session)
            .expect("snapshot")["order"]
            .clone()
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

fn release(path: &Path) {
    fs::write(path, b"go").expect("release through FIFO");
}

fn await_peer(path: &Path) {
    fs::read(path).expect("wait through FIFO");
}

#[test]
fn second_run_waits_for_the_first_then_runs_and_both_succeed() {
    let env = Env::waiting(60);
    let session = env.seed();
    let (started, gate) = (env.fifo("a_started"), env.fifo("a_gate"));
    env.holder("a.py", &started, &gate, "A");
    env.model(
        "b.py",
        "def model(env):\n    env.setVar('order', env.getVar('order') + 'B')\n",
    );

    let project = env.project.clone();
    let request_a = env.request("a.py", Some(&session));
    let run_a = thread::spawn(move || run_postgres(&project, request_a));
    await_peer(&started); // A holds the lock now.

    let project = env.project.clone();
    let request_b = env.request("b.py", Some(&session));
    let run_b = thread::spawn(move || run_postgres(&project, request_b));
    release(&gate);

    run_a.join().expect("A thread").expect("A succeeds");
    run_b
        .join()
        .expect("B thread")
        .expect("B succeeds: serialisation is not a conflict");
    // B started only after A published, so it observed A's write and extended it.
    assert_eq!(env.order(&session), json!("-AB"));
}

#[test]
fn wait_timeout_fails_run_with_session_conflict_without_executing_any_node() {
    let env = Env::waiting(1);
    let session = env.seed();
    let (started, gate) = (env.fifo("a_started"), env.fifo("a_gate"));
    env.holder("a.py", &started, &gate, "A");
    let marker = env.root.join("b_ran");
    env.model(
        "b.py",
        &format!("def model(env):\n    open({marker:?}, 'w').write('ran')\n    env.setVar('order', 'SENTINEL-B')\n"),
    );

    let project = env.project.clone();
    let request_a = env.request("a.py", Some(&session));
    let run_a = thread::spawn(move || run_postgres(&project, request_a));
    await_peer(&started);

    let began = Instant::now();
    let error = run_postgres(&env.project, env.request("b.py", Some(&session)))
        .expect_err("B must time out waiting");
    assert!(
        began.elapsed() >= Duration::from_secs(1),
        "waited the configured timeout"
    );
    assert_eq!(error.code(), "DOWAI_SESSION_CONFLICT");
    assert_eq!(error.exit_code(), 3);
    assert!(!error.to_string().contains("SENTINEL"));
    assert!(!marker.exists(), "no node of B may run");

    let failed: Vec<_> = env
        .finished_events()
        .into_iter()
        .filter(|event| event.error_code.as_deref() == Some("DOWAI_SESSION_CONFLICT"))
        .collect();
    assert_eq!(failed.len(), 1, "audit records the timeout once");
    assert_eq!(failed[0].result.as_deref(), Some("FAILED"));

    let store = RunStore::for_user_state(default_run_store_path().expect("path"));
    let journaled = store.list(10).expect("list");
    assert!(journaled
        .iter()
        .any(|run| run.status == "FAILED"
            && run.error_code.as_deref() == Some("DOWAI_SESSION_CONFLICT")));

    release(&gate);
    run_a.join().expect("A thread").expect("A unaffected");
    assert_eq!(env.order(&session), json!("-A"), "nothing from B published");
    let audit = fs::read_to_string(default_audit_log_path().expect("audit path")).expect("audit");
    assert!(!audit.contains("SENTINEL"), "no variable value in audit");
}

fn assert_lock_is_free(env: &Env, session: &str) {
    env.model(
        "next.py",
        "def model(env):\n    env.setVar('order', env.getVar('order') + 'N')\n",
    );
    let began = Instant::now();
    run_postgres(&env.project, env.request("next.py", Some(session))).expect("lock was released");
    assert!(
        began.elapsed() < Duration::from_secs(20),
        "subsequent run must not wait for the 3600 s timeout"
    );
}

#[test]
fn lock_is_released_after_failed_run() {
    let env = Env::waiting(3600);
    let session = env.seed();
    env.model(
        "bad.py",
        "def model(env):\n    raise RuntimeError('boom')\n",
    );
    run_postgres(&env.project, env.request("bad.py", Some(&session))).expect_err("A fails");
    assert_lock_is_free(&env, &session);
}

#[test]
fn lock_is_released_when_run_fails_before_any_node() {
    let env = Env::waiting(3600);
    let session = env.seed();
    env.model("ok.py", "def model(env):\n    pass\n");
    let mut request = env.request("ok.py", Some(&session));
    // Oversized value: rejected after the lock is taken and before any node starts.
    request
        .variables
        .insert("big".to_owned(), json!("x".repeat(70_000)));
    run_postgres(&env.project, request).expect_err("rejected before nodes");
    assert_lock_is_free(&env, &session);
}

#[test]
fn lock_is_released_after_cancellation() {
    let env = Env::waiting(3600);
    let session = env.seed();
    let (started, gate) = (env.fifo("a_started"), env.fifo("a_gate"));
    env.holder("a.py", &started, &gate, "A");
    let token = CancellationToken::new();
    let trigger = token.clone();
    let canceller = thread::spawn(move || {
        await_peer(&started);
        trigger.cancel();
    });
    let error = run_postgres_with_cancel(&env.project, env.request("a.py", Some(&session)), &token)
        .expect_err("A cancelled");
    canceller.join().expect("canceller");
    assert_eq!(error.code(), "DOWAI_CANCELLED");
    assert_lock_is_free(&env, &session);
}

#[test]
fn cancellation_interrupts_the_wait_for_the_lock() {
    let env = Env::waiting(3600);
    let session = env.seed();
    let (started, gate) = (env.fifo("a_started"), env.fifo("a_gate"));
    env.holder("a.py", &started, &gate, "A");
    env.model("b.py", "def model(env):\n    pass\n");

    let project = env.project.clone();
    let request_a = env.request("a.py", Some(&session));
    let run_a = thread::spawn(move || run_postgres(&project, request_a));
    await_peer(&started);

    let token = CancellationToken::new();
    let trigger = token.clone();
    let canceller = thread::spawn(move || {
        thread::sleep(Duration::from_millis(200));
        trigger.cancel();
    });
    let error = run_postgres_with_cancel(&env.project, env.request("b.py", Some(&session)), &token)
        .expect_err("B cancelled while waiting");
    canceller.join().expect("canceller");
    assert_eq!(error.code(), "DOWAI_CANCELLED");
    release(&gate);
    run_a.join().expect("A thread").expect("A succeeds");
}

#[test]
fn nested_env_run_inherits_the_lock_and_never_self_blocks() {
    let env = Env::waiting(3600);
    let session = env.seed();
    env.model(
        "parent.py",
        "def model(env):\n    env.run('child.py')\n    env.setVar('order', env.getVar('order') + 'P')\n",
    );
    env.model(
        "child.py",
        "def model(env):\n    env.setVar('order', env.getVar('order') + 'C')\n",
    );
    let (sender, receiver) = mpsc::channel();
    let project = env.project.clone();
    let request = env.request("parent.py", Some(&session));
    thread::spawn(move || {
        let _ = sender.send(run_postgres(&project, request));
    });
    receiver
        .recv_timeout(Duration::from_secs(30))
        .expect("nested run must not deadlock")
        .expect("nested run succeeds");
    assert_eq!(env.order(&session), json!("-CP"));
}

#[test]
fn default_error_policy_takes_no_lock_and_never_waits() {
    let env = Env::new(json!({}));
    let session = env.seed();
    let (started, gate) = (env.fifo("a_started"), env.fifo("a_gate"));
    env.holder("a.py", &started, &gate, "A");
    env.model("b.py", "def model(env):\n    env.setVar('other', 1)\n");
    let project = env.project.clone();
    let request_a = env.request("a.py", Some(&session));
    let run_a = thread::spawn(move || run_postgres(&project, request_a));
    await_peer(&started);
    // B proceeds immediately although A is still running.
    run_postgres(&env.project, env.request("b.py", Some(&session))).expect("B not blocked");
    release(&gate);
    run_a.join().expect("A thread").expect("A succeeds");
    let state = default_session_store_path().expect("path");
    let locks = state.parent().expect("state dir").join("session-locks");
    assert!(!locks.exists(), "error policy must not create locks");
}
