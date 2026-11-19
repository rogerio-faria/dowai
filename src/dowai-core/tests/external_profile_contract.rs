//! ENT-128: `external` workspace profiles are validated when loaded. The driver path stays inside
//! `drivers/`, `params` carry no literal secrets, and `env` lists valid variable names.

use dowai_core::load_workspace_profile;
use serde_json::{json, Value};
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

fn workspace_with(profile: Value) -> (PathBuf, PathBuf) {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock is after Unix epoch")
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "dowai-external-profile-{}-{nonce}",
        std::process::id()
    ));
    let project = root.join("project");
    fs::create_dir_all(&project).expect("create project");
    let document = json!({
        "schema_version": 2,
        "profiles": { "external_dw": profile },
    });
    fs::write(
        root.join("profiles.json"),
        serde_json::to_vec(&document).expect("serialize"),
    )
    .expect("write workspace");
    (root, project)
}

fn external(driver: &str, params: Value, env: Value) -> Value {
    json!({ "type": "external", "driver": driver, "params": params, "env": env })
}

#[test]
fn a_valid_external_profile_loads_without_sql_fields() {
    let (root, project) = workspace_with(external(
        "sqlserver/driver.py",
        json!({ "schema": "dw", "port": 1433 }),
        json!(["SQLSERVER_TOKEN"]),
    ));

    let profile = load_workspace_profile(&project, "external_dw").expect("valid external profile");

    assert_eq!(profile.profile_type, "external");
    assert_eq!(profile.driver.as_deref(), Some("sqlserver/driver.py"));
    assert_eq!(profile.env, vec!["SQLSERVER_TOKEN".to_owned()]);
    assert!(profile.host.is_empty());
    let _ = fs::remove_dir_all(root);
}

#[test]
fn driver_paths_must_stay_inside_drivers() {
    for driver in [
        "../escape.py",
        "a/../../escape.py",
        "/abs/escape.py",
        "C:/escape.py",
        "pasta\\escape.py",
        "driver.sql",
        "",
        "pasta//driver.py",
    ] {
        let (root, project) = workspace_with(external(driver, json!({}), json!([])));
        assert!(
            load_workspace_profile(&project, "external_dw").is_err(),
            "driver {driver:?} must be refused"
        );
        let _ = fs::remove_dir_all(root);
    }
}

#[test]
fn literal_secrets_in_params_are_refused() {
    let (root, project) = workspace_with(external(
        "driver.py",
        json!({ "password": "abc123" }),
        json!([]),
    ));

    let error = load_workspace_profile(&project, "external_dw")
        .expect_err("a literal password must be refused");

    assert_eq!(error.code(), "DOWAI_CONFIG_INVALID");
    assert!(
        !error.message().contains("abc123"),
        "the value is never reported"
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn env_names_must_be_valid_identifiers() {
    let (root, project) = workspace_with(external("driver.py", json!({}), json!(["BAD-NAME"])));

    assert!(load_workspace_profile(&project, "external_dw").is_err());
    let _ = fs::remove_dir_all(root);
}

#[test]
fn unknown_profile_types_are_still_refused() {
    let (root, project) = workspace_with(json!({ "type": "oracle", "driver": "x.py" }));

    assert!(load_workspace_profile(&project, "external_dw").is_err());
    let _ = fs::remove_dir_all(root);
}
