//! ENT-059: plugin oficial `email` (plano §9.5) executado pelo worker real contra um gateway SMTP
//! falso em loopback (`tests/plugins/email/fake_smtp.py`). Usa o `python3` do sistema.
//!
//! O código do plugin é lido de `plugins/email/` na raiz do repositório, sem cópia manual.
//! Um único teste controla a variável de ambiente do segredo, para não haver corrida entre
//! testes em paralelo.

use dowai_core::plugin_registry::{discover, resolve_call, PluginRoots, PluginScope};
use dowai_core::{AuditLog, DowaiError, RunStore};
use dowai_runtime::{execute_plugin, PluginExecution, PluginPolicy};
use serde_json::{json, Value};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use uuid::Uuid;

const MANIFEST: &str = include_str!("../../../plugins/email/plugin.json");
const SENDER: &str = include_str!("../../../plugins/email/sender.py");
const FAKE_SMTP: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../tests/plugins/email/fake_smtp.py"
);
const SECRET_PASSWORD: &str = "senha-de-teste-ent059";

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("dowai-plugin-email-{}", Uuid::new_v4()));
        let plugin = root.join("project/plugins/email");
        fs::create_dir_all(&plugin).expect("plugin dir");
        fs::create_dir_all(root.join("workspace")).expect("workspace");
        fs::create_dir_all(root.join("state")).expect("state");
        fs::create_dir_all(root.join("attachments")).expect("attachments");
        fs::write(plugin.join("plugin.json"), MANIFEST).expect("manifest");
        fs::write(plugin.join("sender.py"), SENDER).expect("module");
        fs::write(
            root.join("attachments/relatorio.txt"),
            "conteudo do relatorio",
        )
        .expect("attachment");
        fs::write(root.join("segredo.txt"), "fora do diretorio").expect("outside file");
        Self { root }
    }

    fn execute(
        &self,
        input: Value,
        key: Option<&str>,
        policy: &PluginPolicy,
    ) -> Result<Value, DowaiError> {
        self.execute_target("email.send", input, key, policy)
    }

    fn execute_target(
        &self,
        target: &str,
        input: Value,
        key: Option<&str>,
        policy: &PluginPolicy,
    ) -> Result<Value, DowaiError> {
        let roots = PluginRoots {
            project: Some(self.root.join("project/plugins")),
            global: Some(self.root.join("workspace")),
        };
        let catalog = discover(&roots, PluginScope::All)?;
        let call = resolve_call(&catalog, target, true)?;
        let store = RunStore::new(self.root.join("state/runs.sqlite3"));
        let audit = AuditLog::new(self.root.join("state/audit.jsonl"));
        execute_plugin(&PluginExecution {
            call: &call,
            interpreter: Path::new("python3"),
            input,
            idempotency_key: key,
            store: Some(&store),
            audit: Some(&audit),
            policy,
        })
    }

    fn attachments_dir(&self) -> String {
        self.root.join("attachments").display().to_string()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// Gateway SMTP falso como processo filho, que termina com o teste.
struct Smtp {
    child: Child,
    port: u16,
    log: PathBuf,
    trace: PathBuf,
}

impl Smtp {
    fn start(log: PathBuf) -> Self {
        let trace = log.with_extension("trace");
        let mut child = Command::new("python3")
            .args(["-I", FAKE_SMTP])
            .arg(&log)
            .arg("0")
            .arg(&trace)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("start fake SMTP");
        let stdout = child.stdout.take().expect("stdout");
        let mut line = String::new();
        BufReader::new(stdout)
            .read_line(&mut line)
            .expect("read port line");
        let port = line
            .trim()
            .strip_prefix("PORT ")
            .and_then(|value| value.parse().ok())
            .expect("fake SMTP announces its port");
        Self {
            child,
            port,
            log,
            trace,
        }
    }

    /// Mensagens aceitas pelo gateway falso (uma linha JSON por mensagem).
    fn records(&self) -> Vec<Value> {
        fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).expect("JSON record"))
            .collect()
    }
}

impl Drop for Smtp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn open_policy() -> PluginPolicy {
    PluginPolicy {
        allow_network: true,
        allow_secrets: true,
        ..PluginPolicy::default()
    }
}

fn base_input(smtp: &Smtp, fixture: &Fixture, to: &str) -> Value {
    json!({
        "sender": "dowai@example.org",
        "to": to,
        "subject": "Alerta",
        "host": "127.0.0.1",
        "port": smtp.port,
        "tls": "none",
        "timeout_seconds": 5,
        "attachments_dir": fixture.attachments_dir(),
    })
}

#[test]
fn refuses_declared_secret_when_policy_denies_secrets_before_any_send() {
    let fixture = Fixture::new();
    let smtp = Smtp::start(fixture.root.join("smtp-deny.jsonl"));
    let policy = PluginPolicy {
        allow_network: true,
        allow_secrets: false,
        ..PluginPolicy::default()
    };
    let error = fixture
        .execute(
            base_input(&smtp, &fixture, "ops@example.org"),
            Some("negado.001"),
            &policy,
        )
        .expect_err("secrets denied by policy");
    assert_eq!(error.code(), "DOWAI_POLICY_DENIED");
    assert!(smtp.records().is_empty(), "nothing may be sent");
}

