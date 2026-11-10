use crate::{plan, DowaiError, ExecutionMode, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::fs::OpenOptions;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

const PROJECT_FILE: &str = "dowai_project.json";

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Project {
    pub schema_version: u32,
    pub name: String,
    #[serde(default = "default_models_dir")]
    pub models_dir: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<Value>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub sources: BTreeMap<String, ProjectSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub python: Option<ProjectPython>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectPython {
    pub interpreter: PathBuf,
    #[serde(default = "default_python_timeout_seconds")]
    pub timeout_seconds: u64,
    #[serde(default)]
    pub dependencies: Vec<String>,
    #[serde(default = "default_python_package_policy")]
    pub package_policy: String,
    #[serde(default)]
    pub allow_unlisted_packages: bool,
}

fn default_python_timeout_seconds() -> u64 {
    300
}

fn default_python_package_policy() -> String {
    "declared_and_authorized".to_owned()
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectSource {
    pub schema: String,
    pub tables: BTreeMap<String, String>,
}

fn default_models_dir() -> PathBuf {
    PathBuf::from("models")
}

#[derive(Clone, Debug, Serialize)]
pub struct ValidationDiagnostic {
    pub code: String,
    pub message: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct ValidationReport {
    pub schema_version: u32,
    pub valid: bool,
    pub project: Option<Project>,
    pub diagnostics: Vec<ValidationDiagnostic>,
}

pub fn load_project(project_root: &Path) -> Result<Project> {
    let path = project_root.join(PROJECT_FILE);
    let bytes = fs::read(&path)
        .map_err(|error| DowaiError::Config(format!("Cannot read {}: {error}", path.display())))?;
    let project: Project = serde_json::from_slice(&bytes)
        .map_err(|error| DowaiError::Config(format!("Invalid {}: {error}", path.display())))?;

    if project.schema_version != 2 {
        return Err(DowaiError::Config(format!(
            "Unsupported project schema_version {}; expected 2",
            project.schema_version
        )));
    }
    if project.name.trim().is_empty() {
        return Err(DowaiError::Config(
            "Project name must not be empty".to_owned(),
        ));
    }
    if project.models_dir.as_os_str().is_empty()
        || project.models_dir.is_absolute()
        || project
            .models_dir
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err(DowaiError::Config(
            "models_dir must be a non-empty relative path without '..' components".to_owned(),
        ));
    }
    if project
        .config
        .as_ref()
        .is_some_and(|config| !config.is_object())
    {
        return Err(DowaiError::Config(
            "Project config must be a JSON object".to_owned(),
        ));
    }
    if let Some(python) = &project.python {
        if !python.interpreter.is_absolute()
            || python
                .interpreter
                .components()
                .any(|component| matches!(component, std::path::Component::ParentDir))
        {
            return Err(DowaiError::Config(
                "python.interpreter must be an absolute path to the project interpreter".to_owned(),
            ));
        }
        if !(1..=86_400).contains(&python.timeout_seconds) {
            return Err(DowaiError::Config(
                "python.timeout_seconds must be between 1 and 86400".to_owned(),
            ));
        }
        if !matches!(
            python.package_policy.as_str(),
            "declared_and_authorized" | "declared_only" | "admin_allowlist"
        ) {
            return Err(DowaiError::Config(
                "python.package_policy is not supported".to_owned(),
            ));
        }
        if python.dependencies.iter().any(|dependency| {
            dependency.trim().is_empty() || dependency.chars().any(char::is_control)
        }) {
            return Err(DowaiError::Config(
                "python.dependencies must contain non-empty package declarations".to_owned(),
            ));
        }
    }
    validate_project_sources(&project.sources)?;

    Ok(project)
}

fn validate_project_sources(sources: &BTreeMap<String, ProjectSource>) -> Result<()> {
    for (source_name, source) in sources {
        validate_source_name(source_name, "source")?;
        validate_relation_name(&source.schema, "source schema")?;
        for (table_name, relation_name) in &source.tables {
            validate_source_name(table_name, "source table name")?;
            validate_relation_name(relation_name, "source table relation")?;
        }
    }
    Ok(())
}

fn validate_source_name(name: &str, label: &str) -> Result<()> {
    if name.trim().is_empty() || name.len() > 63 || name.chars().any(char::is_control) {
        return Err(DowaiError::Config(format!("Invalid project {label}")));
    }
    Ok(())
}

fn validate_relation_name(name: &str, label: &str) -> Result<()> {
    if name.is_empty() || name.len() > 63 || name.chars().any(char::is_control) {
        return Err(DowaiError::Config(format!("Invalid {label} identifier")));
    }
    Ok(())
}

pub fn create_project(workspace: &Path, name: &str) -> Result<PathBuf> {
    validate_project_name(name)?;
    let root = workspace.join("projects").join(name);
    if root.exists() {
        return Err(DowaiError::Config(format!(
            "Project already exists: {}",
            root.display()
        )));
    }

    fs::create_dir_all(root.parent().expect("project path has workspace parent"))?;
    fs::create_dir(&root).map_err(|error| {
        DowaiError::Config(format!("Cannot create project {}: {error}", root.display()))
    })?;
    fs::create_dir_all(root.join("models"))?;
    fs::create_dir_all(root.join("tests"))?;
    fs::create_dir_all(root.join(".dowai"))?;
    let project = Project {
        schema_version: 2,
        name: name.to_owned(),
        models_dir: default_models_dir(),
        config: None,
        sources: BTreeMap::new(),
        python: None,
    };
    let project_json = serde_json::to_vec_pretty(&project)
        .map_err(|error| DowaiError::Config(error.to_string()))?;
    fs::write(root.join(PROJECT_FILE), project_json)?;
    create_workspace_config(workspace)?;
    Ok(root)
}

pub fn validate_project(project_root: &Path) -> ValidationReport {
    validate_project_with_configuration(project_root, None, None)
}

pub fn validate_project_with_configuration(
    project_root: &Path,
    user_config_path: Option<&Path>,
    admin_config_path: Option<&Path>,
) -> ValidationReport {
    let mut diagnostics = Vec::new();
    let project = match load_project(project_root) {
        Ok(project) => Some(project),
        Err(error) => {
            diagnostics.push(ValidationDiagnostic {
                code: error.code().to_owned(),
                message: error.message(),
            });
            None
        }
    };

    if let Some(config) = &project {
        let models = project_root.join(&config.models_dir);
        if !models.is_dir() {
            diagnostics.push(ValidationDiagnostic {
                code: "DOWAI_MODELS_DIR_MISSING".to_owned(),
                message: format!("Models directory does not exist: {}", models.display()),
            });
        } else if let Err(error) = plan(project_root, &["all".to_owned()], ExecutionMode::Automatic)
        {
            diagnostics.push(ValidationDiagnostic {
                code: error.code().to_owned(),
                message: error.message(),
            });
        }
        if let Err(error) = crate::resolve_effective_configuration(
            project_root,
            user_config_path,
            admin_config_path,
        ) {
            diagnostics.push(ValidationDiagnostic {
                code: error.code().to_owned(),
                message: error.message(),
            });
        }
    }

    ValidationReport {
        schema_version: 2,
        valid: diagnostics.is_empty(),
        project,
        diagnostics,
    }
}

fn validate_project_name(name: &str) -> Result<()> {
    if name.trim().is_empty()
        || name.trim() != name
        || name == "."
        || name == ".."
        || name.chars().any(|character| {
            character.is_control()
                || matches!(
                    character,
                    '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*'
                )
        })
        || name.ends_with('.')
        || name.ends_with(' ')
        || is_reserved_windows_name(name)
    {
        return Err(DowaiError::Config(
            "Project name must be a single safe path component".to_owned(),
        ));
    }
    Ok(())
}

fn is_reserved_windows_name(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name);
    let uppercase = stem.to_ascii_uppercase();
    matches!(uppercase.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (uppercase.len() == 4
            && (uppercase.starts_with("COM") || uppercase.starts_with("LPT"))
            && uppercase.as_bytes()[3].is_ascii_digit()
            && uppercase.as_bytes()[3] != b'0')
}

fn create_workspace_config(workspace: &Path) -> Result<()> {
    let path = workspace.join("workspace.json");
    let workspace_name = workspace
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("workspace");
    let contents = serde_json::to_vec_pretty(&serde_json::json!({
        "schema_version": 2,
        "workspace_name": workspace_name,
        "profiles": {},
        "defaults": {}
    }))
    .map_err(|error| DowaiError::Config(error.to_string()))?;
    match OpenOptions::new().write(true).create_new(true).open(&path) {
        Ok(mut file) => {
            use std::io::Write;
            file.write_all(&contents).map_err(|error| {
                DowaiError::Config(format!("Cannot initialize {}: {error}", path.display()))
            })?;
        }
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
        Err(error) => {
            return Err(DowaiError::Config(format!(
                "Cannot initialize {}: {error}",
                path.display()
            )))
        }
    }
    Ok(())
}
