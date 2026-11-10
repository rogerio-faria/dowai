//! AC032: JSON and legacy YAML for the same scope. Conflicts are diagnosed and never merged;
//! YAML alone is refused with a pointer to `dowai config migrate`; migration never overwrites,
//! never echoes secrets and always writes a file that loads as native JSON.

use dowai_core::{create_project, migrate_legacy_yaml, resolve_effective_configuration};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock is after Unix epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "dowai-legacy-{name}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&root).expect("create scratch directory");
        Self(root)
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

#[test]
fn json_and_yaml_manifest_in_one_scope_is_a_conflict_not_a_merge() {
    let scratch = Scratch::new("conflict");
    let project = create_project(scratch.path(), "conflict").expect("create project");
    fs::write(
        project.join("dowai_project.yaml"),
        "schema_version: 2\nconfig:\n  execution:\n    max_call_depth: 3\n",
    )
    .expect("write legacy manifest");

    let error = resolve_effective_configuration(&project, None, None)
        .expect_err("two formats for one scope must not be merged");

    assert_eq!(error.code(), "DOWAI_CONFIG_CONFLICT");
}

#[test]
fn yaml_manifest_alone_is_refused_with_a_migration_pointer() {
    let scratch = Scratch::new("legacy-only");
    let project = create_project(scratch.path(), "legacy").expect("create project");
    fs::remove_file(project.join("dowai_project.json")).expect("remove native manifest");
    fs::write(
        project.join("dowai_project.yaml"),
        "schema_version: 2\nconfig: {}\n",
    )
    .expect("write legacy manifest");

    let error = resolve_effective_configuration(&project, None, None)
        .expect_err("legacy YAML is not read as a layer");

    assert_eq!(error.code(), "DOWAI_CONFIG_LEGACY_YAML");
}

#[test]
fn user_yaml_alone_is_refused_and_user_json_plus_yaml_conflicts() {
    let scratch = Scratch::new("user-scope");
    let project = create_project(scratch.path(), "user").expect("create project");
    let user_json = scratch.path().join("user").join("config.json");
    fs::create_dir_all(user_json.parent().expect("parent")).expect("create user dir");
    fs::write(user_json.with_extension("yaml"), "schema_version: 2\n").expect("write yaml");

    let refused = resolve_effective_configuration(&project, Some(&user_json), None)
        .expect_err("user YAML alone is refused");
    assert_eq!(refused.code(), "DOWAI_CONFIG_LEGACY_YAML");

    fs::write(&user_json, "{\"schema_version\": 2}").expect("write native user config");
    let conflict = resolve_effective_configuration(&project, Some(&user_json), None)
        .expect_err("user JSON plus YAML conflicts");
    assert_eq!(conflict.code(), "DOWAI_CONFIG_CONFLICT");
}

#[test]
fn migration_writes_native_json_and_leaves_the_input_untouched() {
    let scratch = Scratch::new("migrate-ok");
    let input = scratch.path().join("config.yaml");
    let output = scratch.path().join("config.json");
    let original = "execution:\n  max_call_depth: 4\nsessions:\n  conflict_policy: wait_process\n";
    fs::write(&input, original).expect("write legacy config");

    let report = migrate_legacy_yaml(&input, &output).expect("migration succeeds");

    assert_eq!(report.changes, ["added schema_version: 2".to_owned()]);
    assert_eq!(fs::read_to_string(&input).expect("input"), original);
    let written: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&output).expect("output")).expect("valid JSON");
    assert_eq!(written["schema_version"], serde_json::json!(2));
    assert_eq!(written["execution"]["max_call_depth"], serde_json::json!(4));
    assert_eq!(
        written["sessions"]["conflict_policy"],
        serde_json::json!("wait_process")
    );
}

#[test]
fn migration_never_replaces_an_existing_output() {
    let scratch = Scratch::new("migrate-exists");
    let input = scratch.path().join("config.yaml");
    let output = scratch.path().join("config.json");
    fs::write(&input, "execution: {}\n").expect("write legacy config");
    fs::write(&output, "{\"keep\": true}").expect("write existing output");

    let error = migrate_legacy_yaml(&input, &output).expect_err("must not overwrite");

    assert_eq!(error.code(), "DOWAI_CONFIG_INVALID");
    assert_eq!(
        fs::read_to_string(&output).expect("existing output"),
        "{\"keep\": true}"
    );
}

#[test]
fn migration_refuses_literal_secrets_without_echoing_them_or_writing_output() {
    let scratch = Scratch::new("migrate-secret");
    let input = scratch.path().join("config.yaml");
    let output = scratch.path().join("config.json");
    fs::write(
        &input,
        "profiles:\n  warehouse:\n    host: db.internal\n    password: hunter2-literal\n",
    )
    .expect("write legacy config");

    let error = migrate_legacy_yaml(&input, &output).expect_err("literal secret refused");

    assert_eq!(error.code(), "DOWAI_CONFIG_INVALID");
    assert!(error.message().contains("profiles.warehouse.password"));
    assert!(!error.message().contains("hunter2-literal"));
    assert!(!output.exists());
}

#[test]
fn migration_accepts_environment_references_for_credentials() {
    let scratch = Scratch::new("migrate-env-ref");
    let input = scratch.path().join("config.yaml");
    let output = scratch.path().join("config.json");
    fs::write(
        &input,
        "profiles:\n  warehouse:\n    password_env: WAREHOUSE_PASSWORD\n",
    )
    .expect("write legacy config");

    migrate_legacy_yaml(&input, &output).expect("env reference is allowed");

    let written = fs::read_to_string(&output).expect("output");
    assert!(written.contains("WAREHOUSE_PASSWORD"));
}

#[test]
fn migration_parse_errors_do_not_echo_the_input() {
    let scratch = Scratch::new("migrate-parse");
    let input = scratch.path().join("config.yaml");
    let output = scratch.path().join("config.json");
    fs::write(&input, "token: [unclosed-marker-xyz\n").expect("write malformed config");

    let error = migrate_legacy_yaml(&input, &output).expect_err("malformed YAML refused");

    assert!(!error.message().contains("unclosed-marker-xyz"));
    assert!(!output.exists());
}

#[test]
fn migration_rejects_wrong_extensions_and_missing_inputs() {
    let scratch = Scratch::new("migrate-args");
    let input = scratch.path().join("config.yaml");
    fs::write(&input, "execution: {}\n").expect("write legacy config");

    let wrong_output = migrate_legacy_yaml(&input, &scratch.path().join("config.txt"))
        .expect_err("output must be .json");
    assert_eq!(wrong_output.code(), "DOWAI_CONFIG_INVALID");

    let wrong_input = migrate_legacy_yaml(
        &scratch.path().join("config.json"),
        &scratch.path().join("out.json"),
    )
    .expect_err("input must be YAML");
    assert_eq!(wrong_input.code(), "DOWAI_CONFIG_INVALID");

    let missing = migrate_legacy_yaml(
        &scratch.path().join("absent.yaml"),
        &scratch.path().join("out.json"),
    )
    .expect_err("missing input");
    assert_eq!(missing.code(), "DOWAI_CONFIG_INVALID");
}
