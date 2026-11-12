//! ENT-013: `config show --effective` reporta a origem de cada campo e nunca expõe segredo
//! literal. Um literal é recusado em qualquer camada, e a mensagem nomeia a chave, não o valor.

use dowai_core::{create_project, resolve_effective_configuration};
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const SENTINEL: &str = "SENTINEL-SEGREDO-7f3a9c2e-NAO-PODE-APARECER";

struct Scratch(PathBuf);

impl Scratch {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("dowai-show-{label}-{}-{nonce}", std::process::id()));
        fs::create_dir_all(&root).expect("scratch dir");
        Self(root)
    }

    fn write(&self, relative: &str, value: &Value) -> PathBuf {
        let path = self.0.join(relative);
        fs::create_dir_all(path.parent().expect("parent")).expect("parent dir");
        fs::write(&path, serde_json::to_vec_pretty(value).expect("json")).expect("write");
        path
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn secret_layer() -> Value {
    json!({
        "schema_version": 2,
        "ai": { "profiles": { "warehouse": { "api_key": SENTINEL } } }
    })
}

fn schema_errors(output: &Value) -> Vec<String> {
    let text = fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../specs/config.schema.json"),
    )
    .expect("schema");
    let schema: Value = serde_json::from_str(&text).expect("schema json");
    let validator = jsonschema::validator_for(&schema).expect("schema compiles");
    validator
        .iter_errors(output)
        .map(|error| error.to_string())
        .collect()
}

#[test]
fn literal_secret_is_refused_in_every_layer_without_echoing_its_value() {
    let scratch = Scratch::new("refuse");
    let workspace = scratch.path().join("ws");
    fs::create_dir_all(&workspace).expect("workspace");
    let project = create_project(&workspace, "secreto").expect("project");

    let user = scratch.write("user/config.json", &secret_layer());
    let admin = scratch.write("admin/config.json", &secret_layer());
    let mut manifest: Value =
        serde_json::from_slice(&fs::read(project.join("dowai_project.json")).expect("manifest"))
            .expect("manifest json");
    manifest["config"] = secret_layer();
    fs::write(
        project.join("dowai_project.json"),
        serde_json::to_vec_pretty(&manifest).expect("json"),
    )
    .expect("write manifest");

    let cases: Vec<(&str, Result<Value, dowai_core::DowaiError>)> = vec![
        (
            "user",
            resolve_effective_configuration(&project, Some(&user), None),
        ),
        (
            "admin",
            resolve_effective_configuration(&project, None, Some(&admin)),
        ),
        (
            "project",
            resolve_effective_configuration(&project, None, None),
        ),
    ];
    for (layer, result) in cases {
        let error = result.expect_err(&format!("{layer} layer must refuse a literal secret"));
        assert_eq!(error.code(), "DOWAI_CONFIG_INVALID", "{layer}");
        assert!(
            error.message().contains("ai.profiles.warehouse.api_key"),
            "{layer} must name the key: {}",
            error.message()
        );
        assert!(
            !error.message().contains(SENTINEL),
            "{layer} echoed the value"
        );
        assert!(
            !format!("{error:?}").contains(SENTINEL),
            "{layer} debug echoed the value"
        );
    }
}

#[test]
fn environment_references_are_accepted_and_only_the_variable_name_is_reported() {
    let scratch = Scratch::new("reference");
    let project = scratch.path().join("project");
    fs::create_dir_all(&project).expect("project");
    let user = scratch.write(
        "user/config.json",
        &json!({
            "schema_version": 2,
            "ai": { "profiles": { "warehouse": { "api_key_env": "DOWAI_AI_KEY" } } }
        }),
    );

    let output = resolve_effective_configuration(&project, Some(&user), None)
        .expect("variable references are allowed");

    let rendered = output.to_string();
    assert!(rendered.contains("DOWAI_AI_KEY"));
    assert!(!rendered.contains(SENTINEL));
    assert_eq!(
        output["origins"]["ai.profiles.warehouse.api_key_env"], "user",
        "the reference must report its user origin"
    );
    assert!(
        schema_errors(&output).is_empty(),
        "{:?}",
        schema_errors(&output)
    );
}

#[test]
fn origin_is_reported_for_each_precedence_layer() {
    let scratch = Scratch::new("origins");
    let workspace = scratch.path().join("ws");
    fs::create_dir_all(&workspace).expect("workspace");
    let project = create_project(&workspace, "origens").expect("project");

    scratch.write(
        "ws/workspace.json",
        &json!({ "schema_version": 2, "runtime": { "queue_capacity": 50 } }),
    );
    let admin = scratch.write(
        "admin/config.json",
        &json!({ "schema_version": 2, "runtime": { "max_worker_processes": 3 } }),
    );
    let user = scratch.write(
        "user/config.json",
        &json!({ "schema_version": 2, "runtime": { "max_threads": 6 } }),
    );
    let mut manifest: Value =
        serde_json::from_slice(&fs::read(project.join("dowai_project.json")).expect("manifest"))
            .expect("json");
    manifest["config"] = json!({ "runtime": { "shutdown_grace_seconds": 25 } });
    fs::write(
        project.join("dowai_project.json"),
        serde_json::to_vec_pretty(&manifest).expect("json"),
    )
    .expect("write manifest");

    let output = resolve_effective_configuration(&project, Some(&user), Some(&admin))
        .expect("resolve layers");

    assert_eq!(output["origins"]["runtime.max_worker_processes"], "admin");
    assert_eq!(output["origins"]["runtime.max_threads"], "user");
    assert_eq!(output["origins"]["runtime.queue_capacity"], "workspace");
    assert_eq!(
        output["origins"]["runtime.shutdown_grace_seconds"],
        "project"
    );
    assert_eq!(output["origins"]["runtime.max_memory_mb"], "engine_default");
    assert_eq!(output["sources"]["admin"], "loaded");
    assert_eq!(output["sources"]["user"], "loaded");
    assert_eq!(output["sources"]["workspace"], "loaded");
    assert_eq!(output["sources"]["project"], "loaded");
    assert!(
        schema_errors(&output).is_empty(),
        "{:?}",
        schema_errors(&output)
    );
}
