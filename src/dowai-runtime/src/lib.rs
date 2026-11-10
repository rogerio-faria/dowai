use dowai_core::{
    admin_config_path, load_project, load_workspace_profile, plan, read_planned_model,
    resolve_effective_configuration, source_relation_key, user_config_path, DowaiError,
    ExecutionMode, ExecutionPlan, Materialization, Result, SessionStore, TemplateOptions,
    WorkspaceProfile,
};
use dowai_postgres::{
    quote_identifier, AdapterError, Materialization as PostgresMaterialization, PgProfile,
    PostgresAdapter, TlsMode,
};
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use uuid::Uuid;

mod executor;
mod python_runtime;

const SESSION_TTL: Duration = Duration::from_secs(86_400);

#[derive(Clone, Debug)]
pub struct RunRequest {
    pub targets: Vec<String>,
    pub mode: Option<ExecutionMode>,
    pub profile: Option<String>,
    pub session_id: Option<String>,
    pub variables: BTreeMap<String, Value>,
    pub confirm_destructive: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct RunNodeResult {
    pub id: String,
    pub relation: String,
    pub materialization: String,
    pub status: String,
}

/// One recorded invocation (static node, dependency, or imperative `env.run`).
/// Never contains variable values.
#[derive(Clone, Debug, Serialize)]
pub struct InvocationRecord {
    pub invocation_id: String,
    pub parent_invocation_id: String,
    pub call_type: String,
    pub target: String,
    pub mode: String,
    pub status: String,
    pub duration_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct RunResult {
    pub schema_version: u32,
    pub session_id: String,
    pub run_id: String,
    pub invocation_id: String,
    pub status: String,
    pub execution_mode: String,
    pub duration_ms: u64,
    pub profile: String,
    pub nodes: Vec<RunNodeResult>,
    pub invocations: Vec<InvocationRecord>,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct PreparedNode {
    pub id: String,
    pub relation: String,
    pub materialization: Materialization,
    pub sql: String,
}

#[derive(Clone, Debug)]
pub struct PreparedPlan {
    pub nodes: Vec<PreparedNode>,
    pub variables: BTreeMap<String, Value>,
}

pub fn run_postgres(project_dir: &Path, request: RunRequest) -> Result<RunResult> {
    let started = Instant::now();
    let root = project_dir.canonicalize().map_err(|error| {
        DowaiError::Path(format!(
            "Project directory is unavailable: {} ({error})",
            project_dir.display()
        ))
    })?;
    let configuration = effective_configuration(&root)?;
    let mode = resolve_execution_mode(&configuration, request.mode)?;
    let execution_plan = plan(&root, &request.targets, mode)?;
    if execution_plan.nodes.is_empty() {
        return Err(DowaiError::Model(
            "The execution plan contains no models".to_owned(),
        ));
    }
    let python_nodes = execution_plan
        .nodes
        .iter()
        .filter(|node| is_python_path(&node.path))
        .count();
    let mixed = python_nodes > 0 && python_nodes < execution_plan.nodes.len();

    enforce_external_write_policy(&configuration)?;
    let python_only = python_nodes == execution_plan.nodes.len();
    if python_only && request.profile.is_some() {
        return Err(DowaiError::Config(
            "--profile is not valid for a Python-only execution plan".to_owned(),
        ));
    }
    let profile = if python_only {
        None
    } else {
        Some(resolve_profile(
            &root,
            &configuration,
            request.profile.as_deref(),
        )?)
    };

    let store = SessionStore::for_user_state(
        default_session_path()
            .ok_or_else(|| DowaiError::State("No user state directory is available".to_owned()))?,
    );
    let prior_session = match request.session_id.as_deref() {
        Some(session_id) => {
            let metadata = store.get(session_id)?;
            if metadata.project_root != root {
                return Err(DowaiError::Session(
                    "Session belongs to a different project".to_owned(),
                ));
            }
            Some(metadata)
        }
        None => None,
    };
    let mut variables = match prior_session.as_ref() {
        Some(session) => store.snapshot(&session.session_id)?,
        None => BTreeMap::new(),
    };
    variables.extend(request.variables);
    store.validate_variables(&variables)?;

    if python_only {
        let (project, python) = python_settings(&root)?;
        let interpreter = &python.interpreter;
        let models_dir = root.join(&project.models_dir);
        let run_invocation_id = Uuid::new_v4().to_string();
        let mut executor = executor::Executor::new(
            &root,
            &models_dir,
            interpreter,
            Duration::from_secs(python.timeout_seconds),
            &configuration,
            &store,
        )?;
        let mut completed_nodes = Vec::with_capacity(execution_plan.nodes.len());
        for node in &execution_plan.nodes {
            variables = executor.run_static(node, mode, variables, &run_invocation_id)?;
            completed_nodes.push(RunNodeResult {
                id: node.id.clone(),
                relation: node.id.clone(),
                materialization: "none".to_owned(),
                status: "SUCCEEDED".to_owned(),
            });
        }
        let invocations = executor.invocations;
        let session = match prior_session {
            Some(session) => session,
            None => store.create(&root, SESSION_TTL)?,
        };
        store.commit_variables(&session.session_id, &variables)?;
        return Ok(RunResult {
            schema_version: 2,
            session_id: session.session_id,
            run_id: Uuid::new_v4().to_string(),
            invocation_id: run_invocation_id,
            status: "SUCCEEDED".to_owned(),
            execution_mode: execution_plan.execution_mode,
            duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            profile: "python".to_owned(),
            nodes: completed_nodes,
            invocations,
            warnings: execution_plan.warnings,
        });
    }

    let (profile_name, profile) = profile.ok_or_else(|| {
        DowaiError::Config("PostgreSQL profile resolution is required for SQL models".to_owned())
    })?;
    let template_options = template_options(&configuration)?;

    if mixed {
        // Mixed SQL + Python run. Nodes execute strictly in plan order; each one sees
        // the variables accumulated by the nodes before it (SQL `def_var`, Python
        // `env.setVar`). SQL is rendered lazily, right before its node runs, so it
        // observes variables set by earlier Python nodes.
        //
        // Limitation: SQL relations already materialized are NOT rolled back when a
        // later node fails. Only session variables are all-or-nothing (published
        // after every node succeeded); this is not a distributed transaction.
        let (project, python) = python_settings(&root)?;
        let models_dir = root.join(&project.models_dir);
        let relations = build_relations(&root, &execution_plan)?;
        let allow_replace = replacement_approval(&configuration, request.confirm_destructive)?;
        let run_invocation_id = Uuid::new_v4().to_string();
        let mut executor = executor::Executor::new(
            &root,
            &models_dir,
            &python.interpreter,
            Duration::from_secs(python.timeout_seconds),
            &configuration,
            &store,
        )?;
        let mut adapter = PostgresAdapter::connect(&postgres_profile(profile))
            .map_err(|error| DowaiError::Connection(error.to_string()))?;
        let mut completed_nodes = Vec::with_capacity(execution_plan.nodes.len());
        for node in &execution_plan.nodes {
            if is_python_path(&node.path) {
                variables = executor.run_static(node, mode, variables, &run_invocation_id)?;
                completed_nodes.push(RunNodeResult {
                    id: node.id.clone(),
                    relation: node.id.clone(),
                    materialization: "none".to_owned(),
                    status: "SUCCEEDED".to_owned(),
                });
                continue;
            }
            let (prepared, next_variables) =
                render_sql_node(&root, node, &variables, &template_options, &relations)?;
            if prepared.sql.trim().is_empty() {
                return Err(DowaiError::Template(format!(
                    "{} renders to an empty SQL statement",
                    prepared.id
                )));
            }
            store.validate_variables(&next_variables)?;
            adapter
                .materialize(
                    &prepared.relation,
                    match prepared.materialization {
                        Materialization::Table => PostgresMaterialization::Table,
                        Materialization::View => PostgresMaterialization::View,
                    },
                    &prepared.sql,
                    allow_replace,
                )
                .map_err(map_execution_error)?;
            variables = next_variables;
            completed_nodes.push(RunNodeResult {
                id: prepared.id,
                relation: prepared.relation,
                materialization: match prepared.materialization {
                    Materialization::Table => "table",
                    Materialization::View => "view",
                }
                .to_owned(),
                status: "SUCCEEDED".to_owned(),
            });
        }
        let session = match prior_session {
            Some(session) => session,
            None => store.create(&root, SESSION_TTL)?,
        };
        store.commit_variables(&session.session_id, &variables)?;
        return Ok(RunResult {
            schema_version: 2,
            session_id: session.session_id,
            run_id: Uuid::new_v4().to_string(),
            invocation_id: run_invocation_id,
            status: "SUCCEEDED".to_owned(),
            execution_mode: execution_plan.execution_mode,
            duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            profile: profile_name,
            nodes: completed_nodes,
            invocations: executor.invocations,
            warnings: execution_plan.warnings,
        });
    }

    let prepared = prepare_postgres_plan(&root, &execution_plan, &variables, &template_options)?;
    if let Some(node) = prepared
        .nodes
        .iter()
        .find(|node| node.sql.trim().is_empty())
    {
        return Err(DowaiError::Template(format!(
            "{} renders to an empty SQL statement",
            node.id
        )));
    }
    store.validate_variables(&prepared.variables)?;
    let mut adapter = PostgresAdapter::connect(&postgres_profile(profile))
        .map_err(|error| DowaiError::Connection(error.to_string()))?;
    let allow_replace = replacement_approval(&configuration, request.confirm_destructive)?;
    let mut completed_nodes = Vec::with_capacity(prepared.nodes.len());
    for node in &prepared.nodes {
        let materialization = match node.materialization {
            Materialization::Table => PostgresMaterialization::Table,
            Materialization::View => PostgresMaterialization::View,
        };
        adapter
            .materialize(&node.relation, materialization, &node.sql, allow_replace)
            .map_err(map_execution_error)?;
        completed_nodes.push(RunNodeResult {
            id: node.id.clone(),
            relation: node.relation.clone(),
            materialization: match node.materialization {
                Materialization::Table => "table",
                Materialization::View => "view",
            }
            .to_owned(),
            status: "SUCCEEDED".to_owned(),
        });
    }

    let session = match prior_session {
        Some(session) => session,
        None => store.create(&root, SESSION_TTL)?,
    };
    store.commit_variables(&session.session_id, &prepared.variables)?;
    Ok(RunResult {
        schema_version: 2,
        session_id: session.session_id,
        run_id: Uuid::new_v4().to_string(),
        invocation_id: Uuid::new_v4().to_string(),
        status: "SUCCEEDED".to_owned(),
        execution_mode: execution_plan.execution_mode,
        duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        profile: profile_name.to_owned(),
        nodes: completed_nodes,
        invocations: Vec::new(),
        warnings: execution_plan.warnings,
    })
}

/// Load and validate the project's supervised Python runtime settings.
fn python_settings(root: &Path) -> Result<(dowai_core::Project, dowai_core::ProjectPython)> {
    let mut project = load_project(root)?;
    let python = project.python.take().ok_or_else(|| {
        DowaiError::Config(
            "Configure python.interpreter with the project's virtual environment".to_owned(),
        )
    })?;
    python_runtime::validate_python_runtime(&python)?;
    if !is_project_path(root, &python.interpreter) {
        return Err(DowaiError::Path(
            "Python interpreter path must be inside the project directory".to_owned(),
        ));
    }
    if !python.interpreter.is_file() {
        return Err(DowaiError::Path(
            "Configured Python interpreter is not a file".to_owned(),
        ));
    }
    Ok((project, python))
}

#[cfg(windows)]
fn is_project_path(project_root: &Path, candidate: &Path) -> bool {
    fn remove_windows_prefix(path: &Path) -> PathBuf {
        let path = path.to_string_lossy();
        PathBuf::from(path.strip_prefix(r"\\?\").unwrap_or(&path))
    }

    remove_windows_prefix(candidate).starts_with(remove_windows_prefix(project_root))
}

#[cfg(not(windows))]
fn is_project_path(project_root: &Path, candidate: &Path) -> bool {
    candidate.starts_with(project_root)
}

fn resolve_python_model_path(
    project_root: &Path,
    models_dir: &Path,
    relative_path: &str,
) -> Result<PathBuf> {
    read_planned_model(project_root, relative_path)?;
    let models_root = project_root
        .join(models_dir)
        .canonicalize()
        .map_err(|_| DowaiError::Path("Project models directory is unavailable".to_owned()))?;
    let model_path = models_root
        .join(relative_path)
        .canonicalize()
        .map_err(|_| DowaiError::Path(format!("Python model is unavailable: {relative_path}")))?;
    if !model_path.starts_with(&models_root) || !model_path.is_file() {
        return Err(DowaiError::Path(format!(
            "Python model path is not inside the models directory: {relative_path}"
        )));
    }
    Ok(model_path)
}

pub fn check_postgres_connection(
    project_dir: &Path,
    requested_profile: Option<&str>,
) -> Result<String> {
    let root = project_dir.canonicalize().map_err(|error| {
        DowaiError::Path(format!(
            "Project directory is unavailable: {} ({error})",
            project_dir.display()
        ))
    })?;
    let configuration = effective_configuration(&root)?;
    let (profile_name, profile) = resolve_profile(&root, &configuration, requested_profile)?;
    let mut adapter = PostgresAdapter::connect(&postgres_profile(profile))
        .map_err(|error| DowaiError::Connection(error.to_string()))?;
    adapter
        .test_connection()
        .map_err(|error| DowaiError::Connection(error.to_string()))?;
    Ok(profile_name)
}

fn build_relations(
    project_dir: &Path,
    execution_plan: &ExecutionPlan,
) -> Result<BTreeMap<String, String>> {
    let mut relation_ids = BTreeSet::new();
    for node in &execution_plan.nodes {
        relation_ids.insert(node.id.clone());
        relation_ids.extend(node.depends_on.iter().cloned());
    }
    let mut ids_by_stem: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for id in &relation_ids {
        let stem = model_stem(id)?;
        ids_by_stem.entry(stem).or_default().push(id.clone());
    }
    if let Some((stem, _)) = ids_by_stem.iter().find(|(_, ids)| ids.len() > 1) {
        return Err(DowaiError::Model(format!(
            "Multiple model paths map to PostgreSQL relation '{stem}'"
        )));
    }
    let mut written_relations = BTreeSet::new();
    let mut relations = BTreeMap::new();
    for (source_name, source) in load_project(project_dir)?.sources {
        let schema = quote_identifier(&source.schema)
            .map_err(|error| DowaiError::Model(error.to_string()))?;
        for (table_name, relation_name) in source.tables {
            let table = quote_identifier(&relation_name)
                .map_err(|error| DowaiError::Model(error.to_string()))?;
            relations.insert(
                source_relation_key(&source_name, &table_name),
                format!("{schema}.{table}"),
            );
        }
    }
    for id in &relation_ids {
        let stem = model_stem(id)?;
        let quoted =
            quote_identifier(&stem).map_err(|error| DowaiError::Model(error.to_string()))?;
        if execution_plan.nodes.iter().any(|node| &node.id == id)
            && !written_relations.insert(stem.clone())
        {
            return Err(DowaiError::Model(format!(
                "Multiple planned models map to PostgreSQL relation '{stem}'"
            )));
        }
        relations.insert(id.replace('\\', "/"), quoted.clone());
        let path_without_extension = Path::new(id)
            .with_extension("")
            .to_string_lossy()
            .replace('\\', "/");
        relations.insert(path_without_extension, quoted.clone());
        if ids_by_stem.get(&stem).is_some_and(|ids| ids.len() == 1) {
            relations.insert(stem, quoted);
        }
    }
    Ok(relations)
}

fn is_python_path(path: &str) -> bool {
    Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("py"))
}

fn render_sql_node(
    project_dir: &Path,
    node: &dowai_core::PlanNode,
    variables: &BTreeMap<String, Value>,
    template_options: &TemplateOptions,
    relations: &BTreeMap<String, String>,
) -> Result<(PreparedNode, BTreeMap<String, Value>)> {
    let source = read_planned_model(project_dir, &node.path)?;
    let rendered = dowai_core::render_sql_model(&source, variables, template_options, relations)
        .map_err(|error| {
            if error.code() == "DOWAI_NOT_IMPLEMENTED" {
                error
            } else {
                DowaiError::Template(format!("{}: {}", node.path, error.message()))
            }
        })?;
    Ok((
        PreparedNode {
            id: node.id.clone(),
            relation: model_stem(&node.id)?,
            materialization: rendered.materialization,
            sql: rendered.sql,
        },
        rendered.variables,
    ))
}

pub fn prepare_postgres_plan(
    project_dir: &Path,
    execution_plan: &ExecutionPlan,
    variables: &BTreeMap<String, Value>,
    template_options: &TemplateOptions,
) -> Result<PreparedPlan> {
    let relations = build_relations(project_dir, execution_plan)?;
    let mut current_variables = variables.clone();
    let mut prepared_nodes = Vec::with_capacity(execution_plan.nodes.len());
    for node in &execution_plan.nodes {
        if is_python_path(&node.path) {
            continue;
        }
        let (prepared, next_variables) = render_sql_node(
            project_dir,
            node,
            &current_variables,
            template_options,
            &relations,
        )?;
        current_variables = next_variables;
        prepared_nodes.push(prepared);
    }
    Ok(PreparedPlan {
        nodes: prepared_nodes,
        variables: current_variables,
    })
}

fn effective_configuration(project_dir: &Path) -> Result<Value> {
    resolve_effective_configuration(
        project_dir,
        user_config_path().as_deref(),
        admin_config_path().as_deref(),
    )
}

fn resolve_execution_mode(
    configuration: &Value,
    requested_mode: Option<ExecutionMode>,
) -> Result<ExecutionMode> {
    let mode = match requested_mode {
        Some(mode) => mode,
        None => match configuration["effective"]["execution"]["default_mode"].as_str() {
            Some("automatic") => ExecutionMode::Automatic,
            Some("manual") => ExecutionMode::Manual,
            _ => {
                return Err(DowaiError::Config(
                    "Effective execution.default_mode is invalid".to_owned(),
                ))
            }
        },
    };
    if mode == ExecutionMode::Manual
        && configuration["effective"]["execution"]["allow_manual_mode"] == Value::Bool(false)
    {
        return Err(DowaiError::Policy(
            "Manual execution mode is disabled by effective policy".to_owned(),
        ));
    }
    Ok(mode)
}

fn resolve_profile(
    project_dir: &Path,
    configuration: &Value,
    requested_profile: Option<&str>,
) -> Result<(String, WorkspaceProfile)> {
    let profile_name = requested_profile
        .or_else(|| configuration["effective"]["execution"]["default_profile"].as_str())
        .ok_or_else(|| {
            DowaiError::Config(
                "Specify --profile or configure execution.default_profile".to_owned(),
            )
        })?;
    let profile = load_workspace_profile(project_dir, profile_name)?;
    enforce_profile_policy(configuration, profile_name, &profile)?;
    Ok((profile_name.to_owned(), profile))
}

fn model_stem(model_id: &str) -> Result<String> {
    Path::new(model_id)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .filter(|stem| !stem.trim().is_empty())
        .map(str::to_owned)
        .ok_or_else(|| DowaiError::Model(format!("Invalid model relation name: {model_id}")))
}

fn template_options(configuration: &Value) -> Result<TemplateOptions> {
    let templates = &configuration["effective"]["templates"];
    let max_nesting = templates["max_nesting_depth"]
        .as_u64()
        .and_then(|value| usize::try_from(value).ok())
        .ok_or_else(|| {
            DowaiError::Config("Effective templates.max_nesting_depth is invalid".to_owned())
        })?;
    let strict_undefined = templates["strict_undefined"].as_bool().ok_or_else(|| {
        DowaiError::Config("Effective templates.strict_undefined is invalid".to_owned())
    })?;
    Ok(TemplateOptions {
        max_nesting,
        strict_undefined,
    })
}

fn enforce_profile_policy(
    configuration: &Value,
    profile_name: &str,
    profile: &WorkspaceProfile,
) -> Result<()> {
    enforce_allowlist(
        configuration,
        "allowed_profiles",
        profile_name,
        "connection profile",
    )?;
    enforce_allowlist(
        configuration,
        "allowed_hosts",
        &profile.host,
        "PostgreSQL host",
    )
}

fn enforce_external_write_policy(configuration: &Value) -> Result<()> {
    let enabled = configuration["effective"]["security"]["allow_external_writes"]
        .as_bool()
        .ok_or_else(|| {
            DowaiError::Config("Effective allow_external_writes is invalid".to_owned())
        })?;
    if !enabled {
        return Err(DowaiError::Policy(
            "External writes are disabled by effective security policy".to_owned(),
        ));
    }
    Ok(())
}

fn replacement_approval(configuration: &Value, explicitly_confirmed: bool) -> Result<bool> {
    let required = configuration["effective"]["security"]["require_approval_for_destructive"]
        .as_bool()
        .ok_or_else(|| {
            DowaiError::Config("Effective require_approval_for_destructive is invalid".to_owned())
        })?;
    Ok(explicitly_confirmed || !required)
}

fn enforce_allowlist(
    configuration: &Value,
    setting: &str,
    selected: &str,
    label: &str,
) -> Result<()> {
    let allowed = configuration["effective"]["security"][setting]
        .as_array()
        .ok_or_else(|| DowaiError::Config(format!("Effective {setting} is invalid")))?;
    let origin = configuration["origins"][format!("security.{setting}")]
        .as_str()
        .unwrap_or("engine_default");
    let restricted = !allowed.is_empty() || matches!(origin, "admin" | "admin_ceiling");
    if restricted && !allowed.iter().any(|value| value.as_str() == Some(selected)) {
        return Err(DowaiError::Policy(format!(
            "The selected {label} is not allowed by effective policy"
        )));
    }
    Ok(())
}

fn postgres_profile(profile: WorkspaceProfile) -> PgProfile {
    let mut adapter_profile = PgProfile::new(
        profile.host,
        profile.port,
        profile.user,
        profile.database,
        profile.password_env,
    );
    if profile.tls == dowai_core::ProfileTlsMode::Disabled {
        adapter_profile.set_tls_mode(TlsMode::Disabled);
    }
    adapter_profile
}

fn map_execution_error(error: AdapterError) -> DowaiError {
    match error {
        AdapterError::InvalidProfile(_) => {
            DowaiError::Config("Invalid PostgreSQL connection profile".to_owned())
        }
        AdapterError::InvalidIdentifier => {
            DowaiError::Model("Invalid PostgreSQL relation identifier".to_owned())
        }
        AdapterError::DestructiveApprovalRequired => DowaiError::Policy(error.to_string()),
        AdapterError::InsecureTlsDisabled => {
            DowaiError::Policy("TLS may only be disabled for a local PostgreSQL host".to_owned())
        }
        AdapterError::QueryFailed
        | AdapterError::UnsupportedRelationType
        | AdapterError::MultipleStatements => DowaiError::Execution(error.to_string()),
        AdapterError::MissingPassword
        | AdapterError::TlsConfigurationFailed
        | AdapterError::ConnectionFailed => DowaiError::Connection(error.to_string()),
    }
}

fn default_session_path() -> Option<PathBuf> {
    dowai_core::default_session_store_path()
}

#[cfg(test)]
mod tests {
    use super::{prepare_postgres_plan, run_postgres, RunRequest};
    use dowai_core::{
        create_project, plan, ExecutionMode, Materialization, ProfileTlsMode, TemplateOptions,
        WorkspaceProfile,
    };
    use serde_json::json;
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock is after Unix epoch")
                .as_nanos();
            let root = std::env::temp_dir()
                .join(format!("dowai-runtime-test-{}-{nonce}", std::process::id()));
            fs::create_dir_all(&root).expect("create fixture");
            let project = create_project(&root, "fixture").expect("create project");
            Self(project)
        }

        fn write_model(&self, name: &str, body: &str) {
            let path = self.0.join("models").join(name);
            fs::create_dir_all(path.parent().expect("model has a parent"))
                .expect("create model directory");
            fs::write(path, body).expect("write SQL model");
        }

        fn allow_external_writes(&self) {
            let manifest_path = self.0.join("dowai_project.json");
            let mut manifest: serde_json::Value =
                serde_json::from_slice(&fs::read(&manifest_path).expect("read project config"))
                    .expect("project config is JSON");
            manifest["config"]["security"]["allow_external_writes"] = json!(true);
            fs::write(
                manifest_path,
                serde_json::to_vec(&manifest).expect("serialize project config"),
            )
            .expect("write project config");
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            if let Some(workspace) = self.0.parent() {
                let _ = fs::remove_dir_all(workspace);
            }
        }
    }

    #[test]
    fn prepared_plan_quotes_relations_and_resolves_dependencies() {
        let fixture = Fixture::new();
        fixture.write_model("source.sql", "select 1 as id");
        fixture.write_model(
            "daily.sql",
            "{{ config(materialized='view') }} select * from {{ ref('source') }}",
        );
        let execution_plan = plan(
            fixture.path(),
            &["daily.sql".to_owned()],
            ExecutionMode::Automatic,
        )
        .expect("plan models");
        let prepared = prepare_postgres_plan(
            fixture.path(),
            &execution_plan,
            &BTreeMap::new(),
            &TemplateOptions::default(),
        )
        .expect("prepare SQL before database connection");

        assert_eq!(prepared.nodes.len(), 2);
        assert_eq!(prepared.nodes[0].relation, "source");
        assert_eq!(prepared.nodes[1].relation, "daily");
        assert_eq!(prepared.nodes[1].materialization, Materialization::View);
        assert_eq!(prepared.nodes[1].sql.trim(), "select * from \"source\"");
    }

    #[test]
    fn prepared_plan_resolves_declared_sources_without_adding_plan_nodes() {
        let fixture = Fixture::new();
        fixture.write_model(
            "report.sql",
            "select * from {{ source('sales', 'orders') }}",
        );
        fs::write(
            fixture.path().join("dowai_project.json"),
            serde_json::to_vec(&json!({
                "schema_version": 2,
                "name": "fixture",
                "models_dir": "models",
                "sources": {
                    "sales": {
                        "schema": "landing zone",
                        "tables": { "orders": "order facts" }
                    }
                }
            }))
            .expect("serialize project configuration"),
        )
        .expect("write project source declaration");
        let execution_plan = plan(
            fixture.path(),
            &["report.sql".to_owned()],
            ExecutionMode::Automatic,
        )
        .expect("plan the model");
        let prepared = prepare_postgres_plan(
            fixture.path(),
            &execution_plan,
            &BTreeMap::new(),
            &TemplateOptions::default(),
        )
        .expect("resolve declared external source");

        assert_eq!(prepared.nodes.len(), 1);
        assert_eq!(
            prepared.nodes[0].sql.trim(),
            "select * from \"landing zone\".\"order facts\""
        );
    }

    #[test]
    fn prepared_plan_propagates_template_defaults_without_mutating_input() {
        let fixture = Fixture::new();
        fixture.write_model("first.sql", "{{ def_var('enabled', true) }}select 1");
        fixture.write_model(
            "second.sql",
            "{% if var('enabled') %}select * from {{ ref('first') }}{% endif %}",
        );
        let execution_plan = plan(
            fixture.path(),
            &["second.sql".to_owned()],
            ExecutionMode::Automatic,
        )
        .expect("plan models");
        let variables = BTreeMap::from([("external".to_owned(), json!("input"))]);
        let prepared = prepare_postgres_plan(
            fixture.path(),
            &execution_plan,
            &variables,
            &TemplateOptions::default(),
        )
        .expect("prepare SQL");

        assert_eq!(prepared.variables["enabled"], json!(true));
        assert_eq!(
            variables,
            BTreeMap::from([("external".to_owned(), json!("input"))])
        );
    }

    #[test]
    fn prepared_plan_rejects_models_that_collide_on_relation_stem() {
        let fixture = Fixture::new();
        fixture.write_model("a/orders.sql", "select 1");
        fixture.write_model("b/orders.sql", "select 2");
        let execution_plan = plan(
            fixture.path(),
            &["all".to_owned()],
            ExecutionMode::Automatic,
        )
        .expect("plan models");

        let error = prepare_postgres_plan(
            fixture.path(),
            &execution_plan,
            &BTreeMap::new(),
            &TemplateOptions::default(),
        )
        .expect_err("ambiguous materialization targets must fail");

        assert_eq!(error.code(), "DOWAI_PLAN_INVALID");
    }

    #[test]
    fn python_execution_requires_an_explicit_project_interpreter() {
        let fixture = Fixture::new();
        fixture.allow_external_writes();
        fs::write(
            fixture.0.join("models/model.py"),
            "def model(env):\n    return 1\n",
        )
        .expect("write Python model");

        let error = super::run_postgres(
            fixture.path(),
            super::RunRequest {
                targets: vec!["model.py".to_owned()],
                mode: Some(ExecutionMode::Automatic),
                profile: None,
                session_id: None,
                variables: BTreeMap::new(),
                confirm_destructive: false,
            },
        )
        .expect_err("Python execution must not pretend to run");

        assert_eq!(error.code(), "DOWAI_CONFIG_INVALID");
    }

    #[test]
    fn run_denies_database_writes_when_external_writes_are_disabled() {
        let fixture = Fixture::new();
        fixture.write_model("test.sql", "select 1 as value");
        fs::write(
            fixture
                .path()
                .parent()
                .expect("project has workspace")
                .join("workspace.json"),
            serde_json::to_vec(&json!({
                "schema_version": 2,
                "profiles": {
                    "warehouse": {
                        "type": "postgres",
                        "host": "127.0.0.1",
                        "port": 5432,
                        "user": "dowai",
                        "database": "analytics",
                        "password_env": "DOWAI_POLICY_TEST_NO_PASSWORD",
                        "tls": "disabled"
                    }
                }
            }))
            .expect("serialize workspace config"),
        )
        .expect("write workspace config");

        for confirm_destructive in [false, true] {
            let error = run_postgres(
                fixture.path(),
                RunRequest {
                    targets: vec!["test.sql".to_owned()],
                    mode: None,
                    profile: Some("warehouse".to_owned()),
                    session_id: None,
                    variables: BTreeMap::new(),
                    confirm_destructive,
                },
            )
            .expect_err("external database writes must be opt-in");

            assert_eq!(error.code(), "DOWAI_POLICY_DENIED");
            assert!(error.message().contains("External writes are disabled"));
        }
    }

    #[test]
    fn replacement_policy_requires_confirmation_when_configured() {
        let configuration = json!({
            "effective": {
                "security": {
                    "require_approval_for_destructive": true
                }
            }
        });

        assert!(!super::replacement_approval(&configuration, false).expect("valid policy"));
        assert!(super::replacement_approval(&configuration, true).expect("explicit approval"));
    }

    #[test]
    fn run_reports_missing_password_without_attempting_database_connection() {
        let fixture = Fixture::new();
        fixture.write_model("test.sql", "select 1 as value");
        fixture.allow_external_writes();
        let password_env = format!(
            "DOWAI_RUNTIME_TEST_MISSING_PASSWORD_{}_{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock is after Unix epoch")
                .as_nanos()
        );
        assert!(std::env::var_os(&password_env).is_none());
        let workspace_config = fixture
            .path()
            .parent()
            .expect("project has workspace")
            .join("workspace.json");
        fs::write(
            workspace_config,
            serde_json::to_vec(&json!({
                "schema_version": 2,
                "profiles": {
                    "warehouse": {
                        "type": "postgres",
                        "host": "127.0.0.1",
                        "port": 5432,
                        "user": "dowai",
                        "database": "analytics",
                        "password_env": password_env,
                        "tls": "disabled"
                    }
                }
            }))
            .expect("serialize workspace config"),
        )
        .expect("write workspace config");

        let error = run_postgres(
            fixture.path(),
            RunRequest {
                targets: vec!["test.sql".to_owned()],
                mode: None,
                profile: Some("warehouse".to_owned()),
                session_id: None,
                variables: BTreeMap::new(),
                confirm_destructive: false,
            },
        )
        .expect_err("missing password must stop the run before connecting");
        assert_eq!(error.code(), "DOWAI_CONNECTION_FAILED");
        assert!(!error
            .to_string()
            .contains("DOWAI_RUNTIME_TEST_MISSING_PASSWORD"));
    }

