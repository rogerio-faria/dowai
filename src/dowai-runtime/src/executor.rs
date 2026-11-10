//! Supervised execution of Python invocations, including nested imperative
//! calls (`env.run`). Nested calls never touch the static plan: they are
//! resolved at run time with the existing planner and recorded in a flat
//! invocation history linked by `parent_invocation_id`.

use crate::python_runtime::{
    self, ChildCall, ChildOutcome, DatasetAnswer, DatasetColumn, HostReply, HostRequest,
    ModelInvocation,
};
use crate::{resolve_execution_mode, resolve_python_model_path, InvocationRecord};
use dowai_core::{
    plan, CancellationToken, DowaiError, ExecutionMode, PlanNode, Result, SessionStore,
};
use dowai_postgres::{DatasetCell, PgProfile, PostgresAdapter};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::{Duration, Instant};
use uuid::Uuid;

type Variables = BTreeMap<String, Value>;

pub(crate) const DATASET_UNAVAILABLE: &str = "DOWAI_DATASET_UNAVAILABLE";
pub(crate) const DATASET_NOT_SQL: &str = "DOWAI_DATASET_NOT_SQL";
pub(crate) const DATASET_UNDECLARED: &str = "DOWAI_DATASET_UNDECLARED";
pub(crate) const DATASET_LIMIT: &str = "DOWAI_DATASET_LIMIT";
/// Dataset requests served per run; a model cannot probe the database without bound.
const MAX_DATASET_REQUESTS: usize = 10_000;
/// Rows served per run through `iter_rows` (AC002): a bounded read, not a data export.
const MAX_DATASET_ROWS: u64 = 5_000_000;
/// Rows per batch (`iter_rows`), enforced on the host as well as in the worker.
const MAX_BATCH_ROWS: u32 = 10_000;

fn dataset_error(code: &'static str, message: &str) -> DowaiError {
    DowaiError::DatasetAccess {
        code,
        message: message.to_owned(),
    }
}

/// Read-only dataset access (`env.ref`) for a mixed run. Only the relations of SQL models
/// of the plan are reachable, and only through a model that declares them as dependencies,
/// so the predecessor has already committed when the request is served.
pub(crate) struct DatasetSource {
    profile: PgProfile,
    /// Every plan name (id, path, stem) to its quoted relation.
    relations: BTreeMap<String, String>,
    /// Quoted relations written by SQL models of the plan.
    sql_relations: BTreeSet<String>,
    adapter: Option<PostgresAdapter>,
    columns: BTreeMap<String, Vec<(String, String)>>,
    /// Requests served (any operation), bounded by `MAX_DATASET_REQUESTS`.
    requests: usize,
    rows_served: u64,
}

impl DatasetSource {
    pub(crate) fn new(
        profile: PgProfile,
        relations: BTreeMap<String, String>,
        sql_relations: BTreeSet<String>,
    ) -> Self {
        Self {
            profile,
            relations,
            sql_relations,
            adapter: None,
            columns: BTreeMap::new(),
            requests: 0,
            rows_served: 0,
        }
    }

    /// Authorize a dataset name for the model that asks: a SQL relation of the plan that the
    /// model declares as a dependency. Returns the quoted relation.
    fn authorize(&self, dataset: &str, depends_on: &[String]) -> Result<String> {
        let relation =
            self.relations.get(dataset).cloned().ok_or_else(|| {
                dataset_error(DATASET_UNAVAILABLE, "Dataset is not part of the plan")
            })?;
        if !self.sql_relations.contains(&relation) {
            return Err(dataset_error(
                DATASET_NOT_SQL,
                "Dataset is not produced by a SQL model",
            ));
        }
        let declared = depends_on
            .iter()
            .any(|id| self.relations.get(id) == Some(&relation));
        if !declared {
            return Err(dataset_error(
                DATASET_UNDECLARED,
                "Dataset was not declared as a dependency of this model",
            ));
        }
        Ok(relation)
    }

    fn adapter(&mut self) -> Result<&mut PostgresAdapter> {
        if self.adapter.is_none() {
            let adapter = PostgresAdapter::connect(&self.profile).map_err(|_| {
                dataset_error(DATASET_UNAVAILABLE, "Dataset source connection failed")
            })?;
            self.adapter = Some(adapter);
        }
        self.adapter
            .as_mut()
            .ok_or_else(|| dataset_error(DATASET_UNAVAILABLE, "Dataset source connection failed"))
    }

