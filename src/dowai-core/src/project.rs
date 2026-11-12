use crate::{plan, DowaiError, ExecutionMode, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::fs::OpenOptions;
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};

const PROJECT_FILE: &str = "dowai_project.json";

/// Locais do sistema onde o workspace nunca é criado (ENT-017). Lista conservadora:
/// `init` roda sem root e não deve gravar em áreas administrativas.
const PROTECTED_WORKSPACE_ROOTS: &[&str] = &[
    "/bin", "/boot", "/dev", "/etc", "/lib", "/lib32", "/lib64", "/proc", "/root", "/run", "/sbin",
    "/sys", "/usr", "/var",
];

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

/// Severidade de um diagnóstico de validação (ENT-018).
///
/// Avisos não invalidam o projeto no modo normal; `validate --strict` os converte em erros.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DiagnosticSeverity {
    Error,
    Warning,
}

#[derive(Clone, Debug, Serialize)]
pub struct ValidationDiagnostic {
    pub code: String,
    pub message: String,
    pub severity: DiagnosticSeverity,
    /// Caminho relativo ao projeto, quando o diagnóstico tem localização conhecida.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

impl ValidationDiagnostic {
    fn error(code: &str, message: String, path: Option<String>) -> Self {
        Self {
            code: code.to_owned(),
            message,
            severity: DiagnosticSeverity::Error,
            path,
        }
    }

    fn warning(code: &str, message: String, path: Option<String>) -> Self {
        Self {
            code: code.to_owned(),
            message,
            severity: DiagnosticSeverity::Warning,
            path,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct ValidationReport {
    pub schema_version: u32,
    pub valid: bool,
    pub project: Option<Project>,
    pub diagnostics: Vec<ValidationDiagnostic>,
}

impl ValidationReport {
    /// Modo `--strict` (ENT-018): cada aviso passa a ser erro e o relatório falha se houver qualquer diagnóstico.
    pub fn apply_strict(&mut self) {
        for diagnostic in &mut self.diagnostics {
            diagnostic.severity = DiagnosticSeverity::Error;
        }
        self.finalize();
    }

    /// Ordena os diagnósticos de forma determinística e recalcula `valid`.
    fn finalize(&mut self) {
        self.diagnostics.sort_by(|a, b| {
            (a.severity, &a.path, &a.code, &a.message)
                .cmp(&(b.severity, &b.path, &b.code, &b.message))
        });
        self.valid = !self
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.severity == DiagnosticSeverity::Error);
    }
}

pub fn load_project(project_root: &Path) -> Result<Project> {
    let path = project_root.join(PROJECT_FILE);
    crate::check_configuration_formats(&path)?;
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

/// Valida o caminho do workspace antes de qualquer escrita (ENT-017).
///
/// Recusa `..` literal, caminhos cujo ancestral existente resolve (com symlinks) para um
/// diretório protegido do sistema ou para a raiz do sistema de arquivos. Não exige root.
pub fn validate_workspace_root(workspace: &Path) -> Result<()> {
    if workspace
        .components()
        .any(|c| matches!(c, Component::ParentDir))
    {
        return Err(DowaiError::Path(
            "Workspace path must not contain '..' components".to_owned(),
        ));
    }
    let absolute = if workspace.is_absolute() {
        workspace.to_path_buf()
    } else {
        std::env::current_dir()?.join(workspace)
    };
    let mut existing = absolute.as_path();
    while !existing.exists() {
        existing = existing.parent().ok_or_else(|| {
            DowaiError::Path("Workspace path has no existing ancestor".to_owned())
        })?;
    }
    if !existing.is_dir() {
        return Err(DowaiError::Path(format!(
            "Workspace ancestor is not a directory: {}",
            existing.display()
        )));
    }
    let resolved = fs::canonicalize(existing)?;
    if resolved.parent().is_none() {
        return Err(DowaiError::Path(
            "Workspace must not be the filesystem root".to_owned(),
        ));
    }
    for protected in PROTECTED_WORKSPACE_ROOTS {
        if resolved.starts_with(protected) {
            return Err(DowaiError::Path(format!(
                "Workspace outside authorized root: {} is a protected system location",
                resolved.display()
            )));
        }
    }
    Ok(())
}

pub fn create_project(workspace: &Path, name: &str) -> Result<PathBuf> {
    validate_project_name(name)?;
    validate_workspace_root(workspace)?;
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
            diagnostics.push(ValidationDiagnostic::error(
                error.code(),
                error.message(),
                Some(PROJECT_FILE.to_owned()),
            ));
            None
        }
    };

    if let Some(config) = &project {
        let models_path = config.models_dir.display().to_string();
        let models = project_root.join(&config.models_dir);
        if !models.is_dir() {
            diagnostics.push(ValidationDiagnostic::error(
                "DOWAI_MODELS_DIR_MISSING",
                format!("Models directory does not exist: {}", models.display()),
                Some(models_path),
            ));
        } else {
            match plan(project_root, &["all".to_owned()], ExecutionMode::Automatic) {
                Ok(execution_plan) => {
                    if execution_plan.nodes.is_empty() {
                        diagnostics.push(ValidationDiagnostic::warning(
                            "DOWAI_MODELS_EMPTY",
                            "Models directory contains no .sql or .py models".to_owned(),
                            Some(models_path.clone()),
                        ));
                    }
                    for warning in execution_plan.warnings {
                        diagnostics.push(ValidationDiagnostic::warning(
                            "DOWAI_PLAN_WARNING",
                            warning,
                            Some(models_path.clone()),
                        ));
                    }
                }
                Err(error) => diagnostics.push(ValidationDiagnostic::error(
                    error.code(),
                    error.message(),
                    Some(models_path),
                )),
            }
        }
        if let Err(error) = crate::resolve_effective_configuration(
            project_root,
            user_config_path,
            admin_config_path,
        ) {
            diagnostics.push(ValidationDiagnostic::error(
                error.code(),
                error.message(),
                None,
            ));
        }
    }

    let mut report = ValidationReport {
        schema_version: 2,
        valid: true,
        project,
        diagnostics,
    };
    report.finalize();
    report
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