#[test]
fn sends_deduplicates_by_key_and_never_leaks_credentials() {
    // Único teste que mexe na variável de ambiente do segredo (ver cabeçalho do arquivo).
    std::env::set_var(
        "DOWAI_SECRET_MAIL_CREDENTIALS",
        json!({"username": "robo@example.org", "password": SECRET_PASSWORD}).to_string(),
    );
    let fixture = Fixture::new();
    let smtp = Smtp::start(fixture.root.join("smtp.jsonl"));
    let policy = open_policy();

    // 1. Envio com template e chave: uma mensagem aceita.
    let mut input = base_input(&smtp, &fixture, "ops@example.org");
    input["template"] = json!("Olá $nome, alerta $tipo");
    input["context"] = json!({"nome": "Ana", "tipo": "limiar"});
    let first = fixture
        .execute(input.clone(), Some("envio.001"), &policy)
        .unwrap_or_else(|error| {
            let trace = fs::read_to_string(&smtp.trace)
                .unwrap_or_else(|error| format!("unavailable: {error}"));
            panic!("first send failed: {error}; SMTP stages: {trace}");
        });
    assert_eq!(first["status"], "SUCCEEDED");
    assert!(
        !first.to_string().contains(SECRET_PASSWORD),
        "result must not contain the password"
    );
    let records = smtp.records();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["to"], json!(["ops@example.org"]));
    assert_eq!(records[0]["subject"], "Alerta");

    // 2. Mesma chave depois do sucesso: DUPLICATE, sem novo envio.
    let duplicate = fixture
        .execute(input.clone(), Some("envio.001"), &policy)
        .expect("duplicate is reported, not repeated");
    assert_eq!(duplicate["status"], "DUPLICATE");
    assert_eq!(smtp.records().len(), 1);

    // 3. Recusa do servidor antes do DATA: FAILED, e a mesma chave nunca é reenviada.
    let rejected = base_input(&smtp, &fixture, "reject@example.org");
    let error = fixture
        .execute(rejected.clone(), Some("envio.002"), &policy)
        .expect_err("server refuses recipient");
    assert_eq!(error.code(), "DOWAI_PLUGIN_FAILED");
    let retry = fixture
        .execute(rejected, Some("envio.002"), &policy)
        .expect_err("failed key is not repeated");
    assert_eq!(retry.code(), "DOWAI_PLUGIN_IDEMPOTENCY_FAILED");
    assert_eq!(smtp.records().len(), 1);

    // 4. Queda após o DATA: incerta, e a mesma chave nunca é reenviada.
    let dropped = base_input(&smtp, &fixture, "drop@example.org");
    let error = fixture
        .execute(dropped.clone(), Some("envio.003"), &policy)
        .expect_err("delivery outcome is uncertain");
    assert_eq!(error.code(), "DOWAI_PLUGIN_FAILED");
    let retry = fixture
        .execute(dropped, Some("envio.003"), &policy)
        .expect_err("uncertain key is not repeated");
    assert_eq!(retry.code(), "DOWAI_PLUGIN_IDEMPOTENCY_UNCERTAIN");
    assert_eq!(smtp.records().len(), 1);

    // 5. Template com variável ausente: recusado antes de conectar.
    let mut missing = input.clone();
    missing["context"] = json!({"nome": "Ana"});
    let error = fixture
        .execute(missing, Some("envio.004"), &policy)
        .expect_err("missing template variable");
    assert_eq!(error.code(), "DOWAI_PLUGIN_FAILED");
    assert_eq!(smtp.records().len(), 1);

    // 6. Injeção de cabeçalho e anexo fora de attachments_dir: recusados antes de conectar.
    let mut injected = input.clone();
    injected["subject"] = json!("Alerta\r\nBcc: espiao@example.org");
    assert!(fixture
        .execute(injected, Some("envio.005"), &policy)
        .is_err());
    let mut traversal = input.clone();
    traversal["attachments"] = json!(["../segredo.txt"]);
    assert!(fixture
        .execute(traversal, Some("envio.006"), &policy)
        .is_err());
    assert_eq!(smtp.records().len(), 1);

    // 7. Anexo válido dentro de attachments_dir: enviado com o conteúdo do arquivo.
    let mut attached = base_input(&smtp, &fixture, "ops@example.org");
    attached["attachments"] = json!(["relatorio.txt"]);
    let result = fixture
        .execute(attached, Some("envio.007"), &policy)
        .expect("attachment send succeeds");
    assert_eq!(result["status"], "SUCCEEDED");
    let records = smtp.records();
    assert_eq!(records.len(), 2);
    assert!(records[1]["bytes"].as_u64().unwrap_or(0) > 20);

    // 8. Lote: duas mensagens em uma sessão, com uma chave.
    let batch = json!({
        "sender": "dowai@example.org",
        "host": "127.0.0.1",
        "port": smtp.port,
        "tls": "none",
        "timeout_seconds": 5,
        "attachments_dir": fixture.attachments_dir(),
        "messages": [
            {"to": "a@example.org", "subject": "Lote 1", "message": "um"},
            {"to": ["b@example.org", "c@example.org"], "subject": "Lote 2", "message": "dois"}
        ]
    });
    let target = "email.send_batch";
    let result = fixture
        .execute_target(target, batch, Some("lote.001"), &policy)
        .expect("batch succeeds");
    assert_eq!(result["result"]["messages"], 2);
    assert_eq!(result["result"]["recipients"], 3);
    assert_eq!(smtp.records().len(), 4);

    std::env::remove_var("DOWAI_SECRET_MAIL_CREDENTIALS");
}
