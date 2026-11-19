//! Supervised execution of Python invocations, including nested imperative
//! calls (`env.run`). Nested calls never touch the static plan: they are
//! resolved at run time with the existing planner and recorded in a flat
//! invocation history linked by `parent_invocation_id`.

use crate::database::{
    parse_quoted_relation, DatabaseAdapter, DatabaseError, DatabaseProfile, NativePage,
};
use crate::dataset_query::{self, FilterRequest};
use crate::external_write::WriteRequest;
use crate::python_runtime::{
    self, CallDetails, ChildCall, ChildOutcome, DatasetAnswer, DatasetColumn, HostReply,
    HostRequest, ModelInvocation,
};
use crate::resources;
use crate::{
    build_relations, build_relations_for_database, enforce_profile_policy, prepare_postgres_plan,
    render_checked, resolve_execution_mode, resolve_python_model_path, template_options,
    InvocationRecord, SqlExecution,
};
use dowai_core::state::NodeStatus;
use dowai_core::{
    admin_config_path, load_workspace_profile, plan, user_config_path,
    validate_project_with_configuration, CancellationToken, DowaiError, ExecutionMode, PlanNode,
    Result, SessionStore,
};
use dowai_mysql::{quote_identifier as mysql_quote, MysqlAdapter};
use dowai_postgres::{DatasetCell, PgProfile, PostgresAdapter};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use uuid::Uuid;

type Variables = BTreeMap<String, Value>;

pub(crate) const DATASET_UNAVAILABLE: &str = "DOWAI_DATASET_UNAVAILABLE";
pub(crate) const DATASET_NOT_SQL: &str = "DOWAI_DATASET_NOT_SQL";
pub(crate) const DATASET_UNDECLARED: &str = "DOWAI_DATASET_UNDECLARED";
pub(crate) const DATASET_LIMIT: &str = "DOWAI_DATASET_LIMIT";
/// `env.source(name, table)` for a source that the project does not declare (ENT-042).
pub(crate) const DATASET_SOURCE_UNDECLARED: &str = "DOWAI_DATASET_SOURCE_UNDECLARED";
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

fn native_dataset_error(error: DatabaseError) -> DowaiError {
    if matches!(error, DatabaseError::Cancelled) {
        DowaiError::cancelled()
    } else {
        dataset_error(DATASET_UNAVAILABLE, &error.into_dowai().message())
    }
}

/// What a worker asks to read: a model (`env.ref`) or a project-declared source
/// (`env.source`, ENT-042).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DatasetRef {
    Model(String),
    Source {
        source: String,
        table: String,
        profile: Option<String>,
    },
}

pub(crate) type SourceDefinitions = BTreeMap<(String, String), (String, String, Option<String>)>;

struct ResolvedSource {
    profile: String,
    table: String,
    schema: String,
    relation: String,
    is_postgres: bool,
    is_native: bool,
    is_model: bool,
}

/// Read-only dataset access (`env.ref`, `env.source`) for a mixed run. Model relations are
/// reachable only when they are SQL relations of the plan and the requesting model declares
/// them as dependencies, so the predecessor has already committed when the request is served.
/// Declared sources are reachable by `(source, table)` pair, independently of the plan.
pub(crate) struct DatasetSource {
    /// Resolved from configuration. An `Err` is reported on the first read (ENT-111).
    profile: std::result::Result<PgProfile, &'static str>,
    /// Every plan name (id, path, stem) to its quoted relation.
    relations: BTreeMap<String, String>,
    /// Quoted relations written by SQL models of the plan.
    sql_relations: BTreeSet<String>,
    /// Tables declared by the project, `(source, table)` to their quoted relation.
    sources: SourceDefinitions,
    project_dir: PathBuf,
    configuration: Value,
    default_profile: Option<String>,
    adapter: Option<PostgresAdapter>,
    columns: BTreeMap<String, Vec<(String, String)>>,
    /// Requests served (any operation), bounded by `MAX_DATASET_REQUESTS`.
    requests: usize,
    rows_served: u64,
    /// Fallback warnings (ENT-041), each recorded once per run.
    warnings: Vec<String>,
    /// `mysql` profiles of declared sources (ENT-116), resolved by name. An `Err` is reported on
    /// the first read of a source that uses it.
    database_profiles: BTreeMap<String, std::result::Result<DatabaseProfile, String>>,
    mysql_adapters: BTreeMap<String, MysqlAdapter>,
    postgres_adapters: BTreeMap<String, PostgresAdapter>,
    native_adapters: BTreeMap<String, DatabaseAdapter>,
    cancel: CancellationToken,
}

