//! ENT-012: o catálogo de parâmetros é a única fonte de tipos, defaults e tetos. Schema,
//! documentação e saída efetiva são conferidos contra ele, e todo parâmetro recusa valor fora
//! do tipo ou do limite com código `DOWAI_*`.

use dowai_core::config_catalog::{self, Ceiling, DefaultValue, Kind, Parameter, PARAMETERS};
use dowai_core::{create_project, resolve_effective_configuration};
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

struct Scratch(PathBuf);

impl Scratch {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock is after Unix epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "dowai-catalog-{label}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&root).expect("create scratch directory");
        Self(root)
    }

    fn file(&self, name: &str, value: &Value) -> PathBuf {
        let path = self.0.join(name);
        fs::write(&path, serde_json::to_vec_pretty(value).expect("serialize")).expect("write");
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

fn repo_file(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(relative)
}

fn config_schema() -> Value {
    let text = fs::read_to_string(repo_file("specs/config.schema.json")).expect("read schema");
    serde_json::from_str(&text).expect("schema is JSON")
}

fn engine_output(label: &str) -> (Scratch, Value) {
    let scratch = Scratch::new(label);
    let project = scratch.path().join("empty-project");
    fs::create_dir_all(&project).expect("create project dir");
    let output = resolve_effective_configuration(&project, None, None).expect("resolve defaults");
    (scratch, output)
}

/// Valor que o tipo do parâmetro deve recusar.
fn invalid_sample(parameter: &Parameter) -> Value {
    match parameter.kind {
        Kind::Bool => json!("não é booleano"),
        Kind::EssentialTrue => json!(false),
        Kind::Integer { max, .. } => json!(max + 1),
        Kind::NonNegativeNumber => json!(-1),
        Kind::Enum(_) => json!("valor-inexistente"),
        Kind::NonEmptyText => json!("   "),
        Kind::NullableText => json!(""),
        Kind::TextList => json!("não é lista"),
        Kind::EmptyTextList => json!(["nao-suportado"]),
        Kind::Object => json!("não é objeto"),
    }
}

/// Valores válidos de fronteira (inclusive) para cada tipo.
fn boundary_samples(parameter: &Parameter) -> Vec<Value> {
    match parameter.kind {
        Kind::Bool => vec![json!(true), json!(false)],
        Kind::EssentialTrue => vec![json!(true)],
        Kind::Integer { min, max } => vec![json!(min), json!(max)],
        Kind::NonNegativeNumber => vec![json!(0), json!(2.5)],
        Kind::Enum(allowed) => allowed.iter().map(|value| json!(value)).collect(),
        Kind::NonEmptyText => vec![json!("x")],
        Kind::NullableText => vec![json!(null), json!("perfil")],
        Kind::TextList => vec![json!([]), json!(["a"])],
        Kind::EmptyTextList => vec![json!([])],
        Kind::Object => vec![json!({})],
    }
}

fn user_layer(section: &str, name: &str, value: Value) -> Value {
    json!({ "schema_version": 2, section: { name: value } })
}

#[test]
fn catalog_declares_unique_keys_and_defaults_accepted_by_their_own_kind() {
    let mut keys = std::collections::BTreeSet::new();
    for parameter in PARAMETERS {
        assert!(
            keys.insert(parameter.key()),
            "duplicate parameter {}",
            parameter.key()
        );
        match parameter.default {
            DefaultValue::Null => assert_eq!(
                parameter.kind,
                Kind::NullableText,
                "only nullable text may default to null: {}",
                parameter.key()
            ),
            _ => assert!(
                parameter.validate(&parameter.default.to_json()).is_ok(),
                "default of {} must satisfy its kind",
                parameter.key()
            ),
        }
    }
    assert_eq!(keys.len(), PARAMETERS.len());
}

#[test]
fn engine_output_for_an_empty_project_is_exactly_the_catalog_defaults() {
    let (_scratch, output) = engine_output("defaults");
    let effective = output["effective"].as_object().expect("effective object");

    let sections: Vec<&str> = effective.keys().map(String::as_str).collect();
    let expected_sections = config_catalog::sections();
    assert_eq!(sections.len(), expected_sections.len());
    for section in &expected_sections {
        assert!(
            effective.contains_key(*section),
            "missing section {section}"
        );
    }
    for parameter in PARAMETERS {
        assert_eq!(
            output["effective"][parameter.section][parameter.name],
            parameter.default.to_json(),
            "effective default of {}",
            parameter.key()
        );
    }
}

#[test]
fn engine_output_validates_against_the_config_schema() {
    let schema = config_schema();
    let validator = jsonschema::validator_for(&schema).expect("schema compiles");
    let (_scratch, output) = engine_output("schema");
    let errors: Vec<String> = validator
        .iter_errors(&output)
        .map(|error| error.to_string())
        .collect();
    assert!(errors.is_empty(), "engine output rejected: {errors:?}");
}

#[test]
fn schema_declares_exactly_the_catalog_parameters_with_matching_defaults_and_enums() {
    let schema = config_schema();
    let effective = &schema["properties"]["effective"]["properties"];
    let mut declared = 0;
    for parameter in PARAMETERS {
        let property = &effective[parameter.section]["properties"][parameter.name];
        assert!(property.is_object(), "schema lacks {}", parameter.key());
        declared += 1;
        assert_eq!(
            property["default"],
            parameter.default.to_json(),
            "schema default of {}",
            parameter.key()
        );
        if let Kind::Enum(allowed) = parameter.kind {
            assert_eq!(
                property["enum"],
                json!(allowed),
                "schema enum of {}",
                parameter.key()
            );
        }
    }
    let total: usize = effective
        .as_object()
        .expect("sections")
        .values()
        .map(|section| section["properties"].as_object().map_or(0, |p| p.len()))
        .sum();
    assert_eq!(declared, PARAMETERS.len());
    assert_eq!(total, PARAMETERS.len(), "schema declares extra parameters");
}

#[test]
fn documentation_lists_every_parameter_with_its_default_in_a_table_row() {
    let docs = fs::read_to_string(repo_file("docs/CONFIGURATION.md")).expect("read docs");
    for parameter in PARAMETERS {
        let key = parameter.key();
        let rendered_default = format!("`{}`", parameter.default.to_json());
        let row = docs
            .lines()
            .find(|line| line.starts_with(&format!("| `{key}` |")))
            .unwrap_or_else(|| panic!("docs lack a table row for {key}"));
        assert!(
            row.contains(&rendered_default),
            "docs row for {key} must show default {rendered_default}: {row}"
        );
    }
}

#[test]
fn every_parameter_refuses_an_out_of_contract_value_with_a_config_code() {
    for parameter in PARAMETERS {
        let scratch = Scratch::new("invalid");
        let project = scratch.path().join("project");
        fs::create_dir_all(&project).expect("create project dir");
        let layer = scratch.file(
            "user.json",
            &user_layer(parameter.section, parameter.name, invalid_sample(parameter)),
        );

        let error =
            resolve_effective_configuration(&project, Some(&layer), None).expect_err(&format!(
                "{} must refuse {:?}",
                parameter.key(),
                invalid_sample(parameter)
            ));

        assert_eq!(error.code(), "DOWAI_CONFIG_INVALID", "{}", parameter.key());
        assert!(
            error.message().contains(&parameter.key()),
            "message for {} must name the setting: {}",
            parameter.key(),
            error.message()
        );
    }
}

#[test]
fn every_parameter_accepts_its_inclusive_boundaries_and_enum_values() {
    for parameter in PARAMETERS {
        for sample in boundary_samples(parameter) {
            let scratch = Scratch::new("boundary");
            let project = scratch.path().join("project");
            fs::create_dir_all(&project).expect("create project dir");
            let layer = scratch.file(
                "user.json",
                &user_layer(parameter.section, parameter.name, sample.clone()),
            );

            let output = resolve_effective_configuration(&project, Some(&layer), None)
                .unwrap_or_else(|error| {
                    panic!(
                        "{} must accept {sample}: {}",
                        parameter.key(),
                        error.message()
                    )
                });
            assert_eq!(
                output["effective"][parameter.section][parameter.name],
                sample,
                "{}",
                parameter.key()
            );
        }
    }
}

#[test]
fn essential_audit_cannot_be_disabled_by_user_or_project_configuration() {
    let scratch = Scratch::new("audit");
    let workspace = scratch.path().join("workspace");
    fs::create_dir_all(&workspace).expect("create workspace");
    let project = create_project(&workspace, "audited").expect("create project");

    let user = scratch.file(
        "user.json",
        &json!({ "schema_version": 2, "logging": { "audit_enabled": false } }),
    );
    let error = resolve_effective_configuration(&project, Some(&user), None)
        .expect_err("user layer must not disable AUDIT");
    assert_eq!(error.code(), "DOWAI_CONFIG_INVALID");
    assert!(error.message().contains("logging.audit_enabled"));

    let manifest_path = project.join("dowai_project.json");
    let mut manifest: Value =
        serde_json::from_str(&fs::read_to_string(&manifest_path).expect("read manifest"))
            .expect("manifest JSON");
    manifest["config"] = json!({ "logging": { "audit_enabled": false } });
    fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&manifest).expect("serialize"),
    )
    .expect("write manifest");
    let error = resolve_effective_configuration(&project, None, None)
        .expect_err("project layer must not disable AUDIT");
    assert_eq!(error.code(), "DOWAI_CONFIG_INVALID");
    assert!(error.message().contains("logging.audit_enabled"));
}

