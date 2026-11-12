//! ENT-058: plugin oficial `rest` (plano §9.5, AC015) executado pelo worker real com API falsa
//! local. Usa o `python3` do sistema, como os demais testes de plugin.
//!
//! O código do plugin é lido de `plugins/rest/` na raiz do repositório, sem cópia manual.

use dowai_core::plugin_registry::{discover, resolve_call, PluginRoots, PluginScope};
use dowai_core::{AuditLog, DowaiError, RunStore};
use dowai_runtime::{execute_plugin, PluginExecution, PluginPolicy};
use serde_json::{json, Value};
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::thread;
use uuid::Uuid;

const MANIFEST: &str = include_str!("../../../plugins/rest/plugin.json");
const CLIENT: &str = include_str!("../../../plugins/rest/client.py");

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("dowai-plugin-rest-{}", Uuid::new_v4()));
        let plugin = root.join("project/plugins/rest");
        fs::create_dir_all(&plugin).expect("plugin dir");
        fs::create_dir_all(root.join("workspace")).expect("workspace");
        fs::create_dir_all(root.join("state")).expect("state");
        fs::write(plugin.join("plugin.json"), MANIFEST).expect("manifest");
        fs::write(plugin.join("client.py"), CLIENT).expect("module");
        Self { root }
    }

    fn execute(
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
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn open_policy() -> PluginPolicy {
    PluginPolicy {
        allow_network: true,
        allow_secrets: true,
        ..PluginPolicy::default()
    }
}

/// API falsa: responde às requisições em ordem com `(status, cabeçalhos extras, corpo)` e
/// devolve o cabeçalho HTTP completo de cada requisição recebida.
fn fake_api(
    responses: Vec<(u16, &'static str, String)>,
) -> (String, thread::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let base = format!("http://{}", listener.local_addr().expect("addr"));
    let handle = thread::spawn(move || {
        let mut seen = Vec::new();
        for (status, extra, body) in responses {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut buffer = Vec::new();
            let mut chunk = [0u8; 1024];
            while !buffer.windows(4).any(|w| w == b"\r\n\r\n") {
                match stream.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => buffer.extend_from_slice(&chunk[..read]),
                }
            }
            let head = String::from_utf8_lossy(&buffer);
            seen.push(head.into_owned());
            let reason = match status {
                200 => "OK",
                429 => "Too Many Requests",
                _ => "Error",
            };
            let response = format!(
                "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
        }
        seen
    });
    (base, handle)
}

fn code(error: &DowaiError) -> &'static str {
    error.code()
}

#[test]
fn paginates_by_cursor_and_retries_429_with_counts_and_no_duplicates() {
    let fixture = Fixture::new();
    let (base, server) = fake_api(vec![
        (
            429,
            "Retry-After: 0\r\n",
            json!({"error": "slow"}).to_string(),
        ),
        (
            200,
            "",
            json!({"data": [{"id": 1}, {"id": 2}], "next_cursor": "c2"}).to_string(),
        ),
        (
            200,
            "",
            json!({"data": [{"id": 3}], "next_cursor": null}).to_string(),
        ),
    ]);

    let value = fixture
        .execute(
            "rest.get_pages",
            json!({
                "url": format!("{base}/items"),
                "backoff_seconds": 0,
                "timeout_seconds": 5,
                "max_attempts": 3,
            }),
            Some("pages-1"),
            &open_policy(),
        )
        .expect("paginated read");

    assert_eq!(value["status"], "SUCCEEDED");
    assert_eq!(value["result"]["pages"], 2);
    assert_eq!(value["result"]["records"], 3);
    assert_eq!(value["result"]["retries"], 1);
    let requests = server.join().expect("server");
    assert_eq!(requests.len(), 3);
    assert!(requests[0].contains("GET /items "));
    assert!(requests[1].contains("GET /items "));
    assert!(requests[2].contains("GET /items?cursor=c2 "));
}

#[test]
fn bearer_token_reaches_the_api_and_not_the_result() {
    let fixture = Fixture::new();
    let (base, server) = fake_api(vec![(200, "", json!({"id": "u1"}).to_string())]);

    let value = fixture
        .execute(
            "rest.get",
            json!({
                "url": format!("{base}/me"),
                "token": "tok-super-secreto",
                "timeout_seconds": 5,
                "max_attempts": 1,
            }),
            Some("get-1"),
            &open_policy(),
        )
        .expect("get");

    server.join().expect("server");
    assert_eq!(value["result"]["status"], 200);
    assert!(!value.to_string().contains("tok-super-secreto"));
}

#[test]
fn rest_token_secret_reaches_the_api_when_token_is_absent_and_never_the_result() {
    // Único teste deste arquivo que mexe em DOWAI_SECRET_REST_TOKEN (ENT-109).
    const SECRET: &str = "tok-do-segredo-host";
    std::env::set_var("DOWAI_SECRET_REST_TOKEN", SECRET);
    let fixture = Fixture::new();
    let (base, server) = fake_api(vec![(200, "", json!({"id": "u1"}).to_string())]);

    let value = fixture
        .execute(
            "rest.get",
            json!({
                "url": format!("{base}/me"),
                "timeout_seconds": 5,
                "max_attempts": 1,
            }),
            Some("get-segredo-1"),
            &open_policy(),
        )
        .expect("get com segredo");

    let requests = server.join().expect("server");
    std::env::remove_var("DOWAI_SECRET_REST_TOKEN");
    assert_eq!(value["result"]["status"], 200);
    assert!(
        requests[0].contains(&format!("Authorization: Bearer {SECRET}")),
        "o token do segredo deve chegar à API: {}",
        requests[0]
    );
    assert!(!value.to_string().contains(SECRET));
    let audit = fs::read_to_string(fixture.root.join("state/audit.jsonl")).unwrap_or_default();
    assert!(
        !audit.contains(SECRET),
        "o token não pode aparecer no AUDIT"
    );
}

#[test]
fn network_is_refused_before_plugin_code_when_policy_denies_it() {
    let fixture = Fixture::new();
    let error = fixture
        .execute(
            "rest.get",
            json!({"url": "http://127.0.0.1:9/x"}),
            Some("denied-1"),
            &PluginPolicy {
                allow_secrets: true,
                ..PluginPolicy::default()
            },
        )
        .expect_err("network declared but not allowed");
    assert_eq!(code(&error), "DOWAI_POLICY_DENIED");
}

#[test]
fn timeout_of_the_plugin_becomes_stable_timeout_code() {
    let fixture = Fixture::new();
    // Manifesto de teste com timeout de 2 s; a API aceita a conexão e não responde.
    let mut manifest: Value = serde_json::from_str(MANIFEST).expect("manifest json");
    manifest["timeout_seconds"] = json!(2);
    fs::write(
        fixture.root.join("project/plugins/rest/plugin.json"),
        manifest.to_string(),
    )
    .expect("manifest override");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let base = format!("http://{}", listener.local_addr().expect("addr"));
    thread::spawn(move || {
        if let Ok((_stream, _)) = listener.accept() {
            thread::sleep(std::time::Duration::from_secs(8));
        }
    });

    let error = fixture
        .execute(
            "rest.get",
            json!({"url": format!("{base}/slow"), "timeout_seconds": 60, "max_attempts": 1}),
            Some("timeout-1"),
            &open_policy(),
        )
        .expect_err("plugin exceeds manifest timeout");
    assert_eq!(code(&error), "DOWAI_PLUGIN_TIMEOUT");
}