    #[test]
    fn run_rejects_empty_rendered_models_before_database_connection() {
        let fixture = Fixture::new();
        fixture.allow_external_writes();
        fixture.write_model(
            "test.sql",
            "{% if var('enabled') %}select 1 as value{% endif %}",
        );
        let workspace_config = fixture
            .path()
            .parent()
            .expect("project has workspace")
            .join("workspace.json");
        fs::write(
            workspace_config,
            serde_json::to_vec(&json!({
                "schema_version": 2,
                "profiles": {
                    "warehouse": {
                        "type": "postgres",
                        "host": "127.0.0.1",
                        "port": 5432,
                        "user": "dowai",
                        "database": "analytics",
                        "password_env": "DOWAI_RUNTIME_EMPTY_SQL_PASSWORD",
                        "tls": "disabled"
                    }
                }
            }))
            .expect("serialize workspace config"),
        )
        .expect("write workspace config");

        let error = run_postgres(
            fixture.path(),
            RunRequest {
                targets: vec!["test.sql".to_owned()],
                mode: None,
                profile: Some("warehouse".to_owned()),
                session_id: None,
                variables: BTreeMap::from([("enabled".to_owned(), json!(false))]),
                confirm_destructive: false,
            },
        )
        .expect_err("empty models must fail before attempting database connection");

        assert_eq!(error.code(), "DOWAI_TEMPLATE_INVALID");
        assert!(error
            .message()
            .contains("renders to an empty SQL statement"));
    }