#[test]
fn administrator_ceilings_apply_to_catalog_parameters_of_every_ceiling_kind() {
    let scratch = Scratch::new("ceilings");
    let project = scratch.path().join("project");
    fs::create_dir_all(&project).expect("create project dir");
    let admin = scratch.file(
        "admin.json",
        &json!({
            "schema_version": 2,
            "runtime": { "max_worker_processes": 2 },
            "database": { "tls_required": true },
            "plugins": { "allow_network": false, "require_idempotency_key": true },
            "ai": { "enabled": false },
            "security": { "allowed_hosts": ["api.interno"] }
        }),
    );
    let user = scratch.file(
        "user.json",
        &json!({
            "schema_version": 2,
            "runtime": { "max_worker_processes": 8 },
            "database": { "tls_required": false },
            "plugins": { "allow_network": true, "require_idempotency_key": false },
            "ai": { "enabled": true },
            "security": { "allowed_hosts": ["api.interno", "externo.exemplo"] }
        }),
    );

    let output = resolve_effective_configuration(&project, Some(&user), Some(&admin))
        .expect("resolve with administrator ceilings");

    assert_eq!(output["effective"]["runtime"]["max_worker_processes"], 2);
    assert_eq!(
        output["origins"]["runtime.max_worker_processes"],
        "admin_ceiling"
    );
    assert_eq!(output["effective"]["database"]["tls_required"], true);
    assert_eq!(output["effective"]["plugins"]["allow_network"], false);
    assert_eq!(
        output["effective"]["plugins"]["require_idempotency_key"],
        true
    );
    assert_eq!(output["effective"]["ai"]["enabled"], false);
    assert_eq!(
        output["effective"]["security"]["allowed_hosts"],
        json!(["api.interno"])
    );
}