    /// Columns of a relation, described once per run.
    fn columns(&mut self, relation: &str) -> Result<Vec<(String, String)>> {
        if let Some(columns) = self.columns.get(relation) {
            return Ok(columns.clone());
        }
        let columns = self
            .adapter()?
            .describe_relation(relation)
            .map_err(|_| dataset_error(DATASET_UNAVAILABLE, "Dataset could not be described"))?;
        self.columns.insert(relation.to_owned(), columns.clone());
        Ok(columns)
    }

    fn schema(&mut self, dataset: &str, depends_on: &[String]) -> Result<DatasetAnswer> {
        let relation = self.authorize(dataset, depends_on)?;
        let columns = self.columns(&relation)?;
        Ok(DatasetAnswer::Columns(
            columns
                .into_iter()
                .map(|(name, data_type)| DatasetColumn { name, data_type })
                .collect(),
        ))
    }

    fn rows(
        &mut self,
        dataset: &str,
        depends_on: &[String],
        offset: Option<u64>,
        limit: Option<u32>,
    ) -> Result<DatasetAnswer> {
        let (Some(offset), Some(limit)) = (offset, limit) else {
            return Err(dataset_error(
                DATASET_UNAVAILABLE,
                "Dataset batch is incomplete",
            ));
        };
        if limit == 0 || limit > MAX_BATCH_ROWS {
            return Err(dataset_error(
                DATASET_LIMIT,
                "Dataset batch size is outside the allowed range",
            ));
        }
        if self.rows_served + u64::from(limit) > MAX_DATASET_ROWS {
            return Err(dataset_error(
                DATASET_LIMIT,
                "Dataset row limit reached for this run",
            ));
        }
        let relation = self.authorize(dataset, depends_on)?;
        let columns = self.columns(&relation)?;
        let rows = self
            .adapter()?
            .read_rows(&relation, &columns, offset, limit)
            .map_err(|_| dataset_error(DATASET_UNAVAILABLE, "Dataset rows could not be read"))?;
        self.rows_served += rows.len() as u64;
        let rows = rows
            .into_iter()
            .map(|row| row.into_iter().map(cell_to_json).collect())
            .collect();
        Ok(DatasetAnswer::Rows {
            columns: columns.into_iter().map(|(name, _)| name).collect(),
            rows,
        })
    }
}

/// JSON form of one cell. Non-finite floats travel as text, since JSON has no NaN.
fn cell_to_json(cell: DatasetCell) -> Value {
    match cell {
        DatasetCell::Null => Value::Null,
        DatasetCell::Bool(value) => Value::Bool(value),
        DatasetCell::Int(value) => Value::from(value),
        DatasetCell::Float(value) => serde_json::Number::from_f64(value)
            .map(Value::Number)
            .unwrap_or_else(|| Value::String(value.to_string())),
        DatasetCell::Text(value) => Value::String(value),
    }
}

pub(crate) struct Executor<'a> {
    pub root: &'a Path,
    pub models_dir: &'a Path,
    pub interpreter: &'a Path,
    pub timeout: Duration,
    pub max_call_depth: usize,
    pub max_invocations: usize,
    pub configuration: &'a Value,
    pub store: &'a SessionStore,
    pub cancel: CancellationToken,
    pub invocations: Vec<InvocationRecord>,
    /// Present only in mixed SQL + Python runs (AC002).
    pub(crate) datasets: Option<DatasetSource>,
    stack: Vec<String>,
    /// Every invocation attempt received, including rejected ones.
    attempts: usize,
}

impl<'a> Executor<'a> {
    pub(crate) fn new(
        root: &'a Path,
        models_dir: &'a Path,
        interpreter: &'a Path,
        timeout: Duration,
        configuration: &'a Value,
        store: &'a SessionStore,
        cancel: CancellationToken,
    ) -> Result<Self> {
        let limit = |name: &str| {
            configuration["effective"]["execution"][name]
                .as_u64()
                .and_then(|value| usize::try_from(value).ok())
                .ok_or_else(|| DowaiError::Config(format!("Effective execution.{name} is invalid")))
        };
        Ok(Self {
            root,
            models_dir,
            interpreter,
            timeout,
            max_call_depth: limit("max_call_depth")?,
            max_invocations: limit("max_invocations_per_run")?,
            configuration,
            store,
            cancel,
            invocations: Vec::new(),
            datasets: None,
            stack: Vec::new(),
            attempts: 0,
        })
    }

    /// Execute a node of the static plan as a root-level invocation.
    pub(crate) fn run_static(
        &mut self,
        node: &PlanNode,
        mode: ExecutionMode,
        variables: Variables,
        root_invocation_id: &str,
        on_start: &mut dyn FnMut(&str),
    ) -> Result<Variables> {
        self.record(
            "static",
            root_invocation_id,
            &node.id,
            mode,
            |this, invocation_id| {
                on_start(invocation_id);
                this.execute_node(
                    &node.path,
                    &node.depends_on,
                    mode,
                    variables,
                    invocation_id,
                    None,
                )
            },
        )
        .1
    }

