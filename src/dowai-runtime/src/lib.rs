use auditor::Auditor;
use dowai_core::{
    admin_config_path, current_time_ms, default_run_store_path, load_project,
    load_workspace_profile, plan, read_planned_model, resolve_effective_configuration, sha256_hex,
    source_relation_key, static_direct_write_signals, user_config_path, verify_planned_model,
    CancellationToken, DowaiError, ExecutionMode, ExecutionPlan, LogPolicy, Materialization,
    NewInvocation, NewRun, PlanNode, Result, RunStart, RunStore, SessionStore, TemplateOptions,
    VariableOrigin, VersionedSnapshot, WorkspaceProfile,
};
use dowai_mysql::MysqlProfile;
use dowai_postgres::{
    quote_identifier, AdapterError, Materialization as PostgresMaterialization, PgProfile,
    PostgresAdapter, TlsMode,
};
pub use plugin_exec::{execute_plugin, PluginExecution, PluginPolicy, DEFAULT_MAX_OUTPUT_BYTES};
use retry::{run_with_retry, wait_unless_cancelled, RetryPolicy};
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};
use uuid::Uuid;

mod auditor;
mod dataset_query;
mod executor;
mod external_write;
mod plugin_exec;
mod python_runtime;
mod resources;
mod retry;
mod waves;

/// Refusal message of a full refresh without `--yes` (ENT-033).
pub const FULL_REFRESH_CONFIRMATION_REQUIRED: &str =
    "Full refresh replaces existing relations and requires explicit confirmation (--yes)";

#[derive(Clone, Debug)]
pub struct RunRequest {
    pub targets: Vec<String>,
    pub mode: Option<ExecutionMode>,
    pub profile: Option<String>,
    pub session_id: Option<String>,
    pub variables: BTreeMap<String, Value>,
    pub confirm_destructive: bool,
    /// Full refresh (ENT-033): rebuilds the selected SQL relations through staging and swap.
    /// Requires `confirm_destructive`; refused before any run starts otherwise.
    pub full_refresh: bool,
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

    /// Records of one node of a concurrent wave (ADR 0008 §6), merged by `absorb` in plan order.
    fn fork(&self) -> Trace {
        Trace {
            audit: self.audit.fork(),
            root_invocation_id: self.root_invocation_id.clone(),
            execution_mode: self.execution_mode.clone(),
            python: Vec::new(),
            extra: Vec::new(),
        }
    }

