//! `env.write` on the host (ENT-048, plano §8.3): authorization, pre-conditions and one
//! transaction per write session against a PostgreSQL destination profile.
//!
//! Lifecycle, driven by the worker over IPC (`kind: "write"`):
//! 1. `open`: policy (`allow_external_writes`, allowed profile), destination checks (table,
//!    columns, unique key for `upsert`, explicit confirmation for `replace`), a dedicated
//!    connection, `BEGIN` and a temporary staging table.
//! 2. `rows`: each batch is inserted into the staging table as one JSON parameter. The host
//!    checks that every row has exactly the declared columns, so missing keys never become
//!    silent `NULL`s.
//! 3. `commit`: `append` copies the staging, `replace` deletes the destination rows and copies
//!    the staging, `upsert` merges by keys. Counts are verified, then `COMMIT`.
//! 4. `abort`: `ROLLBACK` and drop the session. Any error in `rows`/`commit` aborts it.
//!
//! The destination profile is `postgres` or `mysql` (ENT-034). MySQL has no transactional DDL
//! and no `ON CONFLICT`: upsert uses `ON DUPLICATE KEY UPDATE`, and the staging is a temporary
//! table dropped explicitly at the end of the session.
//!
//! The write is atomic on the destination only. It is not atomic with the source of the data,
//! and the result never claims otherwise.

use crate::{
    enforce_external_write_policy, map_execution_error, postgres_profile, resolve_profile,
};
use dowai_core::{DowaiError, Result, WorkspaceProfile};
use dowai_mysql::{MysqlAdapter, MysqlError, MysqlProfile};
use dowai_postgres::{quote_identifier, AdapterError, PostgresAdapter, SqlAdapter, TlsMode};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::path::Path;

pub(crate) const WRITE_INVALID_REQUEST: &str = "DOWAI_WRITE_INVALID_REQUEST";
pub(crate) const WRITE_PRECONDITION: &str = "DOWAI_WRITE_PRECONDITION";
pub(crate) const WRITE_SCHEMA_MISMATCH: &str = "DOWAI_WRITE_SCHEMA_MISMATCH";
pub(crate) const WRITE_ROW_INVALID: &str = "DOWAI_WRITE_ROW_INVALID";
pub(crate) const WRITE_DUPLICATE_KEY: &str = "DOWAI_WRITE_DUPLICATE_KEY";
pub(crate) const WRITE_SESSION: &str = "DOWAI_WRITE_SESSION";

/// Rows accepted by one `rows` operation. The worker sends smaller batches.
pub(crate) const MAX_ROWS_PER_OPERATION: usize = 10_000;
/// Name of the temporary staging table, one per dedicated connection.
const STAGING: &str = "dowai_write_stage";

