//! Real-signal cancellation of `dowai run` (plan 5.5, 6.4, AC056).
//! SIGINT/SIGTERM are Unix-only; on Windows the cancellation contract is covered by
//! the token-based tests in dowai-runtime (no real Ctrl+C is injected there).
#![cfg(unix)]

use dowai_core::create_project;
use serde_json::{json, Value};
use std::fs;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    root: PathBuf,
    project: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let id = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "dowai-cancel-cli-{}-{nonce}-{id}",
            std::process::id()
        ));
        fs::create_dir(&root).expect("create fixture");
        fs::create_dir_all(root.join("state")).expect("state dir");
        fs::create_dir_all(root.join("config")).expect("config dir");
        let project = create_project(&root, "fixture").expect("project");
        let environment = project.join(".venv");
        let created = Command::new("python3")
            .args(["-m", "venv", "--without-pip"])
            .arg(&environment)
            .output()
            .expect("venv");
        assert!(created.status.success());
        let manifest_path = project.join("dowai_project.json");
        let mut manifest: Value =
            serde_json::from_slice(&fs::read(&manifest_path).expect("read")).expect("json");
        manifest["python"] = json!({
            "interpreter": environment.join("bin").join("python"),
            "timeout_seconds": 60
        });
        manifest["config"]["security"]["allow_external_writes"] = json!(true);
        fs::write(&manifest_path, serde_json::to_vec(&manifest).expect("enc")).expect("write");
        let pids = root.join("pids");
        fs::create_dir_all(&pids).expect("pids");
        fs::write(
            project.join("models").join("slow.py"),
            format!(
                "import os, subprocess, time\n\
                 def model(env):\n\
                 \x20   child = subprocess.Popen(['sleep', '60'])\n\
                 \x20   open({child:?}, 'w').write(str(child.pid))\n\
                 \x20   open({worker:?}, 'w').write(str(os.getpid()))\n\
                 \x20   time.sleep(30)\n\
                 \x20   env.setVar('should_not_exist', 1)\n",
                child = pids.join("child").to_string_lossy(),
                worker = pids.join("worker").to_string_lossy(),
            ),
        )
        .expect("model");
        Self { root, project }
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_dowai"));
        command
            .args(args)
            .current_dir(&self.project)
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("LOCALAPPDATA", self.root.join("state"))
            .env("APPDATA", self.root.join("config"))
            .env("PROGRAMDATA", self.root.join("system"));
        command
    }

    fn pid(&self, name: &str) -> Option<u32> {
        fs::read_to_string(self.root.join("pids").join(name))
            .ok()
            .and_then(|text| text.trim().parse().ok())
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for name in ["child", "worker"] {
            if let Some(pid) = self.pid(name) {
                let _ = Command::new("kill")
                    .args(["-KILL", &pid.to_string()])
                    .output();
            }
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn alive(pid: u32) -> bool {
    let output = Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .expect("ps");
    let state = String::from_utf8_lossy(&output.stdout);
    let state = state.trim();
    !state.is_empty() && !state.starts_with('Z')
}

fn wait_for_pids(fixture: &Fixture, child: &mut Child) -> (u32, u32) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let (Some(worker), Some(grandchild)) = (fixture.pid("worker"), fixture.pid("child")) {
            return (worker, grandchild);
        }
        assert!(Instant::now() < deadline, "model never started");
        assert!(
            child.try_wait().expect("poll").is_none(),
            "CLI exited early"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn signal_and_verify(signal: &str) {
    let fixture = Fixture::new();
    let mut cli = fixture
        .command(&["run", "slow.py", "--json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn CLI");
    let (worker, grandchild) = wait_for_pids(&fixture, &mut cli);
    std::thread::sleep(Duration::from_millis(500));
    let sent = Instant::now();
    let status = Command::new("kill")
        .args([signal, &cli.id().to_string()])
        .status()
        .expect("kill");
    assert!(status.success());
    let output = cli.wait_with_output().expect("wait CLI");
    assert!(
        sent.elapsed() < Duration::from_secs(10),
        "cancellation too slow"
    );
    assert_eq!(output.status.code(), Some(130), "exit code must be 130");

    let stdout: Value = serde_json::from_slice(&output.stdout).expect("JSON error output");
    assert_eq!(stdout["error_code"], json!("DOWAI_CANCELLED"));

    let runs = fixture
        .command(&["runs", "list", "--json"])
        .output()
        .expect("runs list");
    let runs: Value = serde_json::from_slice(&runs.stdout).expect("runs JSON");
    let text = runs.to_string();
    assert!(
        text.contains("\"CANCELLED\""),
        "journal must show CANCELLED: {text}"
    );
    assert!(
        text.contains("DOWAI_CANCELLED"),
        "stable error code: {text}"
    );

    std::thread::sleep(Duration::from_millis(300));
    assert!(!alive(worker), "Python worker must not survive");
    assert!(!alive(grandchild), "Python descendants must not survive");

    let audit = fs::read_to_string(fixture.root.join("state/dowai/audit.jsonl")).expect("audit");
    let finished: Vec<Value> = audit
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|event| event["event"] == "run.finished")
        .collect();
    assert_eq!(finished.len(), 1);
    assert_eq!(finished[0]["result"], json!("CANCELLED"));
    assert_eq!(finished[0]["error_code"], json!("DOWAI_CANCELLED"));
    assert!(
        !audit.contains("variables.published"),
        "no variables on cancel"
    );
}

#[test]
fn sigint_cancels_the_run_and_cleans_the_process_tree() {
    signal_and_verify("-INT");
}

#[test]
fn sigterm_cancels_the_run_and_cleans_the_process_tree() {
    signal_and_verify("-TERM");
}
