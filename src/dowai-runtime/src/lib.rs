use auditor::Auditor;
use dowai_core::{
    admin_config_path, current_time_ms, default_run_store_path, load_project,
    load_workspace_profile, plan, read_planned_model, resolve_effective_configuration, sha256_hex,
    source_relation_key, user_config_path, verify_planned_model, CancellationToken, DowaiError,
    ExecutionMode, ExecutionPlan, Materialization, NewInvocation, NewRun, PlanNode, Result,
    RunStore, SessionStore, TemplateOptions, VariableOrigin, VersionedSnapshot, WorkspaceProfile,
};
use dowai_postgres::{
    quote_identifier, AdapterError, Materialization as PostgresMaterialization, PgProfile,
    PostgresAdapter, TlsMode,
};
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};
use uuid::Uuid;

mod auditor;
mod executor;
mod python_runtime;

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

/// Everything the run journal needs that is not part of a successful `RunResult`.
/// Holds identifiers, statuses and timings only; never values or SQL.
struct Trace {
    audit: Auditor,
    root_invocation_id: String,
    execution_mode: Option<String>,
    /// Python invocations salvaged from the executor when the run fails.
    python: Vec<InvocationRecord>,
    /// SQL node records and `SKIPPED` records for nodes never started.
    extra: Vec<InvocationRecord>,
}

impl Trace {
    fn node(
        &mut self,
        invocation_id: String,
        node: &PlanNode,
        mode: ExecutionMode,
        status: &str,
        started: Instant,
        error: Option<&DowaiError>,
    ) {
        self.extra.push(InvocationRecord {
            invocation_id,
            parent_invocation_id: self.root_invocation_id.clone(),
            call_type: "static".to_owned(),
            target: node.id.clone(),
            mode: mode.as_str().to_owned(),
            status: status.to_owned(),
            duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            error_code: error.map(|error| error.code().to_owned()),
        });
    }

    fn skip(&mut self, nodes: &[PlanNode], mode: ExecutionMode) {
        for node in nodes {
            self.node(
                Uuid::new_v4().to_string(),
                node,
                mode,
                "SKIPPED",
                Instant::now(),
                None,
            );
        }
    }
}

/// Runs the plan and records the outcome in the persistent journal (D-011).
///
/// Journal policy: a failed journal write never replaces the run outcome. On success
/// a warning is appended to `RunResult.warnings`; on failure the original error is
/// returned unchanged and a sanitized warning goes to stderr.
pub fn run_postgres(project_dir: &Path, request: RunRequest) -> Result<RunResult> {
    run_postgres_with_cancel(project_dir, request, &CancellationToken::new())
}

/// Same as [`run_postgres`], observing `cancel` (plan 5.5): between nodes, while a Python
/// worker is supervised (its process tree is terminated) and while a SQL statement runs
/// (PostgreSQL cancel request; the open transaction rolls back). A cancelled run ends as
/// `CANCELLED` with `DOWAI_CANCELLED`; nodes not started are `SKIPPED` and no session
/// variables are published.
pub fn run_postgres_with_cancel(
    project_dir: &Path,
    request: RunRequest,
    cancel: &CancellationToken,
) -> Result<RunResult> {
    run_postgres_detailed_with_cancel(project_dir, request, cancel).map_err(|failure| failure.error)
}

/// A run that did not succeed. `run_id` is set when the run started (ENT-015): the CLI and the
/// SDK report it so the failure can be found in the journal. It is `None` when the request
/// was refused before any run existed (for example, a retry refusal).
#[derive(Debug)]
pub struct RunFailure {
    pub run_id: Option<String>,
    pub error: DowaiError,
}

/// Same as [`run_postgres_with_cancel`], but a failure keeps the `run_id` of the started run.
pub fn run_postgres_detailed_with_cancel(
    project_dir: &Path,
    request: RunRequest,
    cancel: &CancellationToken,
) -> std::result::Result<RunResult, RunFailure> {
    run_selected(project_dir, request, None, cancel)
}

/// Options of `dowai retry` (B-05c). Variables are not in the journal, so they are passed
/// again whenever the original run used any.
#[derive(Clone, Debug, Default)]
pub struct RetryRequest {
    /// Re-executes only the models that finished `FAILED`. Their predecessors are not run.
    pub failed_only: bool,
    pub variables: BTreeMap<String, Value>,
    pub confirm_destructive: bool,
}