#[test]
fn administrator_layer_cannot_disable_essential_audit_either() {
    let scratch = Scratch::new("admin-audit");
    let project = scratch.path().join("project");
    fs::create_dir_all(&project).expect("create project dir");
    let admin = scratch.file(
        "admin.json",
        &json!({ "schema_version": 2, "logging": { "audit_enabled": false } }),
    );
    let error = resolve_effective_configuration(&project, None, Some(&admin))
        .expect_err("administrator layer must keep AUDIT enabled");
    assert_eq!(error.code(), "DOWAI_CONFIG_INVALID");
}

#[test]
fn ceiling_kinds_in_the_catalog_are_consistent_with_their_types() {
    for parameter in PARAMETERS {
        match parameter.ceiling {
            Ceiling::Maximum => assert!(
                matches!(
                    parameter.kind,
                    Kind::Integer { .. } | Kind::NonNegativeNumber
                ),
                "{} has a numeric maximum but is not numeric",
                parameter.key()
            ),
            Ceiling::DenyIfAdminFalse | Ceiling::RequireIfAdminTrue => assert!(
                matches!(parameter.kind, Kind::Bool),
                "{} has a boolean ceiling but is not boolean",
                parameter.key()
            ),
            Ceiling::Allowlist => assert!(
                matches!(parameter.kind, Kind::TextList),
                "{} has an allowlist ceiling but is not a text list",
                parameter.key()
            ),
            Ceiling::None => {}
        }
    }
}