/// Fields of one `env.write` operation, as the worker sent them.
#[derive(Debug, Default)]
pub(crate) struct WriteRequest {
    pub operation: String,
    pub target: Option<String>,
    pub table: Option<String>,
    pub mode: Option<String>,
    pub keys: Option<Vec<String>>,
    pub columns: Option<Vec<String>>,
    pub confirm_replace: bool,
    pub rows: Vec<Value>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WriteMode {
    Append,
    Replace,
    Upsert,
}

impl WriteMode {
    fn parse(text: &str) -> Result<Self> {
        match text {
            "append" => Ok(Self::Append),
            "replace" => Ok(Self::Replace),
            "upsert" => Ok(Self::Upsert),
            _ => Err(invalid("mode must be 'append', 'replace' or 'upsert'")),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Append => "append",
            Self::Replace => "replace",
            Self::Upsert => "upsert",
        }
    }
}

/// An open write session. Holds the destination connection and its transaction.
pub(crate) struct ExternalWrite {
    adapter: Backend,
    table: String,
    mode: WriteMode,
    keys: Vec<String>,
    columns: Vec<String>,
    staged: u64,
    /// Depth of the model stack that opened the session; the executor aborts it when that
    /// model ends, so a failed model never leaves a transaction open.
    depth: usize,
}

fn invalid(message: &str) -> DowaiError {
    DowaiError::Write {
        code: WRITE_INVALID_REQUEST,
        message: message.to_owned(),
    }
}

fn precondition(message: &str) -> DowaiError {
    DowaiError::Write {
        code: WRITE_PRECONDITION,
        message: message.to_owned(),
    }
}

fn map_adapter(error: AdapterError) -> DowaiError {
    match error {
        AdapterError::DuplicateMergeKey => DowaiError::Write {
            code: WRITE_DUPLICATE_KEY,
            message: "staged rows repeat a merge key".to_owned(),
        },
        AdapterError::InvalidIdentifier => {
            invalid("table or column name is not a valid identifier")
        }
        other => map_execution_error(other),
    }
}

fn validate_names(names: &[String], label: &str) -> Result<()> {
    if names.is_empty() {
        return Err(invalid(&format!("{label} must not be empty")));
    }
    let mut seen = BTreeSet::new();
    for name in names {
        if name.trim().is_empty() || name.contains('\0') {
            return Err(invalid(&format!("{label} must be non-empty strings")));
        }
        if !seen.insert(name.as_str()) {
            return Err(invalid(&format!("{label} must not repeat a name")));
        }
    }
    Ok(())
}

impl ExternalWrite {
    /// Opens a session after authorization and destination checks. Any failure rolls back the
    /// transaction (dropping the connection also does).
    pub(crate) fn open(
        root: &Path,
        configuration: &Value,
        request: WriteRequest,
        depth: usize,
    ) -> Result<Self> {
        enforce_external_write_policy(configuration)?;
        let target_profile = request
            .target
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| invalid("target must be a non-empty profile name"))?;
        let table = request
            .table
            .clone()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| invalid("table must be a non-empty name"))?;
        let mode = WriteMode::parse(request.mode.as_deref().unwrap_or("append"))?;
        let columns = request
            .columns
            .clone()
            .ok_or_else(|| invalid("columns are required"))?;
        validate_names(&columns, "columns")?;
        let keys = request.keys.clone().unwrap_or_default();
        match mode {
            WriteMode::Upsert => {
                if keys.is_empty() {
                    return Err(precondition("upsert requires keys"));
                }
                validate_names(&keys, "keys")?;
                if keys.iter().any(|key| !columns.contains(key)) {
                    return Err(invalid("keys must be written columns"));
                }
            }
            WriteMode::Append | WriteMode::Replace => {
                if !keys.is_empty() {
                    return Err(invalid("keys are only accepted by upsert"));
                }
            }
        }
        if mode == WriteMode::Replace && !request.confirm_replace {
            return Err(precondition(
                "replace requires explicit confirmation (confirm_replace=True)",
            ));
        }
        if mode != WriteMode::Replace && request.confirm_replace {
            return Err(invalid("confirm_replace only applies to replace"));
        }
        let quoted_table = quote_identifier(&table).map_err(map_adapter)?;

        // Authorization of the destination profile: the same policy as every profile.
        let (_, profile) = resolve_profile(root, configuration, Some(target_profile))?;
        let mut adapter = Backend::connect(profile, configuration)?;
        let prepared = (|| -> Result<()> {
            adapter.begin_transaction()?;
            let described = adapter
                .describe(&table, &quoted_table)
                .map_err(|_| precondition("destination table does not exist or is not readable"))?;
            for column in &columns {
                if !described.iter().any(|name| name == column) {
                    return Err(DowaiError::Write {
                        code: WRITE_SCHEMA_MISMATCH,
                        message: "a written column does not exist in the destination table"
                            .to_owned(),
                    });
                }
            }
            if mode == WriteMode::Upsert && !adapter.has_unique_key(&table, &keys)? {
                return Err(precondition(
                    "destination has no unique key exactly on the upsert keys",
                ));
            }
            adapter.create_staging(&table, STAGING)?;
            Ok(())
        })();
        match prepared {
            Ok(()) => Ok(Self {
                adapter,
                table,
                mode,
                keys,
                columns,
                staged: 0,
                depth,
            }),
            Err(error) => {
                adapter.rollback_quietly();
                Err(error)
            }
        }
    }

    pub(crate) fn depth(&self) -> usize {
        self.depth
    }

    /// Stages one batch. Every row must have exactly the declared columns.
    pub(crate) fn stage_rows(&mut self, rows: &[Value]) -> Result<u64> {
        if rows.len() > MAX_ROWS_PER_OPERATION {
            return Err(invalid("rows operation exceeds the batch limit"));
        }
        if rows.is_empty() {
            return Ok(0);
        }
        for row in rows {
            let Some(object) = row.as_object() else {
                return Err(DowaiError::Write {
                    code: WRITE_ROW_INVALID,
                    message: "every row must be a mapping".to_owned(),
                });
            };
            let complete = object.len() == self.columns.len()
                && self
                    .columns
                    .iter()
                    .all(|column| object.contains_key(column));
            if !complete {
                return Err(DowaiError::Write {
                    code: WRITE_ROW_INVALID,
                    message: "every row must have exactly the declared columns".to_owned(),
                });
            }
        }
        let payload = serde_json::to_string(rows).map_err(|_| DowaiError::Write {
            code: WRITE_ROW_INVALID,
            message: "rows could not be encoded".to_owned(),
        })?;
        let inserted = self
            .adapter
            .write_staging(STAGING, &self.columns, rows, &payload)?;
        self.staged += inserted;
        Ok(inserted)
    }

    /// Applies the staging to the destination and commits. Counts are checked before the commit.
    pub(crate) fn commit(mut self) -> Result<Value> {
        let outcome = self.apply();
        match outcome {
            Ok(payload) => {
                self.adapter.commit()?;
                Ok(payload)
            }
            Err(error) => {
                self.adapter.rollback_quietly();
                Err(error)
            }
        }
    }

    fn apply(&mut self) -> Result<Value> {
        let (inserted, updated, deleted) = match self.mode {
            WriteMode::Append => {
                let inserted = self
                    .adapter
                    .append_staging(STAGING, &self.table, &self.columns)?;
                (inserted, 0, 0)
            }
            WriteMode::Replace => {
                let deleted = self.adapter.delete_all(&self.table)?;
                let inserted = self
                    .adapter
                    .append_staging(STAGING, &self.table, &self.columns)?;
                (inserted, 0, deleted)
            }
            WriteMode::Upsert => {
                let (inserted, updated) =
                    self.adapter
                        .merge(STAGING, &self.table, &self.columns, &self.keys)?;
                (inserted, updated, 0)
            }
        };
        let accounted = match self.mode {
            WriteMode::Upsert => inserted + updated,
            _ => inserted,
        };
        if accounted != self.staged {
            return Err(DowaiError::Write {
                code: WRITE_SCHEMA_MISMATCH,
                message: "destination did not accept every staged row".to_owned(),
            });
        }
        Ok(json!({
            "mode": self.mode.as_str(),
            "rows_staged": self.staged,
            "inserted": inserted,
            "updated": updated,
            "deleted": deleted,
        }))
    }

    /// Rolls back the transaction and discards the staging table.
    pub(crate) fn abort(mut self) {
        self.adapter.rollback_quietly();
    }
}