    /// Count one invocation attempt; fail once the per-run budget is exceeded.
    /// Rejected attempts count too, so a model cannot probe for free.
    fn admit(&mut self) -> Result<()> {
        self.attempts += 1;
        if self.attempts > self.max_invocations {
            return Err(DowaiError::Execution(format!(
                "Invocation limit reached: execution.max_invocations_per_run={}",
                self.max_invocations
            )));
        }
        Ok(())
    }

    /// Replace the recorded target once it resolved to a canonical model id.
    fn set_target(&mut self, invocation_id: &str, target: &str) {
        if let Some(entry) = self
            .invocations
            .iter_mut()
            .rev()
            .find(|entry| entry.invocation_id == invocation_id)
        {
            entry.target = printable(target);
        }
    }

    /// Run `body` inside a new invocation record. Returns the invocation id
    /// together with the outcome; the record is finalized in every case.
    fn record<T>(
        &mut self,
        call_type: &str,
        parent_invocation_id: &str,
        target: &str,
        mode: ExecutionMode,
        body: impl FnOnce(&mut Self, &str) -> Result<T>,
    ) -> (String, Result<T>) {
        let invocation_id = Uuid::new_v4().to_string();
        if let Err(error) = self.admit() {
            return (invocation_id, Err(error));
        }
        let index = self.invocations.len();
        self.invocations.push(InvocationRecord {
            invocation_id: invocation_id.clone(),
            parent_invocation_id: parent_invocation_id.to_owned(),
            call_type: call_type.to_owned(),
            target: printable(target),
            mode: mode.as_str().to_owned(),
            status: "RUNNING".to_owned(),
            duration_ms: 0,
            error_code: None,
        });
        let started = Instant::now();
        let outcome = body(self, &invocation_id);
        let entry = &mut self.invocations[index];
        entry.duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        match outcome {
            Ok(value) => {
                entry.status = "SUCCEEDED".to_owned();
                (invocation_id, Ok(value))
            }
            Err(error) => {
                entry.status = if error.is_cancelled() {
                    "CANCELLED"
                } else {
                    "FAILED"
                }
                .to_owned();
                entry.error_code = Some(error.code().to_owned());
                (invocation_id, Err(error))
            }
        }
    }

    /// Run one Python model in a worker process, serving its nested calls.
    fn execute_node(
        &mut self,
        model_id: &str,
        depends_on: &[String],
        mode: ExecutionMode,
        variables: Variables,
        invocation_id: &str,
        parent_deadline: Option<Instant>,
    ) -> Result<Variables> {
        self.cancel.check()?;
        if self.stack.iter().any(|entry| entry == model_id) {
            return Err(DowaiError::Cycle(format!(
                "Imperative call cycle detected: {}",
                self.chain_with(model_id)
            )));
        }
        if self.stack.len() > self.max_call_depth {
            return Err(DowaiError::Execution(format!(
                "Maximum call depth execution.max_call_depth={} exceeded: {}",
                self.max_call_depth,
                self.chain_with(model_id)
            )));
        }
        // `models_dir` is project-relative or absolute; both join correctly.
        let model_path = resolve_python_model_path(self.root, self.models_dir, model_id)?;
        let mut deadline = Instant::now() + self.timeout;
        if let Some(cap) = parent_deadline {
            deadline = deadline.min(cap);
        }
        let (root, interpreter) = (self.root, self.interpreter);
        let cancel = self.cancel.clone();
        self.stack.push(model_id.to_owned());
        let result = python_runtime::execute_model(
            &ModelInvocation {
                project_root: root,
                interpreter,
                deadline,
                model_path: &model_path,
                dependencies: depends_on,
                mode: mode.as_str(),
                cancel: &cancel,
            },
            &variables,
            &mut |request| self.handle_request(request, invocation_id, deadline, depends_on),
        );
        self.stack.pop();
        let variables = result?;
        self.store.validate_variables(&variables)?;
        Ok(variables)
    }

    fn chain_with(&self, next: &str) -> String {
        self.stack
            .iter()
            .map(String::as_str)
            .chain(std::iter::once(next))
            .collect::<Vec<_>>()
            .join(" -> ")
    }

    fn handle_request(
        &mut self,
        request: HostRequest,
        parent_invocation_id: &str,
        deadline: Instant,
        depends_on: &[String],
    ) -> HostReply {
        match request {
            HostRequest::Call(call) => {
                HostReply::Call(self.handle_call(call, parent_invocation_id, deadline))
            }
            HostRequest::Dataset {
                dataset,
                operation,
                offset,
                limit,
            } => HostReply::Dataset(
                self.dataset_request(&dataset, &operation, offset, limit, depends_on),
            ),
        }
    }