impl DatasetSource {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        profile: std::result::Result<PgProfile, &'static str>,
        relations: BTreeMap<String, String>,
        sql_relations: BTreeSet<String>,
        sources: SourceDefinitions,
        project_dir: &Path,
        configuration: &Value,
        default_profile: Option<String>,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            profile,
            relations,
            sql_relations,
            sources,
            project_dir: project_dir.to_path_buf(),
            configuration: configuration.clone(),
            default_profile,
            adapter: None,
            columns: BTreeMap::new(),
            requests: 0,
            rows_served: 0,
            warnings: Vec::new(),
            database_profiles: BTreeMap::new(),
            mysql_adapters: BTreeMap::new(),
            postgres_adapters: BTreeMap::new(),
            native_adapters: BTreeMap::new(),
            cancel,
        }
    }

    fn database_profile(&mut self, profile: &str) -> Result<DatabaseProfile> {
        if !self.database_profiles.contains_key(profile) {
            let resolved =
                crate::resolve_profile(&self.project_dir, &self.configuration, Some(profile))
                    .and_then(|(_, workspace_profile)| {
                        DatabaseProfile::from_workspace(workspace_profile, &self.configuration)
                    })
                    .map_err(|error| error.message());
            self.database_profiles.insert(profile.to_owned(), resolved);
        }
        self.database_profiles
            .get(profile)
            .ok_or_else(|| {
                dataset_error(
                    DATASET_UNAVAILABLE,
                    "Dataset source profile is not available",
                )
            })?
            .clone()
            .map_err(|reason| dataset_error(DATASET_UNAVAILABLE, &reason))
    }

    fn resolve_source(&mut self, target: &DatasetRef) -> Result<Option<ResolvedSource>> {
        let DatasetRef::Source {
            source,
            table,
            profile,
        } = target
        else {
            return Ok(None);
        };
        let (schema, table_name, declared_profile) = self
            .sources
            .get(&(source.clone(), table.clone()))
            .cloned()
            .ok_or_else(|| {
                dataset_error(
                    DATASET_SOURCE_UNDECLARED,
                    "Source table is not declared in the project",
                )
            })?;
        let profile_name = profile
            .as_deref()
            .or(declared_profile.as_deref())
            .or(self.default_profile.as_deref())
            .ok_or_else(|| {
                dataset_error(
                    DATASET_UNAVAILABLE,
                    "No database profile is selected for this source",
                )
            })?
            .to_owned();
        let database = self.database_profile(&profile_name)?;
        if !database.is_postgres() && !database.is_mysql() && !database.is_native() {
            return Err(dataset_error(
                DATASET_UNAVAILABLE,
                "Python source reads are not yet supported for this database profile",
            ));
        }
        if database.is_mysql() && schema != database.database_name() {
            return Err(dataset_error(
                DATASET_UNAVAILABLE,
                "A MySQL source schema must match the selected profile database",
            ));
        }
        let relation = database
            .relation(&schema, &table_name)
            .map_err(|_| dataset_error(DATASET_UNAVAILABLE, "Source relation is invalid"))?;
        Ok(Some(ResolvedSource {
            profile: profile_name,
            table: table_name,
            schema,
            relation,
            is_postgres: database.is_postgres(),
            is_native: database.is_native(),
            is_model: false,
        }))
    }

    fn native_model_source(&mut self, relation: &str) -> Result<Option<ResolvedSource>> {
        let Some(profile_name) = self.default_profile.clone() else {
            return Ok(None);
        };
        let database = self.database_profile(&profile_name)?;
        let (opening, closing, sql) = match database {
            DatabaseProfile::Oracle { .. } => (
                '"',
                '"',
                "SELECT SYS_CONTEXT('USERENV','CURRENT_SCHEMA') AS \"DOWAI_SCHEMA\" FROM DUAL",
            ),
            DatabaseProfile::SqlServer { .. } => {
                ('[', ']', "SELECT SCHEMA_NAME() AS [DOWAI_SCHEMA]")
            }
            _ => return Ok(None),
        };
        let parts = parse_quoted_relation(relation, opening, closing)
            .map_err(|_| dataset_error(DATASET_UNAVAILABLE, "Dataset relation is invalid"))?;
        let (schema, table) = match parts.as_slice() {
            [schema, table] => (schema.clone(), table.clone()),
            [table] => {
                let cancel = self.cancel.clone();
                let rows = self
                    .native_adapter(&profile_name)?
                    .native_query(sql, &[], &|| cancel.is_cancelled())
                    .map_err(native_dataset_error)?;
                let schema = rows
                    .first()
                    .and_then(|row| row["DOWAI_SCHEMA"].as_str())
                    .ok_or_else(|| {
                        dataset_error(DATASET_UNAVAILABLE, "Dataset schema is unavailable")
                    })?;
                (schema.to_owned(), table.clone())
            }
            _ => {
                return Err(dataset_error(
                    DATASET_UNAVAILABLE,
                    "Dataset relation is invalid",
                ))
            }
        };
        Ok(Some(ResolvedSource {
            profile: profile_name,
            table,
            schema,
            relation: relation.to_owned(),
            is_postgres: false,
            is_native: true,
            is_model: true,
        }))
    }

    fn native_adapter(&mut self, profile: &str) -> Result<&mut DatabaseAdapter> {
        if !self.native_adapters.contains_key(profile) {
            let resolved = self.database_profile(profile)?;
            let adapter = DatabaseAdapter::connect(&resolved).map_err(native_dataset_error)?;
            self.native_adapters.insert(profile.to_owned(), adapter);
        }
        self.native_adapters
            .get_mut(profile)
            .ok_or_else(|| dataset_error(DATASET_UNAVAILABLE, "Dataset source connection failed"))
    }

    fn native_columns(&mut self, source: &ResolvedSource) -> Result<Vec<(String, String)>> {
        let key = format!("native:{}:{}", source.profile, source.relation);
        if let Some(columns) = self.columns.get(&key) {
            return Ok(columns.clone());
        }
        let cancel = self.cancel.clone();
        let columns = self
            .native_adapter(&source.profile)?
            .native_columns(&source.schema, &source.table, &|| cancel.is_cancelled())
            .map_err(native_dataset_error)?;
        if columns.is_empty() {
            return Err(dataset_error(
                DATASET_UNAVAILABLE,
                "Dataset could not be described",
            ));
        }
        self.columns.insert(key, columns.clone());
        Ok(columns)
    }

    fn native_rows(
        &mut self,
        source: &ResolvedSource,
        select: Option<&[String]>,
        filters: &[FilterRequest],
        offset: u64,
        limit: u32,
    ) -> Result<DatasetAnswer> {
        if !filters.is_empty() {
            return Err(dataset_error(
                dataset_query::DATASET_FILTER_UNSUPPORTED,
                "Filters are not supported for Oracle or SQL Server sources",
            ));
        }
        let columns = self.native_columns(source)?;
        let plan = dataset_query::plan(&columns, select, &[])?;
        let order_columns: Vec<String> = columns.into_iter().map(|(name, _)| name).collect();
        let selected: Vec<String> = plan.fetch.into_iter().map(|(name, _)| name).collect();
        let cancel = self.cancel.clone();
        let rows = self
            .native_adapter(&source.profile)?
            .native_rows(
                &source.schema,
                &source.table,
                NativePage {
                    columns: &selected,
                    order_columns: &order_columns,
                    offset,
                    limit,
                    allow_unkeyed: source.is_model,
                },
                &|| cancel.is_cancelled(),
            )
            .map_err(native_dataset_error)?;
        self.rows_served += rows.len() as u64;
        Ok(DatasetAnswer::Rows {
            columns: selected,
            rows,
            residual: plan.residual,
        })
    }

    /// MySQL adapter of a profile, connected on first use and kept for the run.
    fn mysql_adapter(&mut self, profile: &str) -> Result<&mut MysqlAdapter> {
        if !self.mysql_adapters.contains_key(profile) {
            let resolved = self.database_profile(profile)?;
            let DatabaseProfile::Mysql {
                profile: adapter_profile,
                ..
            } = resolved
            else {
                return Err(dataset_error(
                    DATASET_UNAVAILABLE,
                    "The selected source profile is not MySQL",
                ));
            };
            let adapter = MysqlAdapter::connect(&adapter_profile).map_err(|_| {
                dataset_error(DATASET_UNAVAILABLE, "Dataset source connection failed")
            })?;
            self.mysql_adapters.insert(profile.to_owned(), adapter);
        }
        self.mysql_adapters
            .get_mut(profile)
            .ok_or_else(|| dataset_error(DATASET_UNAVAILABLE, "Dataset source connection failed"))
    }

    /// Columns of a MySQL table, described once per run.
    fn mysql_columns(&mut self, profile: &str, table: &str) -> Result<Vec<(String, String)>> {
        let key = format!("mysql:{profile}.{table}");
        if let Some(columns) = self.columns.get(&key) {
            return Ok(columns.clone());
        }
        let columns: Vec<(String, String)> = self
            .mysql_adapter(profile)?
            .introspect_table(table)
            .map_err(|_| dataset_error(DATASET_UNAVAILABLE, "Dataset could not be described"))?
            .into_iter()
            .map(|column| (column.name, column.data_type))
            .collect();
        self.columns.insert(key, columns.clone());
        Ok(columns)
    }

    fn postgres_adapter(&mut self, profile: &str) -> Result<&mut PostgresAdapter> {
        if !self.postgres_adapters.contains_key(profile) {
            let resolved = self.database_profile(profile)?;
            let DatabaseProfile::Postgres {
                profile: adapter_profile,
                ..
            } = resolved
            else {
                return Err(dataset_error(
                    DATASET_UNAVAILABLE,
                    "The selected source profile is not PostgreSQL",
                ));
            };
            let adapter = PostgresAdapter::connect(&adapter_profile).map_err(|_| {
                dataset_error(DATASET_UNAVAILABLE, "Dataset source connection failed")
            })?;
            self.postgres_adapters.insert(profile.to_owned(), adapter);
        }
        self.postgres_adapters
            .get_mut(profile)
            .ok_or_else(|| dataset_error(DATASET_UNAVAILABLE, "Dataset source connection failed"))
    }

    fn postgres_columns(&mut self, profile: &str, relation: &str) -> Result<Vec<(String, String)>> {
        let key = format!("postgres:{profile}:{relation}");
        if let Some(columns) = self.columns.get(&key) {
            return Ok(columns.clone());
        }
        let columns = self
            .postgres_adapter(profile)?
            .describe_relation(relation)
            .map_err(|_| dataset_error(DATASET_UNAVAILABLE, "Dataset could not be described"))?;
        self.columns.insert(key, columns.clone());
        Ok(columns)
    }

    fn postgres_rows(
        &mut self,
        profile: &str,
        relation: &str,
        select: Option<&[String]>,
        filters: &[FilterRequest],
        offset: u64,
        limit: u32,
    ) -> Result<DatasetAnswer> {
        let all_columns = self.postgres_columns(profile, relation)?;
        let plan = dataset_query::plan(&all_columns, select, filters)?;
        for warning in plan.warnings {
            if !self.warnings.contains(&warning) {
                self.warnings.push(warning);
            }
        }
        let rows = self
            .postgres_adapter(profile)?
            .read_rows_filtered(relation, &plan.fetch, &plan.pushed, offset, limit)
            .map_err(|_| dataset_error(DATASET_UNAVAILABLE, "Dataset rows could not be read"))?;
        self.rows_served += rows.len() as u64;
        Ok(DatasetAnswer::Rows {
            columns: plan.fetch.into_iter().map(|(name, _)| name).collect(),
            rows: rows
                .into_iter()
                .map(|row| row.into_iter().map(cell_to_json).collect())
                .collect(),
            residual: plan.residual,
        })
    }

    /// One window of a MySQL source, in primary-key order. Filters are not pushed down or
    /// applied by the host yet: a request with filters is refused (ENT-116 limitation).
    fn mysql_rows(
        &mut self,
        profile: &str,
        table: &str,
        select: Option<&[String]>,
        filters: &[FilterRequest],
        offset: u64,
        limit: u32,
    ) -> Result<DatasetAnswer> {
        if !filters.is_empty() {
            return Err(dataset_error(
                dataset_query::DATASET_FILTER_UNSUPPORTED,
                "Filters are not supported for MySQL sources",
            ));
        }
        let all_columns = self.mysql_columns(profile, table)?;
        let plan = dataset_query::plan(&all_columns, select, &[])?;
        let key_columns = self
            .mysql_adapter(profile)?
            .primary_key(table)
            .map_err(|_| dataset_error(DATASET_UNAVAILABLE, "Dataset rows could not be read"))?;
        if key_columns.is_empty() {
            return Err(dataset_error(
                DATASET_UNAVAILABLE,
                "MySQL source table needs a primary key for ordered batches",
            ));
        }
        let quote = |name: &str| {
            mysql_quote(name)
                .map_err(|_| dataset_error(DATASET_UNAVAILABLE, "Dataset identifier is invalid"))
        };
        let selected = plan
            .fetch
            .iter()
            .map(|(name, _)| quote(name))
            .collect::<Result<Vec<_>>>()?
            .join(", ");
        let keys = key_columns
            .iter()
            .map(|name| quote(name))
            .collect::<Result<Vec<_>>>()?
            .join(", ");
        let sql = format!(
            "SELECT {selected} FROM {} ORDER BY {keys} LIMIT ? OFFSET ?",
            quote(table)?
        );
        let params = [
            dowai_mysql::Value::UInt(u64::from(limit)),
            dowai_mysql::Value::UInt(offset),
        ];
        let rows = self
            .mysql_adapter(profile)?
            .query_rows(&sql, &params)
            .map_err(|_| dataset_error(DATASET_UNAVAILABLE, "Dataset rows could not be read"))?;
        self.rows_served += rows.len() as u64;
        let rows = rows
            .into_iter()
            .map(|row| row.into_iter().map(cell_to_json).collect())
            .collect();
        Ok(DatasetAnswer::Rows {
            columns: plan.fetch.into_iter().map(|(name, _)| name).collect(),
            rows,
            residual: plan.residual,
        })
    }

    /// Warnings raised by dataset reads of the run, drained once when the run finishes.
    pub(crate) fn take_warnings(&mut self) -> Vec<String> {
        std::mem::take(&mut self.warnings)
    }

    /// Authorize a dataset for the model that asks. A model must be a SQL relation of the plan
    /// that the model declares as a dependency; a source must be declared by the project.
    /// Returns the quoted relation.
    fn authorize(&self, target: &DatasetRef, depends_on: &[String]) -> Result<String> {
        let DatasetRef::Model(dataset) = target else {
            return Err(dataset_error(
                DATASET_SOURCE_UNDECLARED,
                "Source table is not declared in the project",
            ));
        };
        let relation =
            self.relations.get(dataset).cloned().ok_or_else(|| {
                dataset_error(DATASET_UNAVAILABLE, "Dataset is not part of the plan")
            })?;
        // A Python-only plan has no SQL model: no model relation can be served (ENT-111).
        if self.sql_relations.is_empty() {
            return Err(dataset_error(
                DATASET_UNAVAILABLE,
                "Model datasets require a SQL model in the run",
            ));
        }
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
            let profile = self
                .profile
                .as_ref()
                .map_err(|reason| dataset_error(DATASET_UNAVAILABLE, reason))?;
            let adapter = PostgresAdapter::connect(profile).map_err(|_| {
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

    fn schema(&mut self, target: &DatasetRef, depends_on: &[String]) -> Result<DatasetAnswer> {
        if let Some(source) = self.resolve_source(target)? {
            let columns = if source.is_postgres {
                self.postgres_columns(&source.profile, &source.relation)?
            } else if source.is_native {
                self.native_columns(&source)?
            } else {
                self.mysql_columns(&source.profile, &source.table)?
            };
            return Ok(DatasetAnswer::Columns(
                columns
                    .into_iter()
                    .map(|(name, data_type)| DatasetColumn { name, data_type })
                    .collect(),
            ));
        }
        let relation = self.authorize(target, depends_on)?;
        if let Some(source) = self.native_model_source(&relation)? {
            let columns = self.native_columns(&source)?;
            return Ok(DatasetAnswer::Columns(
                columns
                    .into_iter()
                    .map(|(name, data_type)| DatasetColumn { name, data_type })
                    .collect(),
            ));
        }
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
        target: &DatasetRef,
        depends_on: &[String],
        request: RowsRequest<'_>,
    ) -> Result<DatasetAnswer> {
        let RowsRequest {
            offset,
            limit,
            select,
            filters,
        } = request;
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
        if let Some(source) = self.resolve_source(target)? {
            return if source.is_postgres {
                self.postgres_rows(
                    &source.profile,
                    &source.relation,
                    select,
                    filters,
                    offset,
                    limit,
                )
            } else if source.is_native {
                self.native_rows(&source, select, filters, offset, limit)
            } else {
                self.mysql_rows(
                    &source.profile,
                    &source.table,
                    select,
                    filters,
                    offset,
                    limit,
                )
            };
        }
        let relation = self.authorize(target, depends_on)?;
        if let Some(source) = self.native_model_source(&relation)? {
            return self.native_rows(&source, select, filters, offset, limit);
        }
        let all_columns = self.columns(&relation)?;
        let plan = dataset_query::plan(&all_columns, select, filters)?;
        for warning in plan.warnings {
            if !self.warnings.contains(&warning) {
                self.warnings.push(warning);
            }
        }
        let rows = self
            .adapter()?
            .read_rows_filtered(&relation, &plan.fetch, &plan.pushed, offset, limit)
            .map_err(|_| dataset_error(DATASET_UNAVAILABLE, "Dataset rows could not be read"))?;
        // Counted after SQL filtering: only rows that the predicates let through are served.
        self.rows_served += rows.len() as u64;
        let rows = rows
            .into_iter()
            .map(|row| row.into_iter().map(cell_to_json).collect())
            .collect();
        Ok(DatasetAnswer::Rows {
            columns: plan.fetch.into_iter().map(|(name, _)| name).collect(),
            rows,
            residual: plan.residual,
        })
    }
}

/// Parameters of one batch read: a window of the relation plus the projection and predicates
/// of a lazy dataset (ENT-041). `select` and `filters` are absent for a plain `iter_rows`.
pub(crate) struct RowsRequest<'a> {
    pub offset: Option<u64>,
    pub limit: Option<u32>,
    pub select: Option<&'a [String]>,
    pub filters: &'a [FilterRequest],
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
    pub interpreter: Option<&'a Path>,
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
    /// Invocation attempts of the whole run, including rejected ones. Shared with the children of
    /// a concurrent wave, so the per-run budget is not multiplied by the concurrency (ENT-120c).
    attempts: Arc<AtomicUsize>,
    /// Run that owns this executor; nested `env.run` results report it (ENT-040).
    run_id: String,
    /// Session of the run when it is already known (explicit session); `None` otherwise.
    session_id: Option<String>,
    /// Open `env.write` session (ENT-048). At most one; it belongs to the model that opened it.
    writer: Option<crate::external_write::WriteSession>,
    /// SQL side of the run, for SQL models run by nested `env.run` (ENT-114). Set by the
    /// run that owns the executor.
    pub(crate) sql: Option<SqlExecution>,
}

/// Shareable part of an [`Executor`], from which the children of a concurrent wave are built
/// (ENT-120c). Each child has its own call stack, invocation records, write session and SQL and
/// dataset connections; the invocation budget is shared with the run.
#[derive(Clone)]
pub(crate) struct ChildTemplate<'a> {
    root: &'a Path,
    models_dir: &'a Path,
    interpreter: Option<&'a Path>,
    timeout: Duration,
    max_call_depth: usize,
    max_invocations: usize,
    configuration: &'a Value,
    store: &'a SessionStore,
    cancel: CancellationToken,
    run_id: String,
    session_id: Option<String>,
    attempts: Arc<AtomicUsize>,
}