    #[test]
    fn profile_and_host_allowlists_are_enforced_without_connecting() {
        let configuration = serde_json::json!({
            "effective": {
                "security": {
                    "allowed_profiles": ["approved"],
                    "allowed_hosts": ["db.example.test"]
                }
            },
            "origins": {
                "security.allowed_profiles": "project",
                "security.allowed_hosts": "admin"
            }
        });
        let approved = WorkspaceProfile {
            profile_type: "postgres".to_owned(),
            host: "db.example.test".to_owned(),
            port: 5432,
            user: "dowai".to_owned(),
            database: "analytics".to_owned(),
            password_env: "DOWAI_PG_PASSWORD".to_owned(),
            tls: ProfileTlsMode::Required,
        };
        assert!(super::enforce_profile_policy(&configuration, "approved", &approved).is_ok());
        assert_eq!(
            super::enforce_profile_policy(&configuration, "blocked", &approved)
                .expect_err("unlisted profile must be denied")
                .code(),
            "DOWAI_POLICY_DENIED"
        );
        let mut blocked_host = approved;
        blocked_host.host = "other.example.test".to_owned();
        assert_eq!(
            super::enforce_profile_policy(&configuration, "approved", &blocked_host)
                .expect_err("host outside admin list must be denied")
                .code(),
            "DOWAI_POLICY_DENIED"
        );
    }