/// Re-executes a journaled run as a new run (B-05c). Allowed for runs that ended `FAILED` or
/// `CANCELLED` (including timeouts, which are `FAILED`). A run that succeeded, or that was
/// recorded before retry support, is refused. Refusals carry stable `DOWAI_RUN_*` or
/// `DOWAI_RETRY_*` codes and never start a run.
pub fn retry_run_with_cancel(
    project_dir: &Path,
    run_id: &str,
    request: RetryRequest,
    cancel: &CancellationToken,
) -> Result<RunResult> {
    retry_run_detailed_with_cancel(project_dir, run_id, request, cancel)
        .map_err(|failure| failure.error)
}

/// Same as [`retry_run_with_cancel`], but a failure of the new run keeps its `run_id`. A
/// refusal is returned with `run_id` `None`, because no new run was started.
pub fn retry_run_detailed_with_cancel(
    project_dir: &Path,
    run_id: &str,
    request: RetryRequest,
    cancel: &CancellationToken,
) -> std::result::Result<RunResult, RunFailure> {
    let (run, selection) = match prepare_retry(project_dir, run_id, request) {
        Ok(prepared) => prepared,
        Err(error) => {
            return Err(RunFailure {
                run_id: None,
                error,
            })
        }
    };
    run_selected(project_dir, run, selection, cancel)
}

/// Checks a retry request and returns the run it starts, with the selection of nodes. Every
/// refusal happens here, before any run exists.
fn prepare_retry(
    project_dir: &Path,
    run_id: &str,
    request: RetryRequest,
) -> Result<(RunRequest, Option<Vec<String>>)> {
    let path = default_run_store_path()
        .ok_or_else(|| DowaiError::State("No user state directory is available".to_owned()))?;
    let source = RunStore::for_user_state(path).retry_source(run_id)?;
    if source.status == "SUCCEEDED" {
        return Err(DowaiError::Retry {
            code: "DOWAI_RUN_NOT_RETRYABLE",
            message: "The run succeeded; there is nothing to retry".to_owned(),
        });
    }
    let targets = source
        .requested_targets
        .clone()
        .ok_or_else(|| DowaiError::Retry {
            code: "DOWAI_RUN_NOT_RETRYABLE",
            message: "The run was recorded before retry support; run it again with dowai run"
                .to_owned(),
        })?;
    let root = project_dir.canonicalize().map_err(|error| {
        DowaiError::Path(format!(
            "Project directory is unavailable: {} ({error})",
            project_dir.display()
        ))
    })?;
    if load_project(&root)?.name != source.project {
        return Err(DowaiError::Retry {
            code: "DOWAI_RETRY_PROJECT_MISMATCH",
            message: "The run belongs to a different project".to_owned(),
        });
    }
    let original_variables = source.variable_count.unwrap_or(0);
    if original_variables > 0 && request.variables.is_empty() {
        return Err(DowaiError::Retry {
            code: "DOWAI_RETRY_VARIABLES_REQUIRED",
            message: format!(
                "The original run received {original_variables} variable(s); their values are not stored. Pass them again with --vars"
            ),
        });
    }
    let mode = match source.execution_mode.as_deref() {
        Some("manual") => Some(ExecutionMode::Manual),
        Some("automatic") => Some(ExecutionMode::Automatic),
        _ => None,
    };
    let selection = if request.failed_only {
        if source.failed_nodes.is_empty() {
            return Err(DowaiError::Retry {
                code: "DOWAI_RETRY_NO_FAILED_NODES",
                message: "The original run has no FAILED model; retry without --failed-only"
                    .to_owned(),
            });
        }
        // Python variables are published only when a whole run succeeds (D-013). A Python
        // predecessor that succeeded in the original run therefore left nothing behind: a
        // selective retry would silently miss its variables.
        let configuration = effective_configuration(&root)?;
        let resolved_mode = resolve_execution_mode(&configuration, mode)?;
        let execution_plan = plan(&root, &targets, resolved_mode)?;
        if let Some(node) = uncommitted_python_predecessor(
            &execution_plan,
            &source.failed_nodes,
            &source.succeeded_nodes,
        ) {
            return Err(DowaiError::Retry {
                code: "DOWAI_RETRY_UNCOMMITTED_VARIABLES",
                message: format!(
                    "Python model {node} finished in the original run, but its variables were not committed; retry without --failed-only"
                ),
            });
        }
        Some(source.failed_nodes.clone())
    } else {
        None
    };
    Ok((
        RunRequest {
            targets,
            mode,
            profile: source.profile.clone(),
            session_id: source.session_id.clone(),
            variables: request.variables,
            confirm_destructive: request.confirm_destructive,
        },
        selection,
    ))
}