impl<'a> ChildTemplate<'a> {
    /// A child executor for one node of a wave. `sql` and `datasets` are the node's own
    /// connections; they are never shared with siblings.
    pub(crate) fn child(
        &self,
        sql: Option<SqlExecution>,
        datasets: Option<DatasetSource>,
    ) -> Executor<'a> {
        Executor {
            root: self.root,
            models_dir: self.models_dir,
            interpreter: self.interpreter,
            timeout: self.timeout,
            max_call_depth: self.max_call_depth,
            max_invocations: self.max_invocations,
            configuration: self.configuration,
            store: self.store,
            cancel: self.cancel.clone(),
            invocations: Vec::new(),
            datasets,
            stack: Vec::new(),
            attempts: Arc::clone(&self.attempts),
            run_id: self.run_id.clone(),
            session_id: self.session_id.clone(),
            writer: None,
            sql,
        }
    }
}

/// Interpreter and per-worker timeout used by every Python model of a run.
pub(crate) struct WorkerSettings<'a> {
    pub interpreter: &'a Path,
    pub timeout: Duration,
}

/// Identifiers of the run an executor serves, reported by nested `env.run` results (ENT-040).
pub(crate) struct RunIdentity {
    pub run_id: String,
    /// `None` while the run has no session yet (implicit session created at the end).
    pub session_id: Option<String>,
}

