//! `--session` entre processos reais do binário `dowai` (ENT-027, AC035).
//!
//! Um `dowai run` grava variáveis em uma sessão; um segundo processo CLI, com o mesmo
//! `--session`, deve lê-las. Uma sessão expirada deve ser recusada pelo segundo processo sem
//! executar nenhum nó.
#![cfg(unix)]

use dowai_core::SessionStore;
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const BIN: &str = env!("CARGO_BIN_EXE_dowai");

struct Sandbox {
    root: PathBuf,
    project: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "dowai-session-cross-cli-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(root.join("state")).expect("state dir");
        fs::create_dir_all(root.join("config")).expect("config dir");

        let project = dowai_core::create_project(&root, "cross").expect("project");
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
        fs::write(&manifest, serde_json::to_vec(&config).expect("serialise")).expect("write");

        Self { root, project }
    }

    fn model(&self, name: &str, body: &str) {
        fs::write(self.project.join("models").join(name), body).expect("write model");
    }

    /// Caminho do store de sessões que a CLI usa com este sandbox (`XDG_STATE_HOME`).
    fn session_store(&self) -> SessionStore {
        SessionStore::for_user_state(
            self.root
                .join("state")
                .join("dowai")
                .join("sessions.sqlite3"),
        )
    }

    fn cli(&self, args: &[&str]) -> Output {
        Self::envs(
            Command::new(BIN).args(args).current_dir(&self.project),
            &self.root,
        )
        .output()
        .expect("run DOWAI CLI")
    }

    fn envs<'a>(command: &'a mut Command, root: &Path) -> &'a mut Command {
        command
            .env("XDG_STATE_HOME", root.join("state"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("LOCALAPPDATA", root.join("state"))
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
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

#[test]
fn second_process_reads_variables_written_by_first_process_in_same_session() {
    let sandbox = Sandbox::new();
    sandbox.model(
        "writer.py",
        "def model(env):\n    env.setVar('regiao', 'sul')\n",
    );
    // Falha (exit != 0) se o valor gravado por outro processo não estiver visível.
    sandbox.model(
        "reader.py",
        "def model(env):\n    \
         valor = env.getVar('regiao')\n    \
         assert valor == 'sul', f'valor inesperado: {valor!r}'\n    \
         env.setVar('lido', 'ok')\n",
    );
    sandbox.model(
        "checker.py",
        "def model(env):\n    \
         assert env.getVar('lido') == 'ok', 'variavel gravada pelo leitor nao visivel'\n",
    );

    let first = sandbox.cli(&["run", "writer.py", "--json"]);
    assert_eq!(first.status.code(), Some(0), "writer run must succeed");
    let session_id = json_stdout(&first)["session_id"]
        .as_str()
        .expect("session id")
        .to_owned();

    // Processo 2 e 3: mesma sessão, cada um em um processo CLI separado.
    let second = sandbox.cli(&["run", "reader.py", "--session", &session_id, "--json"]);
    assert_eq!(
        second.status.code(),
        Some(0),
        "reader must see the variable from the previous process: {}",
        String::from_utf8_lossy(&second.stdout)
    );
    assert_eq!(json_stdout(&second)["status"], "SUCCEEDED");

    let third = sandbox.cli(&["run", "checker.py", "--session", &session_id, "--json"]);
    assert_eq!(
        third.status.code(),
        Some(0),
        "variable written by the reader process must be visible: {}",
        String::from_utf8_lossy(&third.stdout)
    );
    assert_eq!(json_stdout(&third)["status"], "SUCCEEDED");
}

#[test]
fn expired_session_is_refused_by_second_process_without_running_nodes() {
    let sandbox = Sandbox::new();
    let marker = sandbox.root.join("reader-executed");
    sandbox.model(
        "reader.py",
        &format!("def model(env):\n    open({marker:?}, 'w').write('x')\n"),
    );

    // Sessão criada no mesmo store da CLI, com TTL mínimo, expira antes do segundo processo.
    let expired = sandbox
        .session_store()
        .create(&sandbox.project, Duration::from_millis(1))
        .expect("create short-lived session");
    std::thread::sleep(Duration::from_millis(50));

    let output = sandbox.cli(&[
        "run",
        "reader.py",
        "--session",
        &expired.session_id,
        "--json",
    ]);
    let payload = json_stdout(&output);
    assert_ne!(
        output.status.code(),
        Some(0),
        "expired session must not be used: {payload}"
    );
    assert_eq!(payload["error_code"], "DOWAI_SESSION_INVALID");
    assert!(
        !marker.exists(),
        "no node may execute with an expired session"
    );
}