    fn absorb(&mut self, child: Trace) {
        self.audit.absorb(child.audit);
        self.extra.extend(child.extra);
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
    if request.full_refresh && !request.confirm_destructive {
        // ENT-033: refused before a run exists, so nothing is journaled or written.
        return Err(RunFailure {
            run_id: None,
            error: DowaiError::Policy(FULL_REFRESH_CONFIRMATION_REQUIRED.to_owned()),
        });
    }
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
    // ENT-112: a full refresh is destructive by definition. Its retry needs the same explicit
    // confirmation, and it reapplies the mode. Runs without the flag (`None`) are not full.
    let full_refresh = source.full_refresh == Some(true);
    if full_refresh && !request.confirm_destructive {
        return Err(DowaiError::Policy(
            FULL_REFRESH_CONFIRMATION_REQUIRED.to_owned(),
        ));
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
            full_refresh,
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
        full_refresh: request.full_refresh,
    };
    let requested_mode = request.mode;
    let root_invocation_id = Uuid::new_v4().to_string();
    // Operational log policy (ENT-065). Logging must not block a run: an unreadable
    // configuration falls back to the catalog defaults.
    let log_policy = effective_configuration(project_dir)
        .map(|configuration| LogPolicy::from_logging(&configuration["effective"]["logging"]))
        .unwrap_or_default();
    let mut trace = Trace {
        audit: Auditor::new(
            &run_id,
            &root_invocation_id,
            started_ms,
            journal_request.session.clone(),
            project_display_name(project_dir),
            log_policy,
        ),
        root_invocation_id,
        execution_mode: requested_mode.map(|mode| mode.as_str().to_owned()),
        python: Vec::new(),
        extra: Vec::new(),
    };
    // Startup recovery and in-flight marker (ENT-062). The journal is optional for execution:
    // failures become warnings, as the final journal write already does.
    let journal_store = default_run_store_path().map(RunStore::for_user_state);
    if let Some(store) = &journal_store {
        match store.recover_interrupted(current_time_ms()) {
            Ok(report) => {
                if !report.interrupted_runs.is_empty() {
                    eprintln!(
                        "Warning: {} interrupted run(s) recorded as FAILED ({})",
                        report.interrupted_runs.len(),
                        dowai_core::RUN_INTERRUPTED_CODE
                    );
                }
                if report.uncertain_effects > 0 {
                    eprintln!(
                        "Warning: {} outbox effect(s) marked UNCERTAIN; they are not resent",
                        report.uncertain_effects
                    );
                }
            }
            Err(error) => eprintln!("Warning: interrupted runs could not be recovered: {error}"),
        }
    }
    // Held until the end of this function, so it outlives the journal write below.
    let _lease = journal_store.as_ref().and_then(|store| {
        store
            .begin_run(&RunStart {
                run_id: run_id.clone(),
                session_id: journal_request.session.clone(),
                project: project_display_name(project_dir).unwrap_or_default(),
                execution_mode: trace.execution_mode.clone(),
                started_at_ms: started_ms,
                requested_targets: Some(journal_request.targets.clone()),
                profile: journal_request.profile.clone(),
                variable_count: journal_request.variable_count,
                full_refresh: Some(journal_request.full_refresh),
            })
            .map_err(|error| eprintln!("Warning: the run start could not be journaled: {error}"))
            .ok()
    });
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
        trace.audit.python_invocation(record);
        // A nested or predecessor SQL call that succeeded wrote its relation (ENT-114): the same
        // `external_write` event that a static SQL node emits.
        if record.status == "SUCCEEDED" && is_sql_path(&record.target) {
            if let Ok(relation) = model_stem(&record.target) {
                trace
                    .audit
                    .external_write(&record.invocation_id, &record.target, &relation);
            }
        }
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
    full_refresh: bool,
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
            kind: if is_sql_path(&record.target) {
                "sql"
            } else {
                "python"
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
            full_refresh: Some(request.full_refresh),
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
    let retry_policy = RetryPolicy::from_configuration(&configuration)?;
    let mode = resolve_execution_mode(&configuration, request.mode)?;
    trace.execution_mode = Some(mode.as_str().to_owned());
    trace.audit.mode = Some(mode.as_str().to_owned());
    trace.audit.config_hash = serde_json::to_vec(&configuration)
        .ok()
        .map(|bytes| sha256_hex(&bytes));
    let mut execution_plan = restrict_plan(plan(&root, &request.targets, mode)?, selection)?;
    trace.audit.plan_hash = serde_json::to_vec(&execution_plan)
        .ok()
        .map(|bytes| sha256_hex(&bytes));
    trace.audit.ensure_started();
    if execution_plan.nodes.is_empty() {
        return Err(DowaiError::Model(
            "The execution plan contains no models".to_owned(),
        ));
    }
    // ENT-038: limites não aplicados ou parciais são reportados no resultado do run.
    execution_plan.warnings.extend(resources::run_warnings(
        &configuration,
        &execution_plan.nodes,
    )?);
    // ENT-049: escrita direta por bibliotecas gera aviso no resultado; recusada se configurada.
    let direct_writes = direct_write_warnings(&root, &configuration, &execution_plan.nodes)?;
    execution_plan.warnings.extend(direct_writes);
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
            executor::WorkerSettings {
                interpreter,
                timeout: Duration::from_secs(python.timeout_seconds),
            },
            &configuration,
            &store,
            cancel.clone(),
            executor::RunIdentity {
                run_id: run_id.to_owned(),
                session_id: prior_session
                    .as_ref()
                    .map(|session| session.session_id.clone()),
            },
        )?;
        // ENT-114: `env.run("x.sql")` uses the default profile of the run. A missing profile fails
        // only the SQL call that needs it; the connection opens on that first call.
        let sql_profile = resolve_profile(&root, &configuration, None)
            .and_then(|(_, workspace_profile)| postgres_profile(workspace_profile, &configuration))
            .map_err(|error| error.message());
        executor.sql = Some(SqlExecution::new(
            sql_profile,
            None,
            template_options(&configuration)?,
            replacement_approval(&configuration, request.confirm_destructive)?,
            request.full_refresh,
            retry_policy,
            cancel.clone(),
        ));
        // ENT-111: declared sources are readable in Python-only runs. The profile comes from the
        // configuration only; the connection opens on the first dataset read. A Python-only plan
        // has no SQL relation, so model datasets are refused (see `DatasetSource::authorize`).
        let dataset_profile = resolve_profile(&root, &configuration, None)
            .and_then(|(_, workspace_profile)| postgres_profile(workspace_profile, &configuration))
            .map_err(|_| "No PostgreSQL profile is available for datasets");
        executor.datasets = Some(
            executor::DatasetSource::new(
                dataset_profile,
                build_relations(&root, &execution_plan)?,
                BTreeSet::new(),
                declared_source_relations(&root)?,
            )
            .with_mysql(declared_mysql_sources(&root, &configuration)?),
        );
        let completed_nodes = match run_nodes_sequentially(
            trace,
            &execution_plan.nodes,
            mode,
            cancel,
            |trace, node| {
                variables = run_python_node(
                    &mut executor,
                    &mut trace.audit,
                    &root,
                    node,
                    mode,
                    std::mem::take(&mut variables),
                    &run_invocation_id,
                )?;
                Ok(python_node_result(node))
            },
        ) {
            Ok(completed) => completed,
            Err(error) => {
                trace.python = std::mem::take(&mut executor.invocations);
                return Err(error);
            }
        };
        // ENT-111: the dataset profile is resolved from configuration only. A failure is kept
        // by the source and reported on the first dataset read; the connection opens only then.
        // Fallback warnings of dataset reads are reported as in mixed runs (ENT-041).
        if let Some(datasets) = executor.datasets.as_mut() {
            execution_plan.warnings.extend(datasets.take_warnings());
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
            executor::WorkerSettings {
                interpreter: &python.interpreter,
                timeout: Duration::from_secs(python.timeout_seconds),
            },
            &configuration,
            &store,
            cancel.clone(),
            executor::RunIdentity {
                run_id: run_id.to_owned(),
                session_id: prior_session
                    .as_ref()
                    .map(|session| session.session_id.clone()),
            },
        )?;
        // AC002: Python models read SQL datasets of this plan, read-only, through the host.
        let sql_relations: BTreeSet<String> = execution_plan
            .nodes
            .iter()
            .filter(|node| !is_python_path(&node.path))
            .filter_map(|node| relations.get(&node.id.replace('\\', "/")).cloned())
            .collect();
        executor.datasets = Some(
            executor::DatasetSource::new(
                Ok(postgres_profile(profile.clone(), &configuration)?),
                relations.clone(),
                sql_relations,
                declared_source_relations(&root)?,
            )
            .with_mysql(declared_mysql_sources(&root, &configuration)?),
        );
        let pg_profile = postgres_profile(profile, &configuration)?;
        // Connected before the first node, as before ENT-114; nested SQL calls reuse it.
        let adapter = PostgresAdapter::connect(&pg_profile)
            .map_err(|error| DowaiError::Connection(error.to_string()))?;
        executor.sql = Some(SqlExecution::new(
            Ok(pg_profile),
            Some(adapter),
            template_options.clone(),
            allow_replace,
            request.full_refresh,
            retry_policy,
            cancel.clone(),
        ));
        let completed_nodes = match run_nodes_sequentially(
            trace,
            &execution_plan.nodes,
            mode,
            cancel,
            |trace, node| {
                if is_python_path(&node.path) {
                    variables = run_python_node(
                        &mut executor,
                        &mut trace.audit,
                        &root,
                        node,
                        mode,
                        std::mem::take(&mut variables),
                        &run_invocation_id,
                    )?;
                    return Ok(python_node_result(node));
                }
                let hash = model_hash(&root, node);
                let (prepared, next_variables) = match render_checked(
                    &root,
                    node,
                    &variables,
                    &template_options,
                    &relations,
                    &store,
                ) {
                    Ok(rendered) => rendered,
                    Err(error) => {
                        // Rendering failed before any attempt: the node is journaled once.
                        let invocation_id = Uuid::new_v4().to_string();
                        let node_started = Instant::now();
                        trace.audit.node_started(&invocation_id, &node.id, hash);
                        trace
                            .audit
                            .node_finished(&invocation_id, &node.id, Some(&error));
                        trace.node(
                            invocation_id,
                            node,
                            mode,
                            node_failure_status(&error),
                            node_started,
                            Some(&error),
                        );
                        return Err(error);
                    }
                };
                match executor.sql.as_mut() {
                    Some(sql) => sql.materialize(node, mode, hash, Some(&mut *trace), &prepared)?,
                    None => {
                        return Err(DowaiError::Execution(
                            "SQL execution is not configured for this run".to_owned(),
                        ))
                    }
                };
                variables = next_variables;
                Ok(RunNodeResult {
                    id: prepared.id,
                    relation: prepared.relation,
                    materialization: match prepared.materialization {
                        Materialization::Table => "table",
                        Materialization::View => "view",
                    }
                    .to_owned(),
                    status: "SUCCEEDED".to_owned(),
                })
            },
        ) {
            Ok(completed) => completed,
            Err(error) => {
                trace.python = std::mem::take(&mut executor.invocations);
                return Err(error);
            }
        };
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
        // Fallback filters of dataset reads are reported, never applied silently (ENT-041).
        if let Some(datasets) = executor.datasets.as_mut() {
            execution_plan.warnings.extend(datasets.take_warnings());
        }
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

    let (completed_nodes, final_variables) = if matches!(mode, ExecutionMode::Automatic) {
        // ENT-120b (ADR 0008): automatic SQL runs go wave by wave, with the waves' concurrency.
        // The manual mode keeps the sequential loop below, unchanged.
        let wave_plan = waves::waves(&execution_plan.nodes)?;
        let concurrency = wave_concurrency(&configuration)?;
        let relations = build_relations(&root, &execution_plan)?;
        let (rendered, final_variables) = render_sql_waves(
            &root,
            &execution_plan.nodes,
            &wave_plan,
            &variables,
            &template_options,
            &relations,
            &store,
        )?;
        let pg_profile = postgres_profile(profile, &configuration)?;
        let mut adapter = PostgresAdapter::connect(&pg_profile)
            .map_err(|error| DowaiError::Connection(error.to_string()))?;
        let context = WaveContext {
            root: &root,
            profile: &pg_profile,
            mode,
            retry_policy,
            allow_replace: replacement_approval(&configuration, request.confirm_destructive)?,
            full_refresh: request.full_refresh,
            cancel,
            concurrency,
        };
        let completed = run_sql_waves(
            trace,
            &execution_plan.nodes,
            &wave_plan,
            rendered,
            &mut adapter,
            &context,
        )?;
        (completed, final_variables)
    } else {
        let prepared =
            prepare_postgres_plan(&root, &execution_plan, &variables, &template_options)?;
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
        let pg_profile = postgres_profile(profile, &configuration)?;
        let mut adapter = PostgresAdapter::connect(&pg_profile)
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
            // `prepared.nodes` mirrors `execution_plan.nodes` one to one.
            let plan_node = &execution_plan.nodes[index];
            if let Err(error) = materialize_with_retry(
                &mut adapter,
                &pg_profile,
                plan_node,
                model_hash(&root, plan_node),
                mode,
                Some(&mut *trace),
                retry_policy,
                cancel,
                SqlWrite {
                    relation: &node.relation,
                    materialization,
                    sql: &node.sql,
                    allow_replace,
                    full_refresh: request.full_refresh,
                },
            ) {
                trace.skip(&execution_plan.nodes[index + 1..], mode);
                return Err(error);
            }
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

        (completed_nodes, prepared.variables)
    };
    let session = match prior_session {
        Some(session) => session,
        None => store.create(&root, dowai_core::session_ttl_from_config(&configuration)?)?,
    };
    trace.audit.session_id = Some(session.session_id.clone());
    publish_variables(
        &store,
        &session.session_id,
        &base,
        &final_variables,
        run_id,
        &trace.root_invocation_id,
    )?;
    trace
        .audit
        .variables_published(variable_names(&final_variables));
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

/// Sequential scheduler shared by the Python-only and mixed runs (ENT-118). Nodes run one by
/// one in plan order. A cancelled run or a failed node records every node not started as
/// `SKIPPED` and returns the error. `run_node` records its own journal entries for the node
/// it runs, including the failure of that node.
fn run_nodes_sequentially<F>(
    trace: &mut Trace,
    nodes: &[PlanNode],
    mode: ExecutionMode,
    cancel: &CancellationToken,
    mut run_node: F,
) -> Result<Vec<RunNodeResult>>
where
    F: FnMut(&mut Trace, &PlanNode) -> Result<RunNodeResult>,
{
    let mut completed = Vec::with_capacity(nodes.len());
    for (index, node) in nodes.iter().enumerate() {
        if let Err(error) = cancel.check() {
            trace.skip(&nodes[index..], mode);
            return Err(error);
        }
        match run_node(trace, node) {
            Ok(result) => completed.push(result),
            Err(error) => {
                trace.skip(&nodes[index + 1..], mode);
                return Err(error);
            }
        }
    }
    Ok(completed)
}

/// Concurrency of an automatic SQL wave: `min(runtime.max_parallel_nodes,
/// runtime.max_worker_processes)` from the effective configuration (ADR 0008 §2).
fn wave_concurrency(configuration: &Value) -> Result<usize> {
    let runtime = &configuration["effective"]["runtime"];
    let limit = |key: &str| {
        runtime[key]
            .as_u64()
            .filter(|value| (1..=256).contains(value))
            .ok_or_else(|| DowaiError::Config(format!("Effective runtime.{key} is invalid")))
    };
    let parallel = limit("max_parallel_nodes")?;
    let workers = limit("max_worker_processes")?;
    usize::try_from(parallel.min(workers))
        .map_err(|_| DowaiError::Config("Effective runtime concurrency is invalid".to_owned()))
}

/// One wave of the automatic SQL path, rendered before any statement runs (ADR 0008 §3).
struct RenderedWave {
    /// Plan index and rendered SQL of each node, in plan order.
    nodes: Vec<(usize, PreparedNode)>,
    /// A variable written by two nodes of this wave (ADR 0008 §4). It fails the run at the wave end.
    conflict: Option<DowaiError>,
}

/// Renders the waves of an automatic SQL run before any statement runs. Every node of a wave is
/// rendered with the variables published before that wave (ADR 0008 §3), so a node does not see a
/// sibling's variable. Returns the waves and the run's final variables. Rendering stops at the
/// first conflicting wave: the run fails when that wave ends, after its nodes ran.
fn render_sql_waves(
    project_dir: &Path,
    plan_nodes: &[PlanNode],
    wave_plan: &[Vec<usize>],
    variables: &BTreeMap<String, Value>,
    template_options: &TemplateOptions,
    relations: &BTreeMap<String, String>,
    store: &SessionStore,
) -> Result<(Vec<RenderedWave>, BTreeMap<String, Value>)> {
    let mut state = variables.clone();
    let mut rendered = Vec::with_capacity(wave_plan.len());
    for wave in wave_plan {
        let snapshot = state.clone();
        let mut nodes = Vec::with_capacity(wave.len());
        let mut writes: HashMap<usize, Vec<String>> = HashMap::with_capacity(wave.len());
        let mut changes = Vec::with_capacity(wave.len());
        for &index in wave {
            let (prepared, next) = render_checked(
                project_dir,
                &plan_nodes[index],
                &snapshot,
                template_options,
                relations,
                store,
            )?;
            // A variable is written when the node changes or adds it (`def_var` only sets absent keys).
            let changed: BTreeMap<String, Value> = next
                .into_iter()
                .filter(|(key, value)| snapshot.get(key) != Some(value))
                .collect();
            writes.insert(index, changed.keys().cloned().collect());
            changes.push(changed);
            nodes.push((index, prepared));
        }
        let conflict = waves::detect_variable_conflicts(wave, &writes, plan_nodes).err();
        let conflicted = conflict.is_some();
        rendered.push(RenderedWave { nodes, conflict });
        if conflicted {
            break;
        }
        for changed in changes {
            state.extend(changed);
        }
    }
    if rendered.iter().all(|wave| wave.conflict.is_none()) {
        store.validate_variables(&state)?;
    }
    Ok((rendered, state))
}

/// Inputs shared by the SQL nodes of an automatic run.
struct WaveContext<'a> {
    root: &'a Path,
    profile: &'a PgProfile,
    mode: ExecutionMode,
    retry_policy: RetryPolicy,
    allow_replace: bool,
    full_refresh: bool,
    cancel: &'a CancellationToken,
    /// `min(max_parallel_nodes, max_worker_processes)`; `1` runs every wave in order.
    concurrency: usize,
}

/// Records and outcome of one node of a wave. They stay apart until the wave merges them in plan
/// order, whatever the completion order (ADR 0008 §6).
struct WaveNodeRun {
    trace: Trace,
    outcome: Result<()>,
}

/// Runs the rendered waves of an automatic SQL run (ADR 0008 §1 to §7). Each wave runs its nodes
/// (concurrently when it has more than one node and the concurrency is above 1), merges their
/// records in plan order, then checks its conflict and failures. Nothing is published here: the
/// caller publishes the final variables once, after every wave succeeded. Returns the completed
/// nodes in plan order.
fn run_sql_waves(
    trace: &mut Trace,
    plan_nodes: &[PlanNode],
    wave_plan: &[Vec<usize>],
    rendered: Vec<RenderedWave>,
    adapter: &mut PostgresAdapter,
    context: &WaveContext<'_>,
) -> Result<Vec<RunNodeResult>> {
    let mut completed: Vec<(usize, RunNodeResult)> = Vec::with_capacity(plan_nodes.len());
    for (position, wave) in rendered.into_iter().enumerate() {
        if let Err(error) = context.cancel.check() {
            skip_waves(trace, plan_nodes, &wave_plan[position..], context.mode);
            return Err(error);
        }
        let RenderedWave { nodes, conflict } = wave;
        let runs = if context.concurrency > 1 && nodes.len() > 1 {
            run_wave_concurrently(trace, plan_nodes, &nodes, context)
        } else {
            run_wave_in_order(trace, plan_nodes, &nodes, adapter, context)
        };
        let mut failure = None;
        let mut unstarted = false;
        for ((index, prepared), run) in nodes.iter().zip(runs) {
            match run {
                Some(run) => {
                    trace.absorb(run.trace);
                    match run.outcome {
                        Ok(()) => completed.push((*index, sql_node_result(prepared))),
                        Err(error) => {
                            failure.get_or_insert(error);
                        }
                    }
                }
                None => {
                    unstarted = true;
                    trace.skip(std::slice::from_ref(&plan_nodes[*index]), context.mode);
                }
            }
        }
        // ADR 0008 §4 is checked before the node failures of §7. A node that never started can
        // only follow a halt (a failure, already recorded) or a cancellation.
        let error = conflict.or(failure).or_else(|| {
            unstarted.then(|| {
                context.cancel.check().err().unwrap_or_else(|| {
                    DowaiError::Execution("A SQL wave ended with unstarted nodes".to_owned())
                })
            })
        });
        if let Some(error) = error {
            skip_waves(trace, plan_nodes, &wave_plan[position + 1..], context.mode);
            return Err(error);
        }
    }
    completed.sort_by_key(|(index, _)| *index);
    Ok(completed.into_iter().map(|(_, result)| result).collect())
}

/// Sequential wave: nodes run in plan order on the run's connection. The first failure halts the
/// wave; the nodes after it stay unstarted (`None`).
fn run_wave_in_order(
    trace: &Trace,
    plan_nodes: &[PlanNode],
    nodes: &[(usize, PreparedNode)],
    adapter: &mut PostgresAdapter,
    context: &WaveContext<'_>,
) -> Vec<Option<WaveNodeRun>> {
    let mut runs: Vec<Option<WaveNodeRun>> = nodes.iter().map(|_| None).collect();
    for (slot, (index, prepared)) in nodes.iter().enumerate() {
        if context.cancel.is_cancelled() {
            break;
        }
        let mut node_trace = trace.fork();
        let outcome = run_wave_node(
            &mut node_trace,
            adapter,
            &plan_nodes[*index],
            prepared,
            context,
        );
        let failed = outcome.is_err();
        runs[slot] = Some(WaveNodeRun {
            trace: node_trace,
            outcome,
        });
        if failed {
            break;
        }
    }
    runs
}

/// Concurrent wave: up to `context.concurrency` nodes at once, each on its own PostgreSQL
/// connection (ADR 0008 §5). A failed node halts the claim of the nodes not yet started.
fn run_wave_concurrently(
    trace: &Trace,
    plan_nodes: &[PlanNode],
    nodes: &[(usize, PreparedNode)],
    context: &WaveContext<'_>,
) -> Vec<Option<WaveNodeRun>> {
    waves::run_concurrently(
        nodes.len(),
        context.concurrency,
        || context.cancel.is_cancelled(),
        |slot| {
            let (index, prepared) = &nodes[slot];
            let plan_node = &plan_nodes[*index];
            let mut node_trace = trace.fork();
            let outcome = match PostgresAdapter::connect(context.profile) {
                Ok(mut adapter) => {
                    run_wave_node(&mut node_trace, &mut adapter, plan_node, prepared, context)
                }
                Err(error) => {
                    let error = DowaiError::Connection(error.to_string());
                    record_unstarted_failure(&mut node_trace, plan_node, context, &error);
                    Err(error)
                }
            };
            let halt = outcome.is_err();
            (
                WaveNodeRun {
                    trace: node_trace,
                    outcome,
                },
                halt,
            )
        },
    )
}

/// Materializes one rendered node under the retry policy, recording into `node_trace`.
fn run_wave_node(
    node_trace: &mut Trace,
    adapter: &mut PostgresAdapter,
    plan_node: &PlanNode,
    prepared: &PreparedNode,
    context: &WaveContext<'_>,
) -> Result<()> {
    let materialization = match prepared.materialization {
        Materialization::Table => PostgresMaterialization::Table,
        Materialization::View => PostgresMaterialization::View,
    };
    materialize_with_retry(
        adapter,
        context.profile,
        plan_node,
        model_hash(context.root, plan_node),
        context.mode,
        Some(node_trace),
        context.retry_policy,
        context.cancel,
        SqlWrite {
            relation: &prepared.relation,
            materialization,
            sql: &prepared.sql,
            allow_replace: context.allow_replace,
            full_refresh: context.full_refresh,
        },
    )
}

/// Journals a node that failed before its first attempt, such as a failed connection.
fn record_unstarted_failure(
    trace: &mut Trace,
    plan_node: &PlanNode,
    context: &WaveContext<'_>,
    error: &DowaiError,
) {
    let invocation_id = Uuid::new_v4().to_string();
    let started = Instant::now();
    trace.audit.node_started(
        &invocation_id,
        &plan_node.id,
        model_hash(context.root, plan_node),
    );
    trace
        .audit
        .node_finished(&invocation_id, &plan_node.id, Some(error));
    trace.node(
        invocation_id,
        plan_node,
        context.mode,
        node_failure_status(error),
        started,
        Some(error),
    );
}

/// Records every node of `waves` as SKIPPED, wave by wave, in plan order inside each wave.
fn skip_waves(
    trace: &mut Trace,
    plan_nodes: &[PlanNode],
    waves: &[Vec<usize>],
    mode: ExecutionMode,
) {
    for &index in waves.iter().flatten() {
        trace.skip(std::slice::from_ref(&plan_nodes[index]), mode);
    }
}

fn sql_node_result(prepared: &PreparedNode) -> RunNodeResult {
    RunNodeResult {
        id: prepared.id.clone(),
        relation: prepared.relation.clone(),
        materialization: match prepared.materialization {
            Materialization::Table => "table",
            Materialization::View => "view",
        }
        .to_owned(),
        status: "SUCCEEDED".to_owned(),
    }
}

/// Result record of a Python node: it materializes no relation.
fn python_node_result(node: &PlanNode) -> RunNodeResult {
    RunNodeResult {
        id: node.id.clone(),
        relation: node.id.clone(),
        materialization: "none".to_owned(),
        status: "SUCCEEDED".to_owned(),
    }
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
    let mut adapter = PostgresAdapter::connect(&postgres_profile(profile, &configuration)?)
        .map_err(|error| DowaiError::Connection(error.to_string()))?;
    adapter
        .test_connection()
        .map_err(|error| DowaiError::Connection(error.to_string()))?;
    Ok(profile_name)
}

/// Quoted relations of the tables declared by the project's sources (ENT-042), keyed by
/// `(source, table)`. Only these pairs are reachable through `env.source`.
/// Declared sources that read from a MySQL profile (ENT-116, D-121). Each profile is resolved
/// and checked by the same policy as the run's profile. A failure is kept and reported on the
/// first read of a source that uses that profile, so an unrelated run is not blocked.
fn declared_mysql_sources(
    project_dir: &Path,
    configuration: &Value,
) -> Result<executor::MysqlSources> {
    let mut mysql = executor::MysqlSources::default();
    for (source_name, source) in load_project(project_dir)?.sources {
        let Some(profile_name) = source.profile.clone() else {
            continue;
        };
        for (table_name, relation_name) in &source.tables {
            mysql.sources.insert(
                (source_name.clone(), table_name.clone()),
                (profile_name.clone(), relation_name.clone()),
            );
        }
        if mysql.profiles.contains_key(&profile_name) {
            continue;
        }
        let resolved = resolve_profile(project_dir, configuration, Some(&profile_name))
            .and_then(|(_, profile)| {
                if profile.profile_type != "mysql" {
                    return Err(DowaiError::Config(
                        "Source profile is not a MySQL profile".to_owned(),
                    ));
                }
                if profile.database != source.schema {
                    return Err(DowaiError::Config(
                        "MySQL source schema must match the profile database".to_owned(),
                    ));
                }
                mysql_profile(profile, configuration)
            })
            .map_err(|_| "No usable MySQL profile is available for this source");
        mysql.profiles.insert(profile_name, resolved);
    }
    Ok(mysql)
}

/// Builds the MySQL adapter profile with the effective `database` timeouts (ENT-116). The
/// password is never copied: only the name of its environment variable.
fn mysql_profile(profile: WorkspaceProfile, configuration: &Value) -> Result<MysqlProfile> {
    let database = &configuration["effective"]["database"];
    let mut adapter_profile = MysqlProfile::new(
        profile.host,
        profile.port,
        profile.user,
        profile.database,
        profile.password_env,
    );
    if profile.tls == dowai_core::ProfileTlsMode::Disabled {
        adapter_profile.set_tls_mode(TlsMode::Disabled);
    }
    if let Some(seconds) = database["connect_timeout_seconds"].as_u64() {
        adapter_profile.set_connect_timeout(Duration::from_secs(seconds));
    }
    if let Some(seconds) = database["query_timeout_seconds"].as_u64() {
        adapter_profile.set_query_timeout(Some(Duration::from_secs(seconds)));
    }
    Ok(adapter_profile)
}

fn declared_source_relations(project_dir: &Path) -> Result<BTreeMap<(String, String), String>> {
    let mut relations = BTreeMap::new();
    for (source_name, source) in load_project(project_dir)?.sources {
        // Sources with their own profile are read by `declared_mysql_sources` (ENT-116).
        if source.profile.is_some() {
            continue;
        }
        let schema = quote_identifier(&source.schema)
            .map_err(|error| DowaiError::Model(error.to_string()))?;
        for (table_name, relation_name) in source.tables {
            let table = quote_identifier(&relation_name)
                .map_err(|error| DowaiError::Model(error.to_string()))?;
            relations.insert(
                (source_name.clone(), table_name),
                format!("{schema}.{table}"),
            );
        }
    }
    Ok(relations)
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

fn is_sql_path(path: &str) -> bool {
    Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("sql"))
}

/// Renders one SQL node and refuses an empty statement or variables the store rejects. Shared by
/// the SQL nodes of a run and by nested `env.run("x.sql")` calls (ENT-114).
pub(crate) fn render_checked(
    project_dir: &Path,
    node: &PlanNode,
    variables: &BTreeMap<String, Value>,
    template_options: &TemplateOptions,
    relations: &BTreeMap<String, String>,
    store: &SessionStore,
) -> Result<(PreparedNode, BTreeMap<String, Value>)> {
    let (prepared, next_variables) =
        render_sql_node(project_dir, node, variables, template_options, relations)?;
    if prepared.sql.trim().is_empty() {
        return Err(DowaiError::Template(format!(
            "{} renders to an empty SQL statement",
            prepared.id
        )));
    }
    store.validate_variables(&next_variables)?;
    Ok((prepared, next_variables))
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

pub(crate) fn template_options(configuration: &Value) -> Result<TemplateOptions> {
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

/// Verifica modelos Python por AST, sem execução, quanto a escrita direta por bibliotecas
/// (drivers/ORMs e `to_sql`/`write_database`). Com `security.deny_direct_library_writes`
/// verdadeiro, recusa com `DOWAI_POLICY_DENIED`; caso contrário devolve um aviso por modelo.
fn direct_write_warnings(
    root: &Path,
    configuration: &Value,
    nodes: &[PlanNode],
) -> Result<Vec<String>> {
    let deny = configuration["effective"]["security"]["deny_direct_library_writes"]
        .as_bool()
        .ok_or_else(|| {
            DowaiError::Config("Effective deny_direct_library_writes is invalid".to_owned())
        })?;
    let mut warnings = Vec::new();
    for node in nodes.iter().filter(|node| is_python_path(&node.path)) {
        let source = read_planned_model(root, &node.path)?;
        let signals = static_direct_write_signals(&node.id, &source)?;
        if signals.is_empty() {
            continue;
        }
        let found = signals.join(", ");
        if deny {
            return Err(DowaiError::Policy(format!(
                "{}: direct library writes are denied by effective security policy ({found})",
                node.id
            )));
        }
        warnings.push(format!(
            "DOWAI_DIRECT_WRITE_DETECTED: {}: {found}; efeitos gravados diretamente por bibliotecas não são contabilizados pelo DOWAI (use env.write)",
            node.id
        ));
    }
    Ok(warnings)
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

/// Builds the adapter profile from the workspace profile and the effective `database` policy
/// (`connect_timeout_seconds`, `query_timeout_seconds`). The password is never copied: only the
/// name of the environment variable that holds it.
fn postgres_profile(profile: WorkspaceProfile, configuration: &Value) -> Result<PgProfile> {
    // `mysql` profiles are valid workspace profiles but only `env.write` destinations (ENT-034).
    if profile.profile_type != "postgres" {
        return Err(DowaiError::Config(format!(
            "This operation requires a PostgreSQL profile; a '{}' profile is only supported as an env.write destination",
            profile.profile_type
        )));
    }
    let database = &configuration["effective"]["database"];
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
    if let Some(seconds) = database["connect_timeout_seconds"].as_u64() {
        adapter_profile.set_connect_timeout(std::time::Duration::from_secs(seconds));
    }
    if let Some(seconds) = database["query_timeout_seconds"].as_u64() {
        adapter_profile.set_query_timeout(Some(std::time::Duration::from_secs(seconds)));
    }
    Ok(adapter_profile)
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
        | AdapterError::TransientFailure
        | AdapterError::UnsupportedRelationType
        | AdapterError::InvalidTransactionState
        | AdapterError::InvalidStreamLimit
        | AdapterError::InflightLimitExceeded
        | AdapterError::InvalidWriteSpec(_)
        | AdapterError::DuplicateMergeKey
        | AdapterError::SchemaMismatch
        | AdapterError::MultipleStatements => DowaiError::Execution(error.to_string()),
        AdapterError::Cancelled => DowaiError::cancelled(),
        AdapterError::MissingPassword
        | AdapterError::TlsConfigurationFailed
        | AdapterError::ConnectionFailed => DowaiError::Connection(error.to_string()),
    }
}

/// PostgreSQL side of a run (ENT-114). The connection, profile and SQL options are shared by the
/// SQL nodes of the plan and by nested `env.run("x.sql")` calls, so both reach the database
/// through the same materialization path (retry, staging, full refresh).
pub(crate) struct SqlExecution {
    /// The run's profile, or the reason it is unavailable. The reason fails only the SQL work
    /// that needs a database, so a Python-only run without a profile keeps working.
    profile: std::result::Result<PgProfile, String>,
    /// Connection kept between calls; opened by the first SQL work when not given.
    adapter: Option<PostgresAdapter>,
    template_options: TemplateOptions,
    allow_replace: bool,
    full_refresh: bool,
    retry_policy: RetryPolicy,
    cancel: CancellationToken,
}

impl SqlExecution {
    pub(crate) fn new(
        profile: std::result::Result<PgProfile, String>,
        adapter: Option<PostgresAdapter>,
        template_options: TemplateOptions,
        allow_replace: bool,
        full_refresh: bool,
        retry_policy: RetryPolicy,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            profile,
            adapter,
            template_options,
            allow_replace,
            full_refresh,
            retry_policy,
            cancel,
        }
    }

    /// Materializes one rendered node. `trace` receives the audit events and the attempt records
    /// of a static node; a nested call passes `None` and is recorded by the executor history.
    fn materialize(
        &mut self,
        node: &PlanNode,
        mode: ExecutionMode,
        hash: Option<String>,
        trace: Option<&mut Trace>,
        prepared: &PreparedNode,
    ) -> Result<()> {
        let profile = self
            .profile
            .as_ref()
            .map_err(|reason| DowaiError::Config(reason.clone()))?;
        let mut adapter = match self.adapter.take() {
            Some(adapter) => adapter,
            None => PostgresAdapter::connect(profile)
                .map_err(|error| DowaiError::Connection(error.to_string()))?,
        };
        let materialization = match prepared.materialization {
            Materialization::Table => PostgresMaterialization::Table,
            Materialization::View => PostgresMaterialization::View,
        };
        let outcome = materialize_with_retry(
            &mut adapter,
            profile,
            node,
            hash,
            mode,
            trace,
            self.retry_policy,
            &self.cancel,
            SqlWrite {
                relation: &prepared.relation,
                materialization,
                sql: &prepared.sql,
                allow_replace: self.allow_replace,
                full_refresh: self.full_refresh,
            },
        );
        self.adapter = Some(adapter);
        outcome
    }
}

/// One SQL materialization of a node, already rendered.
struct SqlWrite<'a> {
    relation: &'a str,
    materialization: PostgresMaterialization,
    sql: &'a str,
    allow_replace: bool,
    /// ENT-033: staging and swap instead of replacement in place.
    full_refresh: bool,
}

/// Runs one SQL node under the retry policy (ENT-037, ADR 0005). Each attempt has its own
/// invocation id, audit events and journal entry: a failed attempt is journaled as `FAILED`
/// and the last attempt carries the node's outcome. A lost connection is re-established before
/// the next attempt. Only transient failures are repeated.
#[allow(clippy::too_many_arguments)]
fn materialize_with_retry(
    adapter: &mut PostgresAdapter,
    profile: &PgProfile,
    node: &PlanNode,
    hash: Option<String>,
    mode: ExecutionMode,
    mut trace: Option<&mut Trace>,
    policy: RetryPolicy,
    cancel: &CancellationToken,
    write: SqlWrite<'_>,
) -> Result<()> {
    let outcome = run_with_retry(
        policy,
        |error: &AdapterError| error.is_transient(),
        |_attempt| {
            let invocation_id = Uuid::new_v4().to_string();
            let started = Instant::now();
            if let Some(trace) = trace.as_mut() {
                trace
                    .audit
                    .node_started(&invocation_id, &node.id, hash.clone());
            }
            let connected = if adapter.is_closed() {
                PostgresAdapter::connect(profile).map(|fresh| *adapter = fresh)
            } else {
                Ok(())
            };
            let result = connected.and_then(|()| {
                if write.full_refresh {
                    adapter.full_refresh_cancellable(
                        write.relation,
                        write.materialization,
                        write.sql,
                        &|| cancel.is_cancelled(),
                    )
                } else {
                    adapter.materialize_cancellable(
                        write.relation,
                        write.materialization,
                        write.sql,
                        write.allow_replace,
                        &|| cancel.is_cancelled(),
                    )
                }
            });
            match &result {
                Ok(()) => {
                    if let Some(trace) = trace.as_mut() {
                        trace
                            .audit
                            .external_write(&invocation_id, &node.id, write.relation);
                        trace.audit.node_finished(&invocation_id, &node.id, None);
                        trace.node(invocation_id, node, mode, "SUCCEEDED", started, None);
                    }
                }
                Err(error) => {
                    let mapped = map_execution_error(*error);
                    if let Some(trace) = trace.as_mut() {
                        trace
                            .audit
                            .node_finished(&invocation_id, &node.id, Some(&mapped));
                        trace.node(
                            invocation_id,
                            node,
                            mode,
                            node_failure_status(&mapped),
                            started,
                            Some(&mapped),
                        );
                    }
                }
            }
            result
        },
        |delay| wait_unless_cancelled(delay, cancel),
        || Uuid::new_v4().as_u128() as u64,
    );
    outcome.map_err(|error| {
        if cancel.is_cancelled() {
            DowaiError::cancelled()
        } else {
            map_execution_error(error)
        }
    })
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
                full_refresh: false,
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
                    full_refresh: false,
                },
            )
            .expect_err("external database writes must be opt-in");

            assert_eq!(error.code(), "DOWAI_POLICY_DENIED");
            assert!(error.message().contains("External writes are disabled"));
        }
    }

    #[test]
    fn full_refresh_without_confirmation_is_refused_before_any_run() {
        let request = RunRequest {
            targets: vec!["model.sql".to_owned()],
            mode: None,
            profile: None,
            session_id: None,
            variables: BTreeMap::new(),
            confirm_destructive: false,
            full_refresh: true,
        };
        let failure = run_postgres_detailed_with_cancel(
            std::path::Path::new("/nonexistent-dowai-full-refresh"),
            request,
            &CancellationToken::new(),
        )
        .expect_err("full refresh without --yes must be refused");
        assert!(failure.run_id.is_none(), "no run may start");
        assert_eq!(failure.error.code(), "DOWAI_POLICY_DENIED");
        assert_eq!(failure.error.exit_code(), 5);
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
                full_refresh: false,
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
                full_refresh: false,
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

    #[test]
    fn postgres_profile_applies_effective_database_timeouts_without_copying_secrets() {
        let workspace = dowai_core::WorkspaceProfile {
            profile_type: "postgres".to_owned(),
            host: "db.example.test".to_owned(),
            port: 5432,
            user: "dowai".to_owned(),
            database: "analytics".to_owned(),
            password_env: "DOWAI_ENT030_UNIT_PASSWORD".to_owned(),
            tls: dowai_core::ProfileTlsMode::Required,
        };
        let configuration = serde_json::json!({
            "effective": {
                "database": { "connect_timeout_seconds": 25, "query_timeout_seconds": 600 }
            }
        });
        let profile = super::postgres_profile(workspace, &configuration).expect("profile");
        assert_eq!(
            profile.connect_timeout(),
            std::time::Duration::from_secs(25)
        );
        assert_eq!(
            profile.query_timeout(),
            Some(std::time::Duration::from_secs(600))
        );
        assert_eq!(profile.tls_mode(), dowai_postgres::TlsMode::Required);
        assert!(!format!("{profile:?}").contains("secret"));
    }
}