fn map_mysql(error: MysqlError) -> DowaiError {
    match error {
        MysqlError::DuplicateMergeKey => DowaiError::Write {
            code: WRITE_DUPLICATE_KEY,
            message: "staged rows repeat a merge key".to_owned(),
        },
        MysqlError::InvalidIdentifier => invalid("table or column name is not a valid identifier"),
        MysqlError::InvalidProfile(_) => {
            DowaiError::Config("Invalid MySQL connection profile".to_owned())
        }
        MysqlError::InsecureTlsDisabled => {
            DowaiError::Policy("TLS may only be disabled for a local MySQL host".to_owned())
        }
        MysqlError::MissingPassword
        | MysqlError::TlsConfigurationFailed
        | MysqlError::ConnectionFailed => DowaiError::Connection(error.to_string()),
        MysqlError::QueryFailed
        | MysqlError::TransientFailure
        | MysqlError::InvalidTransactionState
        | MysqlError::InvalidStreamLimit
        | MysqlError::InflightLimitExceeded
        | MysqlError::InvalidWriteSpec(_)
        | MysqlError::Unsupported(_) => DowaiError::Execution(error.to_string()),
    }
}

/// Destination connection of a write session: one transaction on one database. A transfer
/// between two databases is two independent transactions, never one atomic unit.
enum Backend {
    Postgres(Box<PostgresAdapter>),
    Mysql(MysqlAdapter),
}

