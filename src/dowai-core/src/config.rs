use crate::{DowaiError, Result};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProfileTlsMode {
    #[default]
    Required,
    Disabled,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceProfile {
    #[serde(rename = "type")]
    pub profile_type: String,
    // SQL profiles (`postgres`, `mysql`) require these; an `external` profile leaves them empty.
    #[serde(default)]
    pub host: String,
    #[serde(default)]
    pub port: u16,
    #[serde(default)]
    pub user: String,
    #[serde(default)]
    pub database: String,
    #[serde(default)]
    pub password_env: String,
    #[serde(default)]
    pub tls: ProfileTlsMode,
    /// `external` profiles (ENT-128): driver file under `drivers/` (relative, `.py`).
    #[serde(default)]
    pub driver: Option<String>,
    /// Non-secret parameters passed to the driver. Literal secrets are refused.
    #[serde(default)]
    pub params: Option<Value>,
    /// Environment variables the driver receives. Only their names are configured.
    #[serde(default)]
    pub env: Vec<String>,
}

pub fn load_workspace_profile(project_dir: &Path, profile_name: &str) -> Result<WorkspaceProfile> {
    if profile_name.trim().is_empty()
        || profile_name
            .chars()
            .any(|character| character.is_control() || character == '/' || character == '\\')
    {
        return Err(DowaiError::Config(
            "Connection profile name is invalid".to_owned(),
        ));
    }
    let path = find_ancestor_file(project_dir, "workspace.json")
        .ok_or_else(|| DowaiError::Config("No workspace.json found for this project".to_owned()))?;
    let document = read_workspace_document(&path)?;
    validate_workspace_profiles(&path, &document)?;
    let profiles = document
        .get("profiles")
        .and_then(Value::as_object)
        .ok_or_else(|| config_error(&path, "Workspace 'profiles' must be an object"))?;
    let value = profiles.get(profile_name).cloned().ok_or_else(|| {
        config_error(
            &path,
            &format!("Unknown connection profile '{profile_name}'"),
        )
    })?;
    parse_workspace_profile(&path, profile_name, &value)
}

fn parse_workspace_profile(
    path: &Path,
    profile_name: &str,
    value: &Value,
) -> Result<WorkspaceProfile> {
    let profile: WorkspaceProfile = serde_json::from_value(value.clone()).map_err(|error| {
        config_error(path, &format!("Invalid profile '{profile_name}': {error}"))
    })?;
    match profile.profile_type.as_str() {
        "postgres" | "mysql" => {}
        "external" => {
            validate_external_profile(path, profile_name, &profile)?;
            return Ok(profile);
        }
        _ => {
            return Err(config_error(
                path,
                &format!(
                    "Unsupported connection profile type '{}' for '{profile_name}'",
                    profile.profile_type
                ),
            ));
        }
    }
    if profile.host.trim().is_empty()
        || profile
            .host
            .chars()
            .any(|character| character.is_control() || character.is_whitespace())
        || profile.user.trim().is_empty()
        || profile.user.chars().any(char::is_control)
        || profile.database.trim().is_empty()
        || profile.database.chars().any(char::is_control)
        || profile.port == 0
        || !valid_environment_variable_name(&profile.password_env)
    {
        return Err(config_error(
            path,
            &format!("Profile '{profile_name}' has invalid connection settings"),
        ));
    }
    Ok(profile)
}

/// Validates an `external` profile (ENT-128). The driver is a Python file under `drivers/` of the
/// project, given as a relative path without `..`, backslashes, drive prefixes or control
/// characters. `params` must be an object without literal secrets, and `env` lists variable names.
fn validate_external_profile(
    path: &Path,
    profile_name: &str,
    profile: &WorkspaceProfile,
) -> Result<()> {
    let invalid = |detail: &str| {
        config_error(
            path,
            &format!("External profile '{profile_name}' is invalid: {detail}"),
        )
    };
    let driver = profile
        .driver
        .as_deref()
        .ok_or_else(|| invalid("driver is required"))?;
    let segments_are_valid = driver
        .split('/')
        .all(|segment| !segment.is_empty() && segment != "..");
    if driver.is_empty()
        || driver.starts_with('/')
        || driver.contains(['\\', ':'])
        || driver.chars().any(char::is_control)
        || !segments_are_valid
        || !driver.ends_with(".py")
    {
        return Err(invalid(
            "driver must be a relative .py path inside drivers/ without '..'",
        ));
    }
    if let Some(params) = &profile.params {
        if !params.is_object() {
            return Err(invalid("params must be an object"));
        }
        reject_secret_literals(params, "")
            .map_err(|_| invalid("params contain a literal secret; use a *_env reference"))?;
    }
    if profile
        .env
        .iter()
        .any(|name| !valid_environment_variable_name(name))
    {
        return Err(invalid("env must list valid environment variable names"));
    }
    Ok(())
}

fn read_workspace_document(path: &Path) -> Result<Value> {
    let bytes = fs::read(path)
        .map_err(|error| config_error(path, &format!("Cannot read workspace JSON: {error}")))?;
    let document: Value = serde_json::from_slice(&bytes)
        .map_err(|error| config_error(path, &format!("Invalid workspace JSON: {error}")))?;
    if document.get("schema_version").and_then(Value::as_u64) != Some(2) {
        return Err(config_error(path, "Workspace schema_version must be 2"));
    }
    Ok(document)
}

fn validate_workspace_profiles(path: &Path, document: &Value) -> Result<()> {
    let Some(profiles) = document.get("profiles") else {
        return Ok(());
    };
    let profiles = profiles
        .as_object()
        .ok_or_else(|| config_error(path, "Workspace 'profiles' must be an object"))?;
    for (name, value) in profiles {
        if name.trim().is_empty()
            || name
                .chars()
                .any(|character| character.is_control() || character == '/' || character == '\\')
        {
            return Err(config_error(
                path,
                "Workspace contains an invalid profile name",
            ));
        }
        parse_workspace_profile(path, name, value)?;
    }
    Ok(())
}

pub fn resolve_effective_configuration(
    project_dir: &Path,
    user_config_path: Option<&Path>,
    admin_config_path: Option<&Path>,
) -> Result<Value> {
    let defaults = configuration_defaults();
    let mut effective = defaults.clone();
    let mut origins = BTreeMap::new();
    collect_origins(&defaults, "", "engine_default", &mut origins);

    let mut sources = BTreeMap::from([
        ("engine_defaults", "active"),
        ("admin", "not_loaded"),
        ("user", "not_loaded"),
        ("workspace", "not_loaded"),
        ("project", "not_loaded"),
    ]);
    let mut admin_values = Map::new();

    if let Some(path) = admin_config_path {
        if let Some(layer) = read_config_layer(path, "administrator")? {
            validate_layer(&layer, path)?;
            sources.insert("admin", "loaded");
            admin_values = layer.clone();
            merge_layer(&mut effective, &layer, "admin", "", &mut origins);
        }
    }

    if let Some(path) = user_config_path {
        if let Some(layer) = read_config_layer(path, "user")? {
            validate_layer(&layer, path)?;
            sources.insert("user", "loaded");
            merge_layer(&mut effective, &layer, "user", "", &mut origins);
        }
    }

    if let Some(legacy_workspace) = find_ancestor_file(project_dir, "workspace.yaml") {
        check_configuration_formats(&legacy_workspace.with_extension("json"))?;
    }
    if let Some(workspace_path) = find_ancestor_file(project_dir, "workspace.json") {
        if let Some(layer) = read_config_layer(&workspace_path, "workspace")? {
            validate_layer(&layer, &workspace_path)?;
            sources.insert("workspace", "loaded");
            merge_layer(&mut effective, &layer, "workspace", "", &mut origins);
        }
    }

    let project_manifest = project_dir.join("dowai_project.json");
    check_configuration_formats(&project_manifest)?;
    if project_manifest.is_file() {
        crate::load_project(project_dir)?;
        let manifest = read_json_object(&project_manifest, "project")?;
        let project_layer = manifest
            .get("config")
            .map(|config| {
                config.as_object().cloned().ok_or_else(|| {
                    config_error(
                        &project_manifest,
                        "The project 'config' property must be an object",
                    )
                })
            })
            .transpose()?
            .unwrap_or_default();
        validate_layer(&project_layer, &project_manifest)?;
        sources.insert("project", "loaded");
        merge_layer(&mut effective, &project_layer, "project", "", &mut origins);
    }

    apply_admin_ceilings(&mut effective, &admin_values, &mut origins);

    Ok(json!({
        "schema_version": 2,
        "effective": effective,
        "origins": origins,
        "sources": sources
    }))
}

pub fn user_config_path() -> Option<PathBuf> {
    if cfg!(windows) {
        std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .map(|path| path.join("DOWAI").join("config.json"))
    } else {
        let root = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")));
        root.map(|path| path.join("dowai").join("config.json"))
    }
}

pub fn admin_config_path() -> Option<PathBuf> {
    if cfg!(windows) {
        std::env::var_os("PROGRAMDATA")
            .map(PathBuf::from)
            .map(|path| path.join("DOWAI").join("config.json"))
    } else {
        Some(PathBuf::from("/etc/dowai/config.json"))
    }
}

fn configuration_defaults() -> Value {
    crate::config_catalog::defaults()
}

/// AC032: one configuration format per scope. The JSON file is the only format read; a YAML
/// sibling is legacy. Both present is a conflict and is never merged; YAML alone is refused
/// with a pointer to `dowai config migrate`.
pub fn check_configuration_formats(json_path: &Path) -> Result<()> {
    let yaml_path = json_path.with_extension("yaml");
    match (json_path.is_file(), yaml_path.is_file()) {
        (true, true) => Err(DowaiError::ConfigConflict(format!(
            "{} and {} both exist. Keep one format per scope: configurations are never merged. Migrate the YAML with `dowai config migrate` and remove it.",
            json_path.display(),
            yaml_path.display()
        ))),
        (false, true) => Err(DowaiError::LegacyConfigFormat(format!(
            "{} is a legacy YAML configuration and is not read. Run `dowai config migrate {} --output {}`.",
            yaml_path.display(),
            yaml_path.display(),
            json_path.display()
        ))),
        _ => Ok(()),
    }
}

/// Outcome of `dowai config migrate`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationReport {
    pub output: PathBuf,
    pub changes: Vec<String>,
}