    #[test]
    fn explicit_empty_administrator_allowlist_denies_all_profiles() {
        let configuration = serde_json::json!({
            "effective": {
                "security": {
                    "allowed_profiles": [],
                    "allowed_hosts": []
                }
            },
            "origins": {
                "security.allowed_profiles": "admin",
                "security.allowed_hosts": "engine_default"
            }
        });
        let profile = WorkspaceProfile {
            profile_type: "postgres".to_owned(),
            host: "localhost".to_owned(),
            port: 5432,
            user: "dowai".to_owned(),
            database: "analytics".to_owned(),
            password_env: "DOWAI_PG_PASSWORD".to_owned(),
            tls: ProfileTlsMode::Required,
        };

        assert_eq!(
            super::enforce_profile_policy(&configuration, "warehouse", &profile)
                .expect_err("empty administrator allowlist must deny")
                .code(),
            "DOWAI_POLICY_DENIED"
        );
    }

    #[test]
    fn run_uses_default_mode_and_respects_manual_mode_policy() {
        let configuration = serde_json::json!({
            "effective": {
                "execution": {
                    "default_mode": "manual",
                    "allow_manual_mode": false
                }
            }
        });
        assert_eq!(
            super::resolve_execution_mode(&configuration, None)
                .expect_err("manual mode policy is restrictive")
                .code(),
            "DOWAI_POLICY_DENIED"
        );
        assert_eq!(
            super::resolve_execution_mode(&configuration, Some(ExecutionMode::Automatic))
                .expect("explicit automatic mode is permitted")
                .as_str(),
            "automatic"
        );
    }
}