/// Runs `request`, keeping only the plan nodes named in `selection` when given.
fn run_selected(
    project_dir: &Path,
    request: RunRequest,
    selection: Option<Vec<String>>,
    cancel: &CancellationToken,
) -> std::result::Result<RunResult, RunFailure> {
    let started_ms = current_time_ms();
    let started = Instant::now();
    let run_id = Uuid::new_v4().to_string();
    let journal_request = JournalRequest {
        session: request.session_id.clone(),
        targets: request.targets.clone(),
        profile: request.profile.clone(),
        variable_count: u32::try_from(request.variables.len()).ok(),
    };
    let requested_mode = request.mode;
    let root_invocation_id = Uuid::new_v4().to_string();
    let mut trace = Trace {
        audit: Auditor::new(
            &run_id,
            &root_invocation_id,
            started_ms,
            journal_request.session.clone(),
            project_display_name(project_dir),
        ),
        root_invocation_id,
        execution_mode: requested_mode.map(|mode| mode.as_str().to_owned()),
        python: Vec::new(),
        extra: Vec::new(),
    };
    let outcome = execute_run(
        project_dir,
        request,
        &run_id,
        &mut trace,
        cancel,
        selection.as_deref(),
    );
    finish_audit(&mut trace, &outcome);
    let audit_failed = trace.audit.failed;
    let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let journal = journal_run(
        project_dir,
        &run_id,
        started_ms,
        duration_ms,
        &journal_request,
        &trace,
        &outcome,
    );
    match (outcome, journal) {
        (Ok(mut result), journal) => {
            if journal.is_err() {
                result
                    .warnings
                    .push("The run journal could not be written".to_owned());
            }
            if audit_failed {
                result
                    .warnings
                    .push("The audit trail could not be written".to_owned());
            }
            Ok(result)
        }
        (Err(error), journal) => {
            if journal.is_err() {
                eprintln!("Warning: the run journal could not be written");
            }
            if audit_failed {
                eprintln!("Warning: the audit trail could not be written");
            }
            Err(RunFailure {
                run_id: Some(run_id),
                error,
            })
        }
    }
}