impl<'a> Executor<'a> {
    pub(crate) fn new(
        root: &'a Path,
        models_dir: &'a Path,
        worker: Option<WorkerSettings<'a>>,
        configuration: &'a Value,
        store: &'a SessionStore,
        cancel: CancellationToken,
        identity: RunIdentity,
    ) -> Result<Self> {
        let limit = |name: &str| {
            configuration["effective"]["execution"][name]
                .as_u64()
                .and_then(|value| usize::try_from(value).ok())
                .ok_or_else(|| DowaiError::Config(format!("Effective execution.{name} is invalid")))
        };
        let timeout = match &worker {
            Some(worker) => worker.timeout,
            None => Duration::from_secs(
                configuration["effective"]["database"]["query_timeout_seconds"]
                    .as_u64()
                    .filter(|seconds| *seconds > 0)
                    .ok_or_else(|| {
                        DowaiError::Config(
                            "Effective database.query_timeout_seconds is invalid".to_owned(),
                        )
                    })?,
            ),
        };
        Ok(Self {
            root,
            models_dir,
            interpreter: worker.map(|worker| worker.interpreter),
            timeout,
            max_call_depth: limit("max_call_depth")?,
            max_invocations: limit("max_invocations_per_run")?,
            configuration,
            store,
            cancel,
            invocations: Vec::new(),
            datasets: None,
            stack: Vec::new(),
            attempts: Arc::new(AtomicUsize::new(0)),
            run_id: identity.run_id,
            session_id: identity.session_id,
            writer: None,
            sql: None,
        })
    }