/// Converts a legacy YAML configuration into the native JSON format (plan 3.6). The input is
/// never modified, an existing output is never replaced, literal secrets are refused without
/// echoing them, and the written file must load as a native configuration.
pub fn migrate_legacy_yaml(input: &Path, output: &Path) -> Result<MigrationReport> {
    let input_is_yaml = matches!(
        input.extension().and_then(|extension| extension.to_str()),
        Some("yaml" | "yml")
    );
    if !input_is_yaml {
        return Err(DowaiError::Config(
            "The input must be a YAML configuration (.yaml or .yml)".to_owned(),
        ));
    }
    if output.extension().and_then(|extension| extension.to_str()) != Some("json") {
        return Err(DowaiError::Config(
            "The output must be a JSON file (.json)".to_owned(),
        ));
    }
    if !input.is_file() {
        return Err(DowaiError::Config(format!(
            "Input configuration not found: {}",
            input.display()
        )));
    }
    if output.exists() {
        return Err(DowaiError::Config(format!(
            "Output already exists and will not be replaced: {}",
            output.display()
        )));
    }
    let text = fs::read_to_string(input).map_err(|error| {
        DowaiError::Config(format!("Cannot read legacy configuration: {error}"))
    })?;
    // The parser's own message can quote input text, so it is not forwarded.
    let document: Value = noyalib::from_str(&text)
        .map_err(|_| DowaiError::Config("Invalid legacy YAML configuration".to_owned()))?;
    let mut object = document.as_object().cloned().ok_or_else(|| {
        DowaiError::Config("Legacy configuration root must be a mapping".to_owned())
    })?;
    let mut changes = Vec::new();
    match object.get("schema_version") {
        None => {
            object.insert("schema_version".to_owned(), json!(2));
            changes.push("added schema_version: 2".to_owned());
        }
        Some(Value::Number(number)) if number.as_u64() == Some(2) => {}
        Some(_) => {
            return Err(DowaiError::Config(
                "Legacy configuration schema_version is not supported; expected 2".to_owned(),
            ))
        }
    }
    reject_secret_literals(&Value::Object(object.clone()), "")?;
    let mut rendered = serde_json::to_string_pretty(&Value::Object(object))
        .map_err(|error| DowaiError::Config(error.to_string()))?;
    rendered.push('\n');

    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output)?;
    let written = file
        .write_all(rendered.as_bytes())
        .and_then(|()| file.sync_all());
    drop(file);
    let verified = written
        .map_err(DowaiError::from)
        .and_then(|()| read_json_object(output, "migrated").map(|_| ()));
    if let Err(error) = verified {
        let _ = fs::remove_file(output);
        return Err(error);
    }
    Ok(MigrationReport {
        output: output.to_path_buf(),
        changes,
    })
}