/// Name of the project for audit/journal records: the manifest name, else the directory
/// name. Never an absolute path.
fn project_display_name(project_dir: &Path) -> Option<String> {
    let root = project_dir.canonicalize().ok()?;
    load_project(&root)
        .map(|project| project.name)
        .ok()
        .or_else(|| {
            root.file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
}

/// Closes the AUDIT stream of a run: policy denial, nested Python invocations
/// (emitted after the fact, from the executor history), then `run.finished`.
fn finish_audit(trace: &mut Trace, outcome: &Result<RunResult>) {
    trace.audit.ensure_started();
    let (nested, error) = match outcome {
        Ok(result) => (result.invocations.clone(), None),
        Err(error) => (trace.python.clone(), Some(error)),
    };
    if let Some(error) = error {
        if error.code().starts_with("DOWAI_POLICY_") {
            trace.audit.policy_denied(error.code());
        }
    }
    for record in nested.iter().filter(|record| record.call_type != "static") {
        trace.audit.python_invocation(
            &record.invocation_id,
            &record.parent_invocation_id,
            &record.call_type,
            &record.target,
            &record.status,
            record.error_code.as_deref(),
        );
    }
    if let Ok(result) = outcome {
        trace.audit.session_id = Some(result.session_id.clone());
    }
    trace.audit.run_finished(error);
}

/// SHA-256 of a planned model file's content (never the content itself).
fn model_hash(root: &Path, node: &PlanNode) -> Option<String> {
    read_planned_model(root, &node.path)
        .ok()
        .map(|content| sha256_hex(content.as_bytes()))
}

/// Runs one Python node, emitting `node.started` / `node.finished` around it.
fn run_python_node(
    executor: &mut executor::Executor<'_>,
    audit: &mut Auditor,
    root: &Path,
    node: &PlanNode,
    mode: ExecutionMode,
    variables: BTreeMap<String, Value>,
    root_invocation_id: &str,
) -> Result<BTreeMap<String, Value>> {
    let hash = model_hash(root, node);
    let mut started_id: Option<String> = None;
    // ENT-024: a Python model changed after planning is refused before the interpreter starts.
    let outcome = match verify_planned_model(root, node) {
        Err(error) => Err(error),
        Ok(_) => executor.run_static(node, mode, variables, root_invocation_id, &mut |id| {
            audit.node_started(id, &node.id, hash.clone());
            started_id = Some(id.to_owned());
        }),
    };
    let invocation_id = match started_id {
        Some(id) => id,
        None => {
            // Rejected before it started (invocation budget): still audit the attempt.
            let id = Uuid::new_v4().to_string();
            audit.node_started(&id, &node.id, hash);
            id
        }
    };
    audit.node_finished(&invocation_id, &node.id, outcome.as_ref().err());
    outcome
}

/// Publishes only what the run changed, atomically, against the run's base revisions.
/// A key changed by another run since the snapshot yields `DOWAI_SESSION_CONFLICT`.
fn publish_variables(
    store: &SessionStore,
    session_id: &str,
    base: &VersionedSnapshot,
    variables: &BTreeMap<String, Value>,
    run_id: &str,
    invocation_id: &str,
) -> Result<()> {
    let changes: BTreeMap<String, Value> = variables
        .iter()
        .filter(|(key, value)| base.values.get(*key) != Some(*value))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    let origin = VariableOrigin {
        run_id: run_id.to_owned(),
        invocation_id: invocation_id.to_owned(),
    };
    store.commit_variables(session_id, &changes, &base.revisions, &origin)
}

/// Names of the variables in the session after a run; values are never audited.
fn variable_names(variables: &BTreeMap<String, Value>) -> Vec<String> {
    variables.keys().cloned().collect()
}

/// The request facts the journal keeps for `dowai retry`. Never variable values.
struct JournalRequest {
    session: Option<String>,
    targets: Vec<String>,
    profile: Option<String>,
    variable_count: Option<u32>,
}

fn journal_run(
    project_dir: &Path,
    run_id: &str,
    started_ms: i64,
    duration_ms: u64,
    request: &JournalRequest,
    trace: &Trace,
    outcome: &Result<RunResult>,
) -> Result<()> {
    let root = project_dir.canonicalize()?;
    let project = load_project(&root)
        .map(|project| project.name)
        .unwrap_or_else(|_| {
            root.file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default()
        });
    let path = default_run_store_path()
        .ok_or_else(|| DowaiError::State("No user state directory is available".to_owned()))?;
    let (status, error_code, session_id, mode, python) = match outcome {
        Ok(result) => (
            "SUCCEEDED",
            None,
            Some(result.session_id.clone()),
            Some(result.execution_mode.clone()),
            result.invocations.as_slice(),
        ),
        Err(error) => (
            if error.is_cancelled() {
                "CANCELLED"
            } else {
                "FAILED"
            },
            Some(error.code().to_owned()),
            request.session.clone(),
            trace.execution_mode.clone(),
            trace.python.as_slice(),
        ),
    };
    let invocations: Vec<NewInvocation> = python
        .iter()
        .chain(trace.extra.iter())
        .map(|record| NewInvocation {
            invocation_id: record.invocation_id.clone(),
            parent_invocation_id: Some(record.parent_invocation_id.clone())
                .filter(|parent| *parent != trace.root_invocation_id),
            kind: if record.call_type == "imperative" || is_python_path(&record.target) {
                "python"
            } else {
                "sql"
            }
            .to_owned(),
            target: record.target.clone(),
            call_type: record.call_type.clone(),
            status: record.status.clone(),
            error_code: record.error_code.clone(),
            duration_ms: record.duration_ms,
        })
        .collect();
    RunStore::for_user_state(path).record(
        &NewRun {
            run_id: run_id.to_owned(),
            session_id,
            project,
            status: status.to_owned(),
            execution_mode: mode,
            started_at_ms: started_ms,
            finished_at_ms: current_time_ms(),
            duration_ms,
            error_code,
            requested_targets: Some(request.targets.clone()),
            profile: request.profile.clone(),
            variable_count: request.variable_count,
        },
        &invocations,
    )
}

/// First Python node that the failed models depend on, directly or not, and that finished
/// `SUCCEEDED` in the original run.
fn uncommitted_python_predecessor(
    execution_plan: &ExecutionPlan,
    failed: &[String],
    succeeded: &[String],
) -> Option<String> {
    let depends_on: BTreeMap<&str, &[String]> = execution_plan
        .nodes
        .iter()
        .map(|node| (node.id.as_str(), node.depends_on.as_slice()))
        .collect();
    let mut ancestors = BTreeSet::new();
    let mut pending: Vec<&str> = failed.iter().map(String::as_str).collect();
    while let Some(id) = pending.pop() {
        for dependency in depends_on.get(id).copied().unwrap_or(&[]) {
            if ancestors.insert(dependency.as_str()) {
                pending.push(dependency.as_str());
            }
        }
    }
    ancestors
        .into_iter()
        .find(|id| is_python_path(id) && succeeded.iter().any(|node| node == id))
        .map(str::to_owned)
}

/// Keeps only the selected nodes of `plan`. Predecessors outside the selection are not run;
/// their relations and session variables come from the earlier, committed runs.
fn restrict_plan(mut plan: ExecutionPlan, selection: Option<&[String]>) -> Result<ExecutionPlan> {
    let Some(selection) = selection else {
        return Ok(plan);
    };
    if let Some(missing) = selection
        .iter()
        .find(|id| !plan.nodes.iter().any(|node| &node.id == *id))
    {
        return Err(DowaiError::Retry {
            code: "DOWAI_RETRY_PLAN_CHANGED",
            message: format!("Failed model {missing} is no longer part of the plan"),
        });
    }
    plan.nodes.retain(|node| selection.contains(&node.id));
    Ok(plan)
}

fn execute_run(
    project_dir: &Path,
    request: RunRequest,
    run_id: &str,
    trace: &mut Trace,
    cancel: &CancellationToken,
    selection: Option<&[String]>,
) -> Result<RunResult> {
    cancel.check()?;
    let started = Instant::now();
    let root = project_dir.canonicalize().map_err(|error| {
        DowaiError::Path(format!(
            "Project directory is unavailable: {} ({error})",
            project_dir.display()
        ))
    })?;
    let configuration = effective_configuration(&root)?;
    let mode = resolve_execution_mode(&configuration, request.mode)?;
    trace.execution_mode = Some(mode.as_str().to_owned());
    trace.audit.mode = Some(mode.as_str().to_owned());
    trace.audit.config_hash = serde_json::to_vec(&configuration)
        .ok()
        .map(|bytes| sha256_hex(&bytes));
    let execution_plan = restrict_plan(plan(&root, &request.targets, mode)?, selection)?;
    trace.audit.plan_hash = serde_json::to_vec(&execution_plan)
        .ok()
        .map(|bytes| sha256_hex(&bytes));
    trace.audit.ensure_started();
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
    )
    .with_session_limits(dowai_core::session_limits_from_config(&configuration)?);
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
    // `wait_process`: the exclusive session lock is taken before the snapshot (so the base is
    // current) and before any node, and is held by this guard until the function returns on
    // every path. Nested `env.run` calls never reach this code, so they cannot self-block. An
    // implicit session (none requested) does not exist yet and nobody can share it: no lock.
    let _session_lock = match prior_session.as_ref() {
        Some(session) => acquire_session_lock(&store, &session.session_id, &configuration, cancel)?,
        None => None,
    };
    // Versioned snapshot taken once at run start: its revisions are the base against which
    // publication detects concurrent writes (AC040).
    let base = match prior_session.as_ref() {
        Some(session) => store.snapshot_versioned(&session.session_id)?,
        None => VersionedSnapshot::default(),
    };
    let mut variables = base.values.clone();
    variables.extend(request.variables);
    store.validate_variables(&variables)?;

    if python_only {
        let (project, python) = python_settings(&root)?;
        let interpreter = &python.interpreter;
        let models_dir = root.join(&project.models_dir);
        let run_invocation_id = trace.root_invocation_id.clone();
        let mut executor = executor::Executor::new(
            &root,
            &models_dir,
            interpreter,
            Duration::from_secs(python.timeout_seconds),
            &configuration,
            &store,
            cancel.clone(),
        )?;
        let mut completed_nodes = Vec::with_capacity(execution_plan.nodes.len());
        for (index, node) in execution_plan.nodes.iter().enumerate() {
            if let Err(error) = cancel.check() {
                trace.python = executor.invocations;
                trace.skip(&execution_plan.nodes[index..], mode);
                return Err(error);
            }
            match run_python_node(
                &mut executor,
                &mut trace.audit,
                &root,
                node,
                mode,
                variables,
                &run_invocation_id,
            ) {
                Ok(next) => variables = next,
                Err(error) => {
                    trace.python = executor.invocations;
                    trace.skip(&execution_plan.nodes[index + 1..], mode);
                    return Err(error);
                }
            }
            completed_nodes.push(RunNodeResult {
                id: node.id.clone(),
                relation: node.id.clone(),
                materialization: "none".to_owned(),
                status: "SUCCEEDED".to_owned(),
            });
        }
        let invocations = executor.invocations;
        trace.python = invocations.clone();
        let session = match prior_session {
            Some(session) => session,
            None => store.create(&root, dowai_core::session_ttl_from_config(&configuration)?)?,
        };
        trace.audit.session_id = Some(session.session_id.clone());
        publish_variables(
            &store,
            &session.session_id,
            &base,
            &variables,
            run_id,
            &trace.root_invocation_id,
        )?;
        trace.audit.variables_published(variable_names(&variables));
        return Ok(RunResult {
            schema_version: 2,
            session_id: session.session_id,
            run_id: run_id.to_owned(),
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
    trace.audit.profile = Some(profile_name.clone());
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
        let run_invocation_id = trace.root_invocation_id.clone();
        let mut executor = executor::Executor::new(
            &root,
            &models_dir,
            &python.interpreter,
            Duration::from_secs(python.timeout_seconds),
            &configuration,
            &store,
            cancel.clone(),
        )?;
        // AC002: Python models read SQL datasets of this plan, read-only, through the host.
        let sql_relations: BTreeSet<String> = execution_plan
            .nodes
            .iter()
            .filter(|node| !is_python_path(&node.path))
            .filter_map(|node| relations.get(&node.id.replace('\\', "/")).cloned())
            .collect();
        executor.datasets = Some(executor::DatasetSource::new(
            postgres_profile(profile.clone()),
            relations.clone(),
            sql_relations,
        ));
        let mut adapter = PostgresAdapter::connect(&postgres_profile(profile))
            .map_err(|error| DowaiError::Connection(error.to_string()))?;
        let mut completed_nodes = Vec::with_capacity(execution_plan.nodes.len());
        for (index, node) in execution_plan.nodes.iter().enumerate() {
            if let Err(error) = cancel.check() {
                trace.python = std::mem::take(&mut executor.invocations);
                trace.skip(&execution_plan.nodes[index..], mode);
                return Err(error);
            }
            if is_python_path(&node.path) {
                match run_python_node(
                    &mut executor,
                    &mut trace.audit,
                    &root,
                    node,
                    mode,
                    variables,
                    &run_invocation_id,
                ) {
                    Ok(next) => variables = next,
                    Err(error) => {
                        trace.python = std::mem::take(&mut executor.invocations);
                        trace.skip(&execution_plan.nodes[index + 1..], mode);
                        return Err(error);
                    }
                }
                completed_nodes.push(RunNodeResult {
                    id: node.id.clone(),
                    relation: node.id.clone(),
                    materialization: "none".to_owned(),
                    status: "SUCCEEDED".to_owned(),
                });
                continue;
            }
            let node_started = Instant::now();
            let sql_invocation_id = Uuid::new_v4().to_string();
            trace
                .audit
                .node_started(&sql_invocation_id, &node.id, model_hash(&root, node));
            let step = (|| {
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
                    .materialize_cancellable(
                        &prepared.relation,
                        match prepared.materialization {
                            Materialization::Table => PostgresMaterialization::Table,
                            Materialization::View => PostgresMaterialization::View,
                        },
                        &prepared.sql,
                        allow_replace,
                        &|| cancel.is_cancelled(),
                    )
                    .map_err(map_execution_error)?;
                Ok((prepared, next_variables))
            })();
            match step {
                Ok((prepared, next_variables)) => {
                    trace
                        .audit
                        .external_write(&sql_invocation_id, &node.id, &prepared.relation);
                    trace
                        .audit
                        .node_finished(&sql_invocation_id, &node.id, None);
                    trace.node(
                        sql_invocation_id,
                        node,
                        mode,
                        "SUCCEEDED",
                        node_started,
                        None,
                    );
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
                Err(error) => {
                    trace
                        .audit
                        .node_finished(&sql_invocation_id, &node.id, Some(&error));
                    trace.node(
                        sql_invocation_id,
                        node,
                        mode,
                        node_failure_status(&error),
                        node_started,
                        Some(&error),
                    );
                    trace.python = std::mem::take(&mut executor.invocations);
                    trace.skip(&execution_plan.nodes[index + 1..], mode);
                    return Err(error);
                }
            }
        }
        let session = match prior_session {
            Some(session) => session,
            None => store.create(&root, dowai_core::session_ttl_from_config(&configuration)?)?,
        };
        trace.audit.session_id = Some(session.session_id.clone());
        publish_variables(
            &store,
            &session.session_id,
            &base,
            &variables,
            run_id,
            &trace.root_invocation_id,
        )?;
        trace.audit.variables_published(variable_names(&variables));
        return Ok(RunResult {
            schema_version: 2,
            session_id: session.session_id,
            run_id: run_id.to_owned(),
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
    for (index, node) in prepared.nodes.iter().enumerate() {
        if let Err(error) = cancel.check() {
            trace.skip(&execution_plan.nodes[index..], mode);
            return Err(error);
        }
        let materialization = match node.materialization {
            Materialization::Table => PostgresMaterialization::Table,
            Materialization::View => PostgresMaterialization::View,
        };
        let node_started = Instant::now();
        // `prepared.nodes` mirrors `execution_plan.nodes` one to one.
        let plan_node = &execution_plan.nodes[index];
        let sql_invocation_id = Uuid::new_v4().to_string();
        trace.audit.node_started(
            &sql_invocation_id,
            &plan_node.id,
            model_hash(&root, plan_node),
        );
        if let Err(error) = adapter
            .materialize_cancellable(
                &node.relation,
                materialization,
                &node.sql,
                allow_replace,
                &|| cancel.is_cancelled(),
            )
            .map_err(map_execution_error)
        {
            trace
                .audit
                .node_finished(&sql_invocation_id, &plan_node.id, Some(&error));
            trace.node(
                sql_invocation_id,
                plan_node,
                mode,
                node_failure_status(&error),
                node_started,
                Some(&error),
            );
            trace.skip(&execution_plan.nodes[index + 1..], mode);
            return Err(error);
        }
        trace
            .audit
            .external_write(&sql_invocation_id, &plan_node.id, &node.relation);
        trace
            .audit
            .node_finished(&sql_invocation_id, &plan_node.id, None);
        trace.node(
            sql_invocation_id,
            plan_node,
            mode,
            "SUCCEEDED",
            node_started,
            None,
        );
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
        None => store.create(&root, dowai_core::session_ttl_from_config(&configuration)?)?,
    };
    trace.audit.session_id = Some(session.session_id.clone());
    publish_variables(
        &store,
        &session.session_id,
        &base,
        &prepared.variables,
        run_id,
        &trace.root_invocation_id,
    )?;
    trace
        .audit
        .variables_published(variable_names(&prepared.variables));
    Ok(RunResult {
        schema_version: 2,
        session_id: session.session_id,
        run_id: run_id.to_owned(),
        invocation_id: trace.root_invocation_id.clone(),
        status: "SUCCEEDED".to_owned(),
        execution_mode: execution_plan.execution_mode,
        duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        profile: profile_name.to_owned(),
        nodes: completed_nodes,
        invocations: Vec::new(),
        warnings: execution_plan.warnings,
    })
}

/// Final status of a node whose execution ended in `error`.
fn node_failure_status(error: &DowaiError) -> &'static str {
    if error.is_cancelled() {
        "CANCELLED"
    } else {
        "FAILED"
    }
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

/// Name of the default connection profile declared by the project, read without connecting (ENT-016).
pub fn declared_default_profile(project_dir: &Path) -> Result<Option<String>> {
    let root = project_dir.canonicalize().map_err(|error| {
        DowaiError::Path(format!(
            "Project directory is unavailable: {} ({error})",
            project_dir.display()
        ))
    })?;
    let configuration = effective_configuration(&root)?;
    Ok(configuration["effective"]["execution"]["default_profile"]
        .as_str()
        .map(str::to_owned))
}

/// Read-only check of the Python runtime declared by the project (ENT-016).
/// Returns `None` when the project does not configure Python; otherwise the validated interpreter.
pub fn check_python_runtime(project_dir: &Path) -> Result<Option<PathBuf>> {
    let root = project_dir.canonicalize().map_err(|error| {
        DowaiError::Path(format!(
            "Project directory is unavailable: {} ({error})",
            project_dir.display()
        ))
    })?;
    if load_project(&root)?.python.is_none() {
        return Ok(None);
    }
    let (_, python) = python_settings(&root)?;
    Ok(Some(python.interpreter))
}

/// Verifies that the `dowai` SDK can be imported by the project interpreter, in isolated mode
/// and with a bounded time (ENT-016). Nothing is installed.
pub fn check_python_sdk(interpreter: &Path, project_dir: &Path) -> Result<()> {
    const SDK_CHECK_TIMEOUT: Duration = Duration::from_secs(15);
    let mut child = std::process::Command::new(interpreter)
        .args(["-I", "-c", "import dowai"])
        .current_dir(project_dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|_| {
            DowaiError::PythonFailure(
                "Could not start the configured project interpreter".to_owned(),
            )
        })?;
    let deadline = std::time::Instant::now() + SDK_CHECK_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(_)) => {
                return Err(DowaiError::PythonFailure(
                    "The dowai SDK is not importable from the project interpreter".to_owned(),
                ))
            }
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(DowaiError::PythonTimeout(
                    "The SDK check exceeded its time limit".to_owned(),
                ));
            }
            Err(_) => {
                return Err(DowaiError::PythonFailure(
                    "Could not monitor the SDK check process".to_owned(),
                ))
            }
        }
    }
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
    let source = verify_planned_model(project_dir, node)?;
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
    let allow_dowai_if_alias = templates["allow_dowai_if_alias"].as_bool().ok_or_else(|| {
        DowaiError::Config("Effective templates.allow_dowai_if_alias is invalid".to_owned())
    })?;
    Ok(TemplateOptions {
        max_nesting,
        strict_undefined,
        allow_dowai_if_alias,
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
        AdapterError::Cancelled => DowaiError::cancelled(),
        AdapterError::MissingPassword
        | AdapterError::TlsConfigurationFailed
        | AdapterError::ConnectionFailed => DowaiError::Connection(error.to_string()),
    }
}

const LOCK_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Honours `sessions.conflict_policy`: `None` for `error`; for `wait_process` polls the
/// per-session lock until acquired, cancelled, or `wait_process_timeout_seconds` elapses.
fn acquire_session_lock(
    store: &SessionStore,
    session_id: &str,
    configuration: &Value,
    cancel: &CancellationToken,
) -> Result<Option<dowai_core::SessionLock>> {
    let sessions = &configuration["effective"]["sessions"];
    if sessions["conflict_policy"].as_str() != Some("wait_process") {
        return Ok(None);
    }
    let seconds = sessions["wait_process_timeout_seconds"]
        .as_u64()
        .ok_or_else(|| {
            DowaiError::Config("Effective sessions.wait_process_timeout_seconds is invalid".into())
        })?;
    let deadline = Instant::now() + Duration::from_secs(seconds);
    loop {
        cancel.check()?;
        if let Some(lock) = store.try_lock_session(session_id)? {
            return Ok(Some(lock));
        }
        let now = Instant::now();
        if now >= deadline {
            return Err(DowaiError::SessionConflict(format!(
                "Session is held by another run; waited {seconds}s (sessions.wait_process_timeout_seconds)"
            )));
        }
        thread::sleep(LOCK_POLL_INTERVAL.min(deadline - now));
    }
}

fn default_session_path() -> Option<PathBuf> {
    dowai_core::default_session_store_path()
}

#[cfg(test)]
mod tests {
    use super::{
        prepare_postgres_plan, run_postgres, run_postgres_detailed_with_cancel, RunRequest,
    };
    use dowai_core::{
        create_project, plan, CancellationToken, ExecutionMode, Materialization, ProfileTlsMode,
        TemplateOptions, WorkspaceProfile,
    };
    use serde_json::json;
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};
    use uuid::Uuid;

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            // The run journal writes to the user state directory; point it at a
            // process-private directory once so unit tests never touch the real one.
            static ISOLATED_STATE: std::sync::Once = std::sync::Once::new();
            ISOLATED_STATE.call_once(|| {
                let state = std::env::temp_dir()
                    .join(format!("dowai-runtime-unit-state-{}", std::process::id()));
                std::env::set_var("XDG_STATE_HOME", &state);
                std::env::set_var("LOCALAPPDATA", &state);
            });
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

        let failure = run_postgres_detailed_with_cancel(
            fixture.path(),
            RunRequest {
                targets: vec!["test.sql".to_owned()],
                mode: None,
                profile: Some("warehouse".to_owned()),
                session_id: None,
                variables: BTreeMap::new(),
                confirm_destructive: false,
            },
            &CancellationToken::new(),
        )
        .expect_err("missing password must stop the run before connecting");
        assert_eq!(failure.error.code(), "DOWAI_CONNECTION_FAILED");
        assert!(!failure
            .error
            .to_string()
            .contains("DOWAI_RUNTIME_TEST_MISSING_PASSWORD"));
        // ENT-015: the run started, so its identifier is reported with the failure.
        let run_id = failure.run_id.expect("a started run reports its run_id");
        assert!(
            Uuid::parse_str(&run_id).is_ok(),
            "run_id is a UUID: {run_id}"
        );
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