    /// What a child executor of a concurrent wave inherits from this one (ENT-120c). It holds
    /// only shareable values, so a wave's threads can build their children from it.
    pub(crate) fn child_template(&self) -> ChildTemplate<'a> {
        ChildTemplate {
            root: self.root,
            models_dir: self.models_dir,
            interpreter: self.interpreter,
            timeout: self.timeout,
            max_call_depth: self.max_call_depth,
            max_invocations: self.max_invocations,
            configuration: self.configuration,
            store: self.store,
            cancel: self.cancel.clone(),
            run_id: self.run_id.clone(),
            session_id: self.session_id.clone(),
            attempts: Arc::clone(&self.attempts),
        }
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
                    None,
                )
            },
        )
        .1
    }

    /// Runs a SQL or Python target after materialization, preserving the SQL connection.
    /// Python targets receive the parent model as `origem`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn run_post(
        &mut self,
        target: &str,
        parent_path: &str,
        origin: &str,
        mode: ExecutionMode,
        variables: Variables,
        parent_invocation_id: &str,
        on_start: &mut dyn FnMut(&str),
    ) -> Result<Variables> {
        let pushed_parent = self.stack.last().map(String::as_str) != Some(parent_path);
        if pushed_parent {
            self.stack.push(parent_path.to_owned());
        }
        let depends_on = [origin.to_owned()];
        let result = self
            .record(
                "imperative",
                parent_invocation_id,
                target,
                mode,
                |this, invocation_id| {
                    on_start(invocation_id);
                    if is_python(target) {
                        let scope = plan(this.root, &[target.to_owned()], ExecutionMode::Manual)?;
                        let [node] = scope.nodes.as_slice() else {
                            return Err(DowaiError::Model(
                                "run() accepts exactly one model target".to_owned(),
                            ));
                        };
                        this.set_target(invocation_id, &node.id);
                        this.execute_node(
                            &node.path,
                            &depends_on,
                            mode,
                            variables,
                            invocation_id,
                            None,
                            Some(origin),
                        )
                    } else {
                        this.run_child(
                            target,
                            mode,
                            variables,
                            invocation_id,
                            Instant::now() + this.timeout,
                        )
                    }
                },
            )
            .1;
        if pushed_parent {
            self.stack.pop();
        }
        result
    }

    /// Count one invocation attempt; fail once the per-run budget is exceeded.
    /// Rejected attempts count too, so a model cannot probe for free.
    fn admit(&mut self) -> Result<()> {
        let attempts = self.attempts.fetch_add(1, Ordering::SeqCst) + 1;
        if attempts > self.max_invocations {
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
            status: advance(NodeStatus::Pending, NodeStatus::Running),
            duration_ms: 0,
            error_code: None,
        });
        let started = Instant::now();
        let outcome = body(self, &invocation_id);
        let entry = &mut self.invocations[index];
        entry.duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        match outcome {
            Ok(value) => {
                entry.status = advance(NodeStatus::Running, NodeStatus::Succeeded);
                (invocation_id, Ok(value))
            }
            Err(error) => {
                let next = if error.is_cancelled() {
                    NodeStatus::Cancelled
                } else {
                    NodeStatus::Failed
                };
                entry.status = advance(NodeStatus::Running, next);
                entry.error_code = Some(error.code().to_owned());
                (invocation_id, Err(error))
            }
        }
    }

    /// Run one Python model in a worker process, serving its nested calls.
    // `origin` (ENT-126) makes eight arguments; the call sites read clearly by position.
    #[allow(clippy::too_many_arguments)]
    fn execute_node(
        &mut self,
        model_id: &str,
        depends_on: &[String],
        mode: ExecutionMode,
        variables: Variables,
        invocation_id: &str,
        parent_deadline: Option<Instant>,
        origin: Option<&str>,
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
        let interpreter = self.interpreter.ok_or_else(|| {
            DowaiError::Config(
                "Configure python.interpreter with the project's virtual environment".to_owned(),
            )
        })?;
        let root = self.root;
        // ENT-038: limites do worker resolvidos antes de iniciá-lo; `require` recusa aqui,
        // sem executar código do usuário.
        let limits = resources::worker_limits(self.configuration)?;
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
                memory_bytes: limits.memory_bytes,
                origin,
            },
            &variables,
            &mut |request| self.handle_request(request, invocation_id, deadline, depends_on),
        );
        // A write session opened by this model ends with it, even when the model failed (ENT-048).
        if self
            .writer
            .as_ref()
            .is_some_and(|writer| writer.depth() == self.stack.len())
        {
            self.abort_write();
        }
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
                target,
                operation,
                offset,
                limit,
                select,
                filters,
            } => HostReply::Dataset(self.dataset_request(
                &target,
                &operation,
                RowsRequest {
                    offset,
                    limit,
                    select: select.as_deref(),
                    filters: &filters,
                },
                depends_on,
            )),
            HostRequest::Command {
                operation,
                target,
                mode,
                variables,
            } => HostReply::Command(self.command(&operation, target, mode, variables)),
            HostRequest::Export { path } => HostReply::Export(self.export_destination(&path)),
            HostRequest::Write(request) => HostReply::Write(self.write_request(request)),
        }
    }

    /// Serves one `env.write` operation (ENT-048). A failed `rows` or `commit` aborts the
    /// session, so the destination transaction never stays open after an error.
    fn write_request(&mut self, request: WriteRequest) -> Result<Value> {
        self.cancel.check()?;
        match request.operation.as_str() {
            "open" => {
                if self.writer.is_some() {
                    return Err(DowaiError::Write {
                        code: crate::external_write::WRITE_SESSION,
                        message: "another write session is already open".to_owned(),
                    });
                }
                let depth = self.stack.len();
                self.writer = Some(crate::external_write::WriteSession::open(
                    self.root,
                    self.configuration,
                    request,
                    depth,
                    WorkerSettings {
                        interpreter: self.interpreter.ok_or_else(|| DowaiError::Config(
                            "Configure python.interpreter with the project's virtual environment".to_owned(),
                        ))?,
                        timeout: self.timeout,
                    },
                    self.cancel.clone(),
                )?);
                Ok(json!({ "state": "open" }))
            }
            "rows" => {
                let Some(writer) = self.writer.as_mut() else {
                    return Err(Self::no_write_session());
                };
                match writer.stage_rows(&request.rows) {
                    Ok(staged) => Ok(json!({ "rows": staged })),
                    Err(error) => {
                        self.abort_write();
                        Err(error)
                    }
                }
            }
            "commit" => {
                let Some(writer) = self.writer.take() else {
                    return Err(Self::no_write_session());
                };
                writer.commit()
            }
            "abort" => {
                self.abort_write();
                Ok(json!({ "state": "aborted" }))
            }
            _ => Err(DowaiError::Write {
                code: crate::external_write::WRITE_INVALID_REQUEST,
                message: "unknown write operation".to_owned(),
            }),
        }
    }

    fn no_write_session() -> DowaiError {
        DowaiError::Write {
            code: crate::external_write::WRITE_SESSION,
            message: "no write session is open".to_owned(),
        }
    }

    fn abort_write(&mut self) {
        if let Some(writer) = self.writer.take() {
            writer.abort();
        }
    }

    /// Authorizes the destination of `env.generateCsv` (ENT-050).
    ///
    /// The destination must stay inside the project directory: no `..`, no absolute path
    /// outside it, no symbolic link in any component, no directory as target, and no
    /// reserved project entry (`models`, `.dowai`, `.venv`, `dowai_project.json`). Missing
    /// directories inside the project are created. Returns the absolute target path. The
    /// worker writes a temporary file next to it and renames it only after the last row.
    fn export_destination(&self, requested: &str) -> Result<String> {
        self.cancel.check()?;
        if requested.trim().is_empty() || requested.contains('\0') || requested.len() > 4096 {
            return Err(DowaiError::Path(
                "Export path must be a non-empty path without NUL bytes".to_owned(),
            ));
        }
        let root = fs::canonicalize(self.root)?;
        let candidate = Path::new(requested);
        let relative: PathBuf = if candidate.is_absolute() {
            candidate
                .strip_prefix(self.root)
                .or_else(|_| candidate.strip_prefix(&root))
                .map(Path::to_path_buf)
                .map_err(|_| {
                    DowaiError::Policy(
                        "Export destination is outside the project directory".to_owned(),
                    )
                })?
        } else {
            candidate.to_path_buf()
        };
        let mut normal = PathBuf::new();
        for component in relative.components() {
            match component {
                Component::Normal(part) => normal.push(part),
                Component::CurDir => {}
                Component::ParentDir => {
                    return Err(DowaiError::Path(
                        "Export path must not contain '..'".to_owned(),
                    ))
                }
                _ => {
                    return Err(DowaiError::Path(
                        "Export path must be relative to the project directory".to_owned(),
                    ))
                }
            }
        }
        // Lowercased: reserved names must also match on case-insensitive file systems.
        let first = normal
            .components()
            .next()
            .and_then(|component| component.as_os_str().to_str().map(str::to_lowercase));
        let Some(file_name) = normal.file_name().map(|name| name.to_owned()) else {
            return Err(DowaiError::Path(
                "Export path must name a file inside the project directory".to_owned(),
            ));
        };
        let manifest_target =
            normal.components().count() == 1 && first.as_deref() == Some("dowai_project.json");
        if manifest_target || matches!(first.as_deref(), Some("models" | ".dowai" | ".venv")) {
            return Err(DowaiError::Policy(
                "Export destination uses a reserved project directory".to_owned(),
            ));
        }
        let directories = normal.parent().map(Path::to_path_buf).unwrap_or_default();
        let mut current = root;
        for part in directories.components() {
            current.push(part.as_os_str());
            match fs::symlink_metadata(&current) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    return Err(DowaiError::Policy(
                        "Export destination crosses a symbolic link".to_owned(),
                    ))
                }
                Ok(metadata) if metadata.is_dir() => {}
                Ok(_) => {
                    return Err(DowaiError::Path(
                        "Export destination component is not a directory".to_owned(),
                    ))
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    fs::create_dir(&current)?;
                }
                Err(error) => return Err(DowaiError::Io(error)),
            }
        }
        let target = current.join(&file_name);
        match fs::symlink_metadata(&target) {
            Ok(metadata) if metadata.file_type().is_symlink() => Err(DowaiError::Policy(
                "Export destination is a symbolic link".to_owned(),
            )),
            Ok(metadata) if metadata.is_dir() => Err(DowaiError::Path(
                "Export destination is a directory".to_owned(),
            )),
            Ok(_) => Ok(target.to_string_lossy().into_owned()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok(target.to_string_lossy().into_owned())
            }
            Err(error) => Err(DowaiError::Io(error)),
        }
    }

    /// Engine commands callable from a model (`env.plan`, `env.validate`) (ENT-040). Each one
    /// runs the same function as the CLI command and returns the same JSON payload. They
    /// never execute a model and record no invocation.
    fn command(
        &mut self,
        operation: &str,
        target: Option<String>,
        mode: Option<String>,
        variables: Variables,
    ) -> Result<serde_json::Value> {
        self.cancel.check()?;
        match operation {
            "plan" => {
                let target = target
                    .ok_or_else(|| DowaiError::Config("env.plan requires a target".to_owned()))?;
                let mode = match mode.as_deref().unwrap_or("automatic") {
                    "automatic" => ExecutionMode::Automatic,
                    "manual" => ExecutionMode::Manual,
                    _ => {
                        return Err(DowaiError::Config(
                            "env.plan mode must be 'automatic' or 'manual'".to_owned(),
                        ))
                    }
                };
                self.store.validate_variables(&variables)?;
                let execution_plan = plan(self.root, &[target], mode)?;
                prepare_postgres_plan(
                    self.root,
                    &execution_plan,
                    &variables,
                    &template_options(self.configuration)?,
                )?;
                serde_json::to_value(execution_plan)
                    .map_err(|error| DowaiError::Model(error.to_string()))
            }
            "validate" => {
                let report = validate_project_with_configuration(
                    self.root,
                    user_config_path().as_deref(),
                    admin_config_path().as_deref(),
                );
                serde_json::to_value(report).map_err(|error| DowaiError::Model(error.to_string()))
            }
            "connection_get" => self.connection_get(target),
            _ => Err(DowaiError::NotImplemented(
                "This engine command is not available to models".to_owned(),
            )),
        }
    }

    /// `env.connections.get(name)` (ENT-043): a capability handle for a workspace profile.
    /// The profile must pass the same `security.allowed_profiles` and `allowed_hosts` policy
    /// that a SQL run applies. Only the profile name and its type leave the host; host,
    /// user, database, password and any secret stay on the host side.
    fn connection_get(&self, target: Option<String>) -> Result<serde_json::Value> {
        let name = target.ok_or_else(|| {
            DowaiError::Config("env.connections.get requires a profile name".to_owned())
        })?;
        let profile = load_workspace_profile(self.root, &name)?;
        enforce_profile_policy(self.configuration, &name, &profile)?;
        Ok(serde_json::json!({
            "name": name,
            "kind": profile.profile_type,
        }))
    }

    fn dataset_request(
        &mut self,
        target: &DatasetRef,
        operation: &str,
        request: RowsRequest<'_>,
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
            "schema" => source.schema(target, depends_on),
            "rows" => source.rows(target, depends_on, request),
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
                    details: None,
                };
            }
        };
        let target = call.target;
        let variables = call.variables;
        let started = Instant::now();
        // The worker-supplied text is never recorded: the record starts as
        // invalid and is upgraded to the canonical id once resolved.
        let (invocation_id, result) = self.record(
            "imperative",
            parent_invocation_id,
            INVALID_TARGET,
            mode,
            |this, invocation_id| this.run_child(&target, mode, variables, invocation_id, deadline),
        );
        let details = result.is_ok().then(|| CallDetails {
            run_id: self.run_id.clone(),
            session_id: self.session_id.clone(),
            execution_mode: mode.as_str().to_owned(),
            duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        });
        ChildOutcome {
            invocation_id,
            result,
            details,
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
        // The run-time plan of the target: in automatic mode its predecessors (Python and SQL
        // alike) run first. It never changes the static DAG of the caller (ENT-114).
        let scope = plan(self.root, &targets, mode)?;
        let relations = match self.sql.as_ref().and_then(|sql| sql.profile.as_ref().ok()) {
            Some(profile) => build_relations_for_database(self.root, &scope, profile)?,
            None => build_relations(self.root, &scope)?,
        };
        let mut predecessors = Vec::new();
        if mode == ExecutionMode::Automatic {
            predecessors = scope
                .nodes
                .into_iter()
                .filter(|candidate| candidate.id != node.id)
                .collect();
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
                        this.execute_step(
                            predecessor,
                            &relations,
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
        self.execute_step(
            node,
            &relations,
            mode,
            current,
            invocation_id,
            Some(deadline),
        )
    }

    /// One model of a call: a Python model runs in a worker, a SQL model is materialized (ENT-114).
    fn execute_step(
        &mut self,
        node: &PlanNode,
        relations: &BTreeMap<String, String>,
        mode: ExecutionMode,
        variables: Variables,
        invocation_id: &str,
        parent_deadline: Option<Instant>,
    ) -> Result<Variables> {
        if is_python(&node.path) {
            return self.execute_node(
                &node.path,
                &node.depends_on,
                mode,
                variables,
                invocation_id,
                parent_deadline,
                None,
            );
        }
        self.execute_sql(node, relations, mode, &variables, invocation_id)
    }

    /// Renders and materializes one SQL model with the run's SQL side. The variables it
    /// defines are returned to the caller, which merges them like any Python model's variables.
    fn execute_sql(
        &mut self,
        node: &PlanNode,
        relations: &BTreeMap<String, String>,
        mode: ExecutionMode,
        variables: &Variables,
        invocation_id: &str,
    ) -> Result<Variables> {
        self.cancel.check()?;
        if self.stack.iter().any(|entry| entry == &node.path) {
            return Err(DowaiError::Cycle(format!(
                "Imperative call cycle detected: {}",
                self.chain_with(&node.path)
            )));
        }
        if self.stack.len() > self.max_call_depth {
            return Err(DowaiError::Execution(format!(
                "Maximum call depth execution.max_call_depth={} exceeded: {}",
                self.max_call_depth,
                self.chain_with(&node.path)
            )));
        }
        self.stack.push(node.path.clone());
        let result = (|| {
            let sql = self.sql.as_mut().ok_or_else(|| {
                DowaiError::Execution("SQL execution is not configured for this run".to_owned())
            })?;
            let (prepared, next_variables) = render_checked(
                self.root,
                node,
                variables,
                &sql.template_options,
                relations,
                self.store,
            )?;
            sql.materialize(node, mode, None, None, &prepared)?;
            if let Some(datasets) = self.datasets.as_mut() {
                let relation = relations.get(&node.id.replace('\\', "/")).ok_or_else(|| {
                    DowaiError::Execution("SQL model relation is unavailable".to_owned())
                })?;
                datasets.sql_relations.insert(relation.clone());
                datasets.relations.extend(
                    relations
                        .iter()
                        .filter(|(_, value)| *value == relation)
                        .map(|(key, value)| (key.clone(), value.clone())),
                );
            }
            // ENT-126: `run()` targets execute only after the materialization succeeded.
            let mut variables = next_variables;
            for target in &prepared.post_run_targets {
                variables = self.run_post(
                    target,
                    &node.path,
                    &prepared.relation,
                    mode,
                    variables,
                    invocation_id,
                    &mut |_| {},
                )?;
            }
            Ok(variables)
        })();
        self.stack.pop();
        result
    }
}

const INVALID_TARGET: &str = "<invalid>";

/// Recorded text: no control characters, bounded length.
/// Applies one transition of the node state machine and returns the persisted label.
/// The executor only ever moves `PENDING` -> `RUNNING` -> final, which the machine allows,
/// so the fallback to `FAILED` is an internal-invariant guard, never a normal path.
fn advance(current: NodeStatus, next: NodeStatus) -> String {
    current
        .transition_to(next)
        .unwrap_or(NodeStatus::Failed)
        .as_str()
        .to_owned()
}

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