/// Refuses literal secrets: a credential-like key with a non-empty string value that is not
/// a `*_env` or `*_ref` reference. The value is never included in the error.
fn reject_secret_literals(value: &Value, path: &str) -> Result<()> {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                let child_path = if path.is_empty() {
                    key.clone()
                } else {
                    format!("{path}.{key}")
                };
                let lower = key.to_ascii_lowercase();
                let credential_like = ["password", "secret", "token", "api_key", "apikey"]
                    .iter()
                    .any(|marker| lower.contains(marker));
                let reference = lower.ends_with("_env") || lower.ends_with("_ref");
                if credential_like
                    && !reference
                    && matches!(child, Value::String(text) if !text.is_empty())
                {
                    return Err(DowaiError::Config(format!(
                        "Literal secret at '{child_path}' is refused; use a '*_env' reference or a secret manager"
                    )));
                }
                reject_secret_literals(child, &child_path)?;
            }
            Ok(())
        }
        Value::Array(items) => {
            for item in items {
                reject_secret_literals(item, path)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn read_config_layer(path: &Path, label: &str) -> Result<Option<Map<String, Value>>> {
    check_configuration_formats(path)?;
    if !path.exists() {
        return Ok(None);
    }
    if label == "workspace" {
        let document = read_workspace_document(path)?;
        validate_workspace_profiles(path, &document)?;
    }
    let mut layer = read_json_object(path, label)?;
    if label == "workspace" {
        if let Some(defaults) = layer.remove("defaults") {
            let defaults = defaults.as_object().ok_or_else(|| {
                config_error(path, "Workspace 'defaults' property must be an object")
            })?;
            for (key, default_value) in defaults {
                let merged = match layer.get(key) {
                    Some(direct_value) => merge_values(default_value, direct_value),
                    None => default_value.clone(),
                };
                layer.insert(key.clone(), merged);
            }
        }
    }
    Ok(Some(layer))
}

fn merge_values(base: &Value, override_value: &Value) -> Value {
    match (base.as_object(), override_value.as_object()) {
        (Some(base_object), Some(override_object)) => {
            let mut merged = base_object.clone();
            for (key, value) in override_object {
                let value = merged
                    .get(key)
                    .map(|base_value| merge_values(base_value, value))
                    .unwrap_or_else(|| value.clone());
                merged.insert(key.clone(), value);
            }
            Value::Object(merged)
        }
        _ => override_value.clone(),
    }
}

fn read_json_object(path: &Path, label: &str) -> Result<Map<String, Value>> {
    let bytes = fs::read(path).map_err(|error| {
        config_error(path, &format!("Cannot read {label} configuration: {error}"))
    })?;
    let value: Value = serde_json::from_slice(&bytes).map_err(|error| {
        config_error(
            path,
            &format!("Invalid {label} configuration JSON: {error}"),
        )
    })?;
    let object = value
        .as_object()
        .ok_or_else(|| config_error(path, "Configuration root must be a JSON object"))?;
    if object.get("schema_version").and_then(Value::as_u64) != Some(2) {
        return Err(config_error(path, "Configuration schema_version must be 2"));
    }
    let mut layer = object.clone();
    layer.remove("schema_version");
    if label == "workspace" {
        layer.remove("workspace_name");
        layer.remove("profiles");
    }
    Ok(layer)
}

fn validate_layer(layer: &Map<String, Value>, path: &Path) -> Result<()> {
    // ENT-013: segredo literal não entra em nenhuma camada; a mensagem nomeia a chave, nunca o valor.
    reject_secret_literals(&Value::Object(layer.clone()), "").map_err(|error| match error {
        DowaiError::Config(message) => config_error(path, &message),
        other => other,
    })?;
    for (key, value) in layer {
        if key == "schema_version" {
            continue;
        }
        if !crate::config_catalog::sections().contains(&key.as_str()) {
            return Err(config_error(
                path,
                &format!("Unknown configuration section '{key}'"),
            ));
        }
        if value.is_null() {
            continue;
        }
        let section = value.as_object().ok_or_else(|| {
            config_error(
                path,
                &format!("Configuration section '{key}' must be an object"),
            )
        })?;
        for (setting, value) in section {
            validate_setting(key, setting, value, path)?;
        }
    }
    Ok(())
}

fn validate_setting(section: &str, setting: &str, value: &Value, path: &Path) -> Result<()> {
    if value.is_null() {
        return Ok(());
    }
    let Some(parameter) = crate::config_catalog::find(section, setting) else {
        return Err(config_error(
            path,
            &format!("Unknown configuration setting '{section}.{setting}'"),
        ));
    };
    parameter.validate(value).map_err(|expectation| {
        config_error(
            path,
            &format!(
                "Invalid value for configuration setting '{section}.{setting}': expected {expectation}"
            ),
        )
    })
}

fn merge_layer(
    target: &mut Value,
    layer: &Map<String, Value>,
    source: &str,
    parent: &str,
    origins: &mut BTreeMap<String, String>,
) {
    for (key, value) in layer {
        if key == "schema_version" || value.is_null() {
            continue;
        }
        let path = join_path(parent, key);
        let target_value = &mut target[key];
        if let (Some(target_object), Some(layer_object)) =
            (target_value.as_object_mut(), value.as_object())
        {
            let mut nested_target = Value::Object(target_object.clone());
            merge_layer(&mut nested_target, layer_object, source, &path, origins);
            *target_value = nested_target;
        } else {
            *target_value = value.clone();
            collect_origins(value, &path, source, origins);
        }
    }
}

fn collect_origins(
    value: &Value,
    parent: &str,
    source: &str,
    origins: &mut BTreeMap<String, String>,
) {
    if let Some(object) = value.as_object() {
        for (key, value) in object {
            collect_origins(value, &join_path(parent, key), source, origins);
        }
    } else if !parent.is_empty() {
        origins.insert(parent.to_owned(), source.to_owned());
    }
}

fn apply_admin_ceilings(
    effective: &mut Value,
    admin: &Map<String, Value>,
    origins: &mut BTreeMap<String, String>,
) {
    use crate::config_catalog::{Ceiling, PARAMETERS};

    for parameter in PARAMETERS {
        let Some(ceiling) = admin
            .get(parameter.section)
            .and_then(Value::as_object)
            .and_then(|section| section.get(parameter.name))
            .filter(|value| !value.is_null())
        else {
            continue;
        };
        let Some(current) = effective
            .get(parameter.section)
            .and_then(|section| section.get(parameter.name))
            .cloned()
        else {
            continue;
        };
        let restricted = match parameter.ceiling {
            Ceiling::None => None,
            Ceiling::Maximum => match (current.as_f64(), ceiling.as_f64()) {
                (Some(value), Some(maximum)) if value > maximum => Some(ceiling.clone()),
                _ => None,
            },
            Ceiling::DenyIfAdminFalse => (ceiling.as_bool() == Some(false)
                && current.as_bool() == Some(true))
            .then_some(Value::Bool(false)),
            Ceiling::RequireIfAdminTrue => (ceiling.as_bool() == Some(true)
                && current.as_bool() == Some(false))
            .then_some(Value::Bool(true)),
            Ceiling::Allowlist => match (current.as_array(), ceiling.as_array()) {
                (Some(values), Some(allowed)) => {
                    let intersection = values
                        .iter()
                        .filter(|value| allowed.contains(value))
                        .cloned()
                        .collect::<Vec<_>>();
                    (&intersection != values).then_some(Value::Array(intersection))
                }
                _ => None,
            },
        };
        if let Some(value) = restricted {
            if let Some(section) = effective
                .get_mut(parameter.section)
                .and_then(Value::as_object_mut)
            {
                section.insert(parameter.name.to_owned(), value);
                origins.insert(parameter.key(), "admin_ceiling".to_owned());
            }
        }
    }
}

fn find_ancestor_file(start: &Path, filename: &str) -> Option<PathBuf> {
    start
        .ancestors()
        .map(|ancestor| ancestor.join(filename))
        .find(|candidate| candidate.is_file())
}

fn join_path(parent: &str, child: &str) -> String {
    if parent.is_empty() {
        child.to_owned()
    } else {
        format!("{parent}.{child}")
    }
}

fn config_error(path: &Path, message: &str) -> DowaiError {
    DowaiError::Config(format!("{}: {message}", path.display()))
}

fn valid_environment_variable_name(name: &str) -> bool {
    let mut characters = name.chars();
    matches!(characters.next(), Some(character) if character == '_' || character.is_ascii_alphabetic())
        && characters.all(|character| character == '_' || character.is_ascii_alphanumeric())
}