impl Backend {
    fn connect(profile: WorkspaceProfile, configuration: &Value) -> Result<Self> {
        if profile.profile_type == "mysql" {
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
                adapter_profile.set_connect_timeout(std::time::Duration::from_secs(seconds));
            }
            if let Some(seconds) = database["query_timeout_seconds"].as_u64() {
                adapter_profile.set_query_timeout(Some(std::time::Duration::from_secs(seconds)));
            }
            return MysqlAdapter::connect(&adapter_profile)
                .map(Self::Mysql)
                .map_err(map_mysql);
        }
        let pg_profile = postgres_profile(profile, configuration)?;
        PostgresAdapter::connect(&pg_profile)
            .map(|adapter| Self::Postgres(Box::new(adapter)))
            .map_err(map_execution_error)
    }

    fn begin_transaction(&mut self) -> Result<()> {
        match self {
            Self::Postgres(adapter) => adapter.begin_transaction().map_err(map_adapter),
            Self::Mysql(adapter) => adapter.begin_transaction().map_err(map_mysql),
        }
    }

    fn commit(&mut self) -> Result<()> {
        match self {
            Self::Postgres(adapter) => adapter.commit().map_err(map_adapter),
            Self::Mysql(adapter) => adapter.commit().map_err(map_mysql),
        }
    }

    fn rollback_quietly(&mut self) {
        match self {
            Self::Postgres(adapter) => {
                let _ = adapter.rollback();
            }
            Self::Mysql(adapter) => {
                let _ = adapter.rollback();
            }
        }
    }

    /// Column names of the destination table.
    fn describe(&mut self, table: &str, quoted_table: &str) -> Result<Vec<String>> {
        match self {
            Self::Postgres(adapter) => adapter
                .describe_relation(quoted_table)
                .map(|columns| columns.into_iter().map(|(name, _)| name).collect())
                .map_err(map_adapter),
            Self::Mysql(adapter) => adapter
                .introspect_table(table)
                .map(|columns| columns.into_iter().map(|column| column.name).collect())
                .map_err(map_mysql),
        }
    }

    fn has_unique_key(&mut self, table: &str, keys: &[String]) -> Result<bool> {
        match self {
            Self::Postgres(adapter) => adapter.has_unique_key(table, keys).map_err(map_adapter),
            Self::Mysql(adapter) => adapter.has_unique_key(table, keys).map_err(map_mysql),
        }
    }

    fn create_staging(&mut self, table: &str, staging: &str) -> Result<()> {
        match self {
            Self::Postgres(adapter) => adapter.create_staging(table, staging).map_err(map_adapter),
            Self::Mysql(adapter) => adapter.create_staging(table, staging).map_err(map_mysql),
        }
    }

    /// `rows` are validated objects with exactly `columns`; `payload` is their JSON text.
    fn write_staging(
        &mut self,
        staging: &str,
        columns: &[String],
        rows: &[Value],
        payload: &str,
    ) -> Result<u64> {
        match self {
            Self::Postgres(adapter) => adapter
                .write_staging_json(staging, columns, payload)
                .map_err(map_adapter),
            Self::Mysql(adapter) => {
                let values: Vec<Vec<dowai_mysql::Value>> = rows
                    .iter()
                    .map(|row| {
                        columns
                            .iter()
                            .map(|column| json_to_mysql(&row[column.as_str()]))
                            .collect()
                    })
                    .collect();
                adapter
                    .write_staging_batch(staging, columns, &values)
                    .map_err(map_mysql)
            }
        }
    }

    fn append_staging(&mut self, staging: &str, table: &str, columns: &[String]) -> Result<u64> {
        match self {
            Self::Postgres(adapter) => adapter
                .append_staging(staging, table, columns)
                .map_err(map_adapter),
            Self::Mysql(adapter) => adapter
                .append_staging(staging, table, columns)
                .map_err(map_mysql),
        }
    }

    fn delete_all(&mut self, table: &str) -> Result<u64> {
        match self {
            Self::Postgres(adapter) => adapter.delete_all(table).map_err(map_adapter),
            Self::Mysql(adapter) => adapter.delete_all(table).map_err(map_mysql),
        }
    }

    /// Inserted and updated counts of the merge.
    fn merge(
        &mut self,
        staging: &str,
        table: &str,
        columns: &[String],
        keys: &[String],
    ) -> Result<(u64, u64)> {
        match self {
            Self::Postgres(adapter) => adapter
                .merge_staging(staging, table, columns, keys)
                .map(|counters| (counters.inserted, counters.updated))
                .map_err(map_adapter),
            Self::Mysql(adapter) => adapter
                .merge(staging, table, columns, keys)
                .map(|counters| (counters.inserted, counters.updated))
                .map_err(map_mysql),
        }
    }
}

/// JSON value to MySQL parameter. Booleans become 0/1 (MySQL has no real boolean); arrays and
/// objects are sent as JSON text.
fn json_to_mysql(value: &Value) -> dowai_mysql::Value {
    match value {
        Value::Null => dowai_mysql::Value::NULL,
        Value::Bool(flag) => dowai_mysql::Value::Int(i64::from(*flag)),
        Value::Number(number) => number
            .as_i64()
            .map(dowai_mysql::Value::Int)
            .or_else(|| number.as_u64().map(dowai_mysql::Value::UInt))
            .or_else(|| number.as_f64().map(dowai_mysql::Value::Double))
            .unwrap_or(dowai_mysql::Value::NULL),
        Value::String(text) => dowai_mysql::Value::Bytes(text.clone().into_bytes()),
        other => dowai_mysql::Value::Bytes(other.to_string().into_bytes()),
    }
}
