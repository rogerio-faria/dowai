//! ENT-013 (E2E): o binário `dowai config show --effective` nunca imprime segredo literal,
//! nem em stdout nem em stderr, e a saída bem-sucedida valida contra o schema.

use dowai_core::create_project;
use serde_json::{json, Value};
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

const SENTINEL: &str = "SENTINEL-CLI-SEGREDO-b41e0d77-NAO-PODE-APARECER";

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
        let root = std::env::temp_dir().join(format!(
            "dowai-config-show-cli-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(root.join("config").join("dowai")).expect("config dir");
        let project = create_project(&root, "cli").expect("project");
        Self { root, project }
    }

    fn user_config(&self, value: &Value) {
        fs::write(
            self.root.join("config").join("dowai").join("config.json"),
            serde_json::to_vec_pretty(value).expect("json"),
        )
        .expect("write user config");
    }

    fn show(&self) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_dowai"))
            .args(["config", "show", "--effective", "--json", "--project-dir"])
            .arg(&self.project)
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .output()
            .expect("run dowai config show")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn cli_refuses_a_literal_secret_and_never_prints_it() {
    let fixture = Fixture::new();
    fixture.user_config(&json!({
        "schema_version": 2,
        "ai": { "profiles": { "warehouse": { "api_key": SENTINEL } } }
    }));

    let output = fixture.show();

    assert!(
        !output.status.success(),
        "literal secret must fail the command"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stdout.contains(SENTINEL), "stdout leaked the secret");
    assert!(!stderr.contains(SENTINEL), "stderr leaked the secret");
    let error: Value = serde_json::from_str(&stdout).expect("error is reported as JSON on stdout");
    assert_eq!(error["error_code"], "DOWAI_CONFIG_INVALID");
    assert!(
        error["message"]
            .as_str()
            .is_some_and(|message| message.contains("ai.profiles.warehouse.api_key")),
        "message must name the key: {error}"
    );
}

#[test]
fn cli_output_with_a_variable_reference_is_valid_json_against_the_schema() {
    let fixture = Fixture::new();
    fixture.user_config(&json!({
        "schema_version": 2,
        "ai": { "profiles": { "warehouse": { "api_key_env": "DOWAI_AI_KEY" } } }
    }));

    let output = fixture.show();

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let document: Value = serde_json::from_slice(&output.stdout).expect("stdout is JSON");
    let schema: Value = serde_json::from_str(
        &fs::read_to_string(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../specs/config.schema.json"),
        )
        .expect("schema"),
    )
    .expect("schema json");
    let validator = jsonschema::validator_for(&schema).expect("schema compiles");
    let errors: Vec<String> = validator
        .iter_errors(&document)
        .map(|e| e.to_string())
        .collect();
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(document["effective"]["runtime"]["max_threads"], 8);
    assert_eq!(document["origins"]["runtime.max_threads"], "engine_default");
}