    fn dataset_request(
        &mut self,
        dataset: &str,
        operation: &str,
        offset: Option<u64>,
        limit: Option<u32>,
        depends_on: &[String],
    ) -> Result<DatasetAnswer> {
        self.cancel.check()?;
        let source = self.datasets.as_mut().ok_or_else(|| {
            dataset_error(
                DATASET_UNAVAILABLE,
                "Datasets are only available in runs that include SQL models",
            )
        })?;
        if source.requests >= MAX_DATASET_REQUESTS {
            return Err(dataset_error(
                DATASET_LIMIT,
                "Dataset request limit reached for this run",
            ));
        }
        // Every request counts, including unknown operations (bounded per run).
        source.requests += 1;
        match operation {
            "schema" => source.schema(dataset, depends_on),
            "rows" => source.rows(dataset, depends_on, offset, limit),
            _ => Err(DowaiError::NotImplemented(
                "This dataset operation is not available yet".to_owned(),
            )),
        }
    }

    fn handle_call(
        &mut self,
        call: ChildCall,
        parent_invocation_id: &str,
        deadline: Instant,
    ) -> ChildOutcome {
        let requested = call.mode.as_deref().unwrap_or("automatic");
        let mode = match requested {
            "automatic" => ExecutionMode::Automatic,
            "manual" => ExecutionMode::Manual,
            _ => {
                let (invocation_id, result) = self.record(
                    "imperative",
                    parent_invocation_id,
                    INVALID_TARGET,
                    ExecutionMode::Automatic,
                    |_, _| {
                        Err::<Variables, _>(DowaiError::Config(
                            "env.run mode must be 'automatic' or 'manual'".to_owned(),
                        ))
                    },
                );
                return ChildOutcome {
                    invocation_id,
                    result,
                };
            }
        };
        let target = call.target;
        let variables = call.variables;
        // The worker-supplied text is never recorded: the record starts as
        // invalid and is upgraded to the canonical id once resolved.
        let (invocation_id, result) = self.record(
            "imperative",
            parent_invocation_id,
            INVALID_TARGET,
            mode,
            |this, invocation_id| this.run_child(&target, mode, variables, invocation_id, deadline),
        );
        ChildOutcome {
            invocation_id,
            result,
        }
    }

    fn run_child(
        &mut self,
        target: &str,
        mode: ExecutionMode,
        variables: Variables,
        invocation_id: &str,
        deadline: Instant,
    ) -> Result<Variables> {
        self.store.validate_variables(&variables)?;
        resolve_execution_mode(self.configuration, Some(mode))?;
        let targets = [target.to_owned()];
        // The planner validates confinement to the models directory.
        let single = plan(self.root, &targets, ExecutionMode::Manual)?;
        let [node] = single.nodes.as_slice() else {
            return Err(DowaiError::Model(
                "env.run() accepts exactly one model target".to_owned(),
            ));
        };
        self.set_target(invocation_id, &node.id);
        let mut predecessors = Vec::new();
        if mode == ExecutionMode::Automatic {
            let full = plan(self.root, &targets, ExecutionMode::Automatic)?;
            predecessors = full
                .nodes
                .into_iter()
                .filter(|candidate| candidate.id != node.id)
                .collect();
        }
        for candidate in std::iter::once(node).chain(predecessors.iter()) {
            if !is_python(&candidate.path) {
                return Err(DowaiError::NotImplemented(format!(
                    "env.run() of SQL models is not available yet: {}",
                    candidate.path
                )));
            }
        }
        let mut current = variables;
        for predecessor in &predecessors {
            let input = current.clone();
            let outcome = self
                .record(
                    "dependency",
                    invocation_id,
                    &predecessor.id,
                    mode,
                    |this, dependency_id| {
                        this.execute_node(
                            &predecessor.path,
                            &predecessor.depends_on,
                            mode,
                            input,
                            dependency_id,
                            Some(deadline),
                        )
                    },
                )
                .1;
            // A failed predecessor fails the call; both records stay FAILED.
            current = outcome?;
        }
        self.execute_node(
            &node.path,
            &node.depends_on,
            mode,
            current,
            invocation_id,
            Some(deadline),
        )
    }
}

const INVALID_TARGET: &str = "<invalid>";

/// Recorded text: no control characters, bounded length.
fn printable(text: &str) -> String {
    text.chars()
        .filter(|character| !character.is_control())
        .take(256)
        .collect()
}

fn is_python(path: &str) -> bool {
    Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("py"))
}
