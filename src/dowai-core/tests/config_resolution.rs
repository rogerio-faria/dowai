use dowai_core::{
    load_workspace_profile, resolve_effective_configuration, DowaiError, ProfileTlsMode,
};
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

struct TestConfigTree(PathBuf);

impl TestConfigTree {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock is after Unix epoch")
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("dowai-config-test-{}-{nonce}", std::process::id()));
        fs::create_dir_all(&root).expect("create test directory");
        Self(root)
    }

    fn write_json(&self, path: &str, value: &Value) -> PathBuf {
        let file_path = self.0.join(path);
        fs::create_dir_all(file_path.parent().expect("file has a parent"))
            .expect("create file parent");
        fs::write(
            &file_path,
            serde_json::to_vec_pretty(value).expect("serialize fixture"),
        )
        .expect("write fixture");
        file_path
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestConfigTree {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn resolve(
    project_dir: &Path,
    user_config: Option<&Path>,
    admin_config: Option<&Path>,
) -> Result<Value, DowaiError> {
    resolve_effective_configuration(project_dir, user_config, admin_config)
}

#[test]
fn effective_configuration_applies_project_over_workspace_user_admin_and_defaults() {
    let tree = TestConfigTree::new();
    let project = tree.path().join("workspace").join("projects").join("sales");
    fs::create_dir_all(&project).expect("create project");
    tree.write_json(
        "workspace/workspace.json",
        &json!({
            "schema_version": 2,
            "execution": { "default_mode": "manual" },
            "ai": { "enabled": true }
        }),
    );
    tree.write_json(
        "workspace/projects/sales/dowai_project.json",
        &json!({
            "schema_version": 2,
            "name": "sales",
            "config": {
                "execution": {
                    "default_mode": "automatic",
                    "default_profile": "warehouse"
                }
            }
        }),
    );
    let user_config = tree.write_json(
        "user/config.json",
        &json!({
            "schema_version": 2,
            "execution": { "stop_on_error": false },
            "templates": { "max_nesting_depth": 10 }
        }),
    );
    let admin_config = tree.write_json(
        "admin/config.json",
        &json!({
            "schema_version": 2,
            "execution": { "max_call_depth": 8 },
            "security": { "allow_external_writes": false }
        }),
    );

    let result = resolve(&project, Some(&user_config), Some(&admin_config))
        .expect("resolve layered configuration");

    assert_eq!(
        result["effective"]["execution"]["default_mode"],
        "automatic"
    );
    assert_eq!(
        result["effective"]["execution"]["default_profile"],
        "warehouse"
    );
    assert_eq!(result["effective"]["execution"]["stop_on_error"], false);
    assert_eq!(result["effective"]["execution"]["max_call_depth"], 8);
    assert_eq!(result["effective"]["templates"]["max_nesting_depth"], 10);
    assert_eq!(result["effective"]["ai"]["enabled"], true);
    assert_eq!(result["sources"]["project"], "loaded");
    assert_eq!(result["sources"]["workspace"], "loaded");
    assert_eq!(result["sources"]["user"], "loaded");
    assert_eq!(result["sources"]["admin"], "loaded");
    assert_eq!(result["origins"]["execution.default_mode"], "project");
    assert_eq!(result["origins"]["execution.default_profile"], "project");
    assert_eq!(result["origins"]["execution.stop_on_error"], "user");
    assert_eq!(result["origins"]["execution.max_call_depth"], "admin");
}

#[test]
fn administrator_policy_cannot_be_weakened_by_project_or_user_configuration() {
    let tree = TestConfigTree::new();
    let project = tree.path().join("project");
    fs::create_dir_all(&project).expect("create project");
    tree.write_json(
        "project/dowai_project.json",
        &json!({
            "schema_version": 2,
            "name": "sales",
            "config": {
                "security": {
                    "allow_external_writes": true,
                    "require_approval_for_destructive": false
                },
                "execution": {
                    "allow_manual_mode": true,
                    "max_call_depth": 64
                }
            }
        }),
    );
    let admin = tree.write_json(
        "admin.json",
        &json!({
            "schema_version": 2,
            "security": {
                "allow_external_writes": false,
                "require_approval_for_destructive": true
            },
            "execution": {
                "allow_manual_mode": false,
                "max_call_depth": 8
            }
        }),
    );

    let result = resolve(&project, None, Some(&admin)).expect("resolve configuration");

    assert_eq!(
        result["effective"]["security"]["allow_external_writes"],
        false
    );
    assert_eq!(
        result["effective"]["security"]["require_approval_for_destructive"],
        true
    );
    assert_eq!(result["effective"]["execution"]["allow_manual_mode"], false);
    assert_eq!(result["effective"]["execution"]["max_call_depth"], 8);
    assert_eq!(
        result["origins"]["security.allow_external_writes"],
        "admin_ceiling"
    );
}

#[test]
fn configuration_arrays_replace_instead_of_merging_by_index() {
    let tree = TestConfigTree::new();
    let project = tree.path().join("project");
    fs::create_dir_all(&project).expect("create project");
    let user = tree.write_json(
        "user.json",
        &json!({
            "schema_version": 2,
            "security": { "allowed_profiles": ["source_a", "source_b"] }
        }),
    );
    tree.write_json(
        "project/dowai_project.json",
        &json!({
            "schema_version": 2,
            "name": "sales",
            "config": { "security": { "allowed_profiles": ["source_b"] } }
        }),
    );

    let result = resolve(&project, Some(&user), None).expect("resolve configuration");

    assert_eq!(
        result["effective"]["security"]["allowed_profiles"],
        json!(["source_b"])
    );
    assert_eq!(result["origins"]["security.allowed_profiles"], "project");
}

#[test]
fn workspace_defaults_are_loaded_and_direct_workspace_values_override_them() {
    let tree = TestConfigTree::new();
    let project = tree.path().join("workspace").join("projects").join("sales");
    fs::create_dir_all(&project).expect("create project");
    tree.write_json(
        "workspace/workspace.json",
        &json!({
            "schema_version": 2,
            "workspace_name": "analytics",
            "defaults": {
                "execution": {
                    "default_mode": "manual",
                    "stop_on_error": false
                }
            },
            "execution": { "stop_on_error": true }
        }),
    );

    let result = resolve(&project, None, None).expect("resolve workspace defaults");

    assert_eq!(result["effective"]["execution"]["default_mode"], "manual");
    assert_eq!(result["effective"]["execution"]["stop_on_error"], true);
    assert_eq!(result["origins"]["execution.default_mode"], "workspace");
    assert_eq!(result["origins"]["execution.stop_on_error"], "workspace");
}

#[test]
fn invalid_configuration_is_rejected_with_its_source_path() {
    let tree = TestConfigTree::new();
    let project = tree.path().join("project");
    fs::create_dir_all(&project).expect("create project");
    let invalid = tree.write_json(
        "user.json",
        &json!({
            "schema_version": 2,
            "execution": { "default_mode": "sometimes" }
        }),
    );

    let error = resolve(&project, Some(&invalid), None).expect_err("reject invalid config");

    assert_eq!(error.code(), "DOWAI_CONFIG_INVALID");
    assert!(error.message().contains("user.json"));
}

#[test]
fn unknown_configuration_keys_are_rejected_instead_of_silently_ignored() {
    let tree = TestConfigTree::new();
    let project = tree.path().join("project");
    fs::create_dir_all(&project).expect("create project");
    let invalid = tree.write_json(
        "user.json",
        &json!({
            "schema_version": 2,
            "execution": { "surprise": true }
        }),
    );

    let error = resolve(&project, Some(&invalid), None).expect_err("reject unknown key");

    assert_eq!(error.code(), "DOWAI_CONFIG_INVALID");
}

#[test]
fn absent_layers_report_engine_defaults_as_the_source() {
    let tree = TestConfigTree::new();
    let project = tree.path().join("empty");
    fs::create_dir_all(&project).expect("create project directory");

    let result = resolve(&project, None, None).expect("resolve defaults");

    assert_eq!(
        result["effective"]["execution"]["default_mode"],
        "automatic"
    );
    assert_eq!(
        result["origins"]["execution.default_mode"],
        "engine_default"
    );
    assert_eq!(result["sources"]["workspace"], "not_loaded");
    assert_eq!(result["sources"]["project"], "not_loaded");
}

#[test]
fn workspace_profiles_use_environment_references_and_default_to_tls() {
    let tree = TestConfigTree::new();
    let project = tree.path().join("workspace").join("projects").join("sales");
    fs::create_dir_all(&project).expect("create project");
    tree.write_json(
        "workspace/workspace.json",
        &json!({
            "schema_version": 2,
            "profiles": {
                "warehouse": {
                    "type": "postgres",
                    "host": "db.example.test",
                    "port": 5432,
                    "user": "dowai",
                    "database": "analytics",
                    "password_env": "DOWAI_PG_PASSWORD"
                }
            }
        }),
    );

    let profile =
        load_workspace_profile(&project, "warehouse").expect("load the named workspace profile");

    assert_eq!(profile.profile_type, "postgres");
    assert_eq!(profile.password_env, "DOWAI_PG_PASSWORD");
    assert_eq!(profile.tls, ProfileTlsMode::Required);
}

#[test]
fn workspace_profiles_reject_literal_passwords_and_unknown_fields() {
    let tree = TestConfigTree::new();
    let project = tree.path().join("workspace").join("projects").join("sales");
    fs::create_dir_all(&project).expect("create project");
    tree.write_json(
        "workspace/workspace.json",
        &json!({
            "schema_version": 2,
            "profiles": {
                "warehouse": {
                    "type": "postgres",
                    "host": "localhost",
                    "port": 5432,
                    "user": "dowai",
                    "database": "analytics",
                    "password_env": "DOWAI_PG_PASSWORD",
                    "password": "do-not-accept"
                }
            }
        }),
    );

    let error = load_workspace_profile(&project, "warehouse")
        .expect_err("literal secrets and unknown properties must be rejected");

    assert_eq!(error.code(), "DOWAI_CONFIG_INVALID");
    assert!(!error.message().contains("do-not-accept"));
}

#[test]
fn conflict_policy_defaults_to_error_and_rejects_any_other_value() {
    let tree = TestConfigTree::new();
    let project = tree.path().join("project");
    fs::create_dir_all(&project).expect("create project");
    tree.write_json(
        "project/dowai_project.json",
        &json!({ "schema_version": 2, "name": "p", "config": {} }),
    );
    let result = resolve(&project, None, None).expect("defaults");
    assert_eq!(result["effective"]["sessions"]["conflict_policy"], "error");

    for rejected in [json!("last_writer_wins"), json!("merge"), json!(true)] {
        tree.write_json(
            "project/dowai_project.json",
            &json!({
                "schema_version": 2,
                "name": "p",
                "config": { "sessions": { "conflict_policy": rejected } }
            }),
        );
        let error = resolve(&project, None, None).expect_err("policy must be rejected");
        assert_eq!(error.code(), "DOWAI_CONFIG_INVALID");
    }
}
