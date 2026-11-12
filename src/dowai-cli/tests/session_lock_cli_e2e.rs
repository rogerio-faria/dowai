//! `sessions.conflict_policy = "wait_process"` exercitado entre dois processos reais do binário
//! `dowai` (ENT-026). O primeiro `dowai run` mantém o lock exclusivo da sessão; o segundo, com
//! timeout curto, deve falhar com `DOWAI_SESSION_CONFLICT` sem executar nenhum nó; depois da
//! liberação, um terceiro run na mesma sessão deve passar, provando que o lock foi liberado.
#![cfg(unix)]

use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const BIN: &str = env!("CARGO_BIN_EXE_dowai");
const WAIT_DEADLINE: Duration = Duration::from_secs(60);

struct Sandbox {
    root: PathBuf,
    project: PathBuf,
}

impl Sandbox {
    fn new(timeout_seconds: u64) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "dowai-session-lock-cli-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(root.join("state")).expect("state dir");
        fs::create_dir_all(root.join("config")).expect("config dir");

        let project = dowai_core::create_project(&root, "wait").expect("project");
        let venv = project.join(".venv");
        let created = Command::new("python3")
            .args(["-m", "venv", "--without-pip"])
            .arg(&venv)
            .output()
            .expect("create venv");
        assert!(created.status.success(), "venv must be created");

        let manifest = project.join("dowai_project.json");
        let mut config: Value =
            serde_json::from_slice(&fs::read(&manifest).expect("read manifest"))
                .expect("manifest json");
        config["python"] = json!({"interpreter": venv.join("bin/python"), "timeout_seconds": 60});
        config["config"]["security"]["allow_external_writes"] = json!(true);
        config["config"]["sessions"] = json!({
            "conflict_policy": "wait_process",
            "wait_process_timeout_seconds": timeout_seconds
        });
        fs::write(&manifest, serde_json::to_vec(&config).expect("serialise")).expect("write");

        Self { root, project }
    }

    fn model(&self, name: &str, body: &str) {
        fs::write(self.project.join("models").join(name), body).expect("write model");
    }

    fn envs<'a>(command: &'a mut Command, root: &Path) -> &'a mut Command {
        command
            .env("XDG_STATE_HOME", root.join("state"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("LOCALAPPDATA", root.join("state"))
    }

    fn cli(&self, args: &[&str]) -> Output {
        Self::envs(
            Command::new(BIN).args(args).current_dir(&self.project),
            &self.root,
        )
        .output()
        .expect("run DOWAI CLI")
    }

    fn spawn_run(&self, target: &str, session: &str) -> Child {
        Self::envs(
            Command::new(BIN)
                .args(["run", target, "--session", session, "--json"])
                .current_dir(&self.project),
            &self.root,
        )
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn DOWAI run")
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// Kills the holder process if the test panics before it is joined.
struct ChildGuard(Option<Child>);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(child) = self.0.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn json_stdout(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "stdout must be one JSON payload ({error}): {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

fn wait_for_file(path: &Path, holder: &mut Child) {
    let started = Instant::now();
    while !path.exists() {
        if let Some(status) = holder.try_wait().expect("poll holder") {
            panic!("holder run exited before signalling start: {status}");
        }
        assert!(
            started.elapsed() < WAIT_DEADLINE,
            "holder did not signal start within {WAIT_DEADLINE:?}"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn second_process_times_out_with_stable_code_while_first_holds_session_lock() {
    let sandbox = Sandbox::new(1);
    sandbox.model("seed.py", "def model(env):\n    env.setVar('order', '-')\n");
    let seed = sandbox.cli(&["run", "seed.py", "--json"]);
    assert_eq!(seed.status.code(), Some(0), "seed run must succeed");
    let session_id = json_stdout(&seed)["session_id"]
        .as_str()
        .expect("session id")
        .to_owned();

    // The holder signals `started` once it owns the session lock, then blocks until `gate`
    // exists. Polling a file (instead of a FIFO) keeps a failing test from hanging forever.
    let started = sandbox.root.join("holder-started");
    let gate = sandbox.root.join("holder-gate");
    sandbox.model(
        "hold.py",
        &format!(
            "import os, time\n\
             def model(env):\n    \
             open({started:?}, 'w').write('x')\n    \
             while not os.path.exists({gate:?}):\n        \
             time.sleep(0.02)\n    \
             env.setVar('order', env.getVar('order') + 'H')\n"
        ),
    );
    sandbox.model(
        "other.py",
        "def model(env):\n    env.setVar('order', env.getVar('order') + 'O')\n",
    );

    let mut holder = ChildGuard(Some(sandbox.spawn_run("hold.py", &session_id)));
    wait_for_file(&started, holder.0.as_mut().expect("holder child"));

    // Second process: same session, lock held elsewhere, timeout = 1 s.
    let contender = sandbox.cli(&["run", "other.py", "--session", &session_id, "--json"]);
    let contender_json = json_stdout(&contender);
    assert_eq!(
        contender_json["error_code"], "DOWAI_SESSION_CONFLICT",
        "timeout must surface the stable session conflict code"
    );
    assert_eq!(
        contender.status.code(),
        Some(dowai_core::DowaiError::SessionConflict(String::new()).exit_code()),
        "exit code must follow the SessionConflict contract"
    );

    // Release the holder and collect its result.
    fs::write(&gate, "go").expect("release holder");
    let holder_child = holder.0.take().expect("holder child");
    let holder_output = holder_child.wait_with_output().expect("holder output");
    assert_eq!(
        holder_output.status.code(),
        Some(0),
        "holder must finish successfully: {}",
        String::from_utf8_lossy(&holder_output.stderr)
    );
    assert_eq!(json_stdout(&holder_output)["status"], "SUCCEEDED");

    // Lock released across processes: a third run on the same session now succeeds.
    let after = sandbox.cli(&["run", "other.py", "--session", &session_id, "--json"]);
    assert_eq!(
        after.status.code(),
        Some(0),
        "run after release must succeed: {}",
        String::from_utf8_lossy(&after.stdout)
    );
    assert_eq!(json_stdout(&after)["status"], "SUCCEEDED");
}
