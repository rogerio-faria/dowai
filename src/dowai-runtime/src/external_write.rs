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

use crate::database::{DatabaseAdapter, DatabaseProfile, NativeTransaction};
use crate::{
    enforce_external_write_policy, map_execution_error, postgres_profile, resolve_profile,
};
use dowai_core::{CancellationToken, DowaiError, Result, WorkspaceProfile};
use dowai_mysql::{MysqlAdapter, MysqlError};
use dowai_postgres::{quote_identifier, AdapterError, PostgresAdapter, SqlAdapter};
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
pub(crate) enum WriteMode {
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

    pub(crate) fn as_str(self) -> &'static str {
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

/// Validated parts of a `write` request (ENT-128): shared by the SQL and the driver sessions.
pub(crate) struct Validated {
    pub table: String,
    pub mode: WriteMode,
    pub columns: Vec<String>,
    pub keys: Vec<String>,
}

/// Generic checks of a `write` request, independent of the destination kind.
pub(crate) fn validate_write_request(request: &WriteRequest) -> Result<Validated> {
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
    Ok(Validated {
        table,
        mode,
        columns,
        keys,
    })
}

impl ExternalWrite {
    /// Opens a session after authorization and destination checks. Any failure rolls back the
    /// transaction (dropping the connection also does).
    pub(crate) fn open(
        root: &Path,
        configuration: &Value,
        request: WriteRequest,
        depth: usize,
        cancel: CancellationToken,
    ) -> Result<Self> {
        enforce_external_write_policy(configuration)?;
        let target_profile = request
            .target
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| invalid("target must be a non-empty profile name"))?;
        let Validated {
            table,
            mode,
            columns,
            keys,
        } = validate_write_request(&request)?;
        let quoted_table = quote_identifier(&table).map_err(map_adapter)?;

        // Authorization of the destination profile: the same policy as every profile.
        let (_, profile) = resolve_profile(root, configuration, Some(target_profile))?;
        let mut adapter = Backend::connect(profile, configuration, cancel)?;
        let prepared = (|| -> Result<()> {
            adapter.begin_transaction()?;
            let described = adapter.describe(&table, &quoted_table).map_err(|error| {
                if matches!(adapter, Backend::Native(_)) {
                    error
                } else {
                    precondition("destination table does not exist or is not readable")
                }
            })?;
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
        MysqlError::DestructiveApprovalRequired => DowaiError::Policy(
            "Replacing an existing relation requires explicit approval".to_owned(),
        ),
        MysqlError::MissingPassword
        | MysqlError::TlsConfigurationFailed
        | MysqlError::ConnectionFailed => DowaiError::Connection(error.to_string()),
        MysqlError::QueryFailed
        | MysqlError::TransientFailure
        | MysqlError::InvalidTransactionState
        | MysqlError::InvalidStreamLimit
        | MysqlError::InflightLimitExceeded
        | MysqlError::InvalidWriteSpec(_)
        | MysqlError::SchemaMismatch
        | MysqlError::Unsupported(_) => DowaiError::Execution(error.to_string()),
    }
}

/// Destination connection of a write session: one transaction on one database. A transfer
/// between two databases is two independent transactions, never one atomic unit.
enum Backend {
    Postgres(Box<PostgresAdapter>),
    Mysql(MysqlAdapter),
    Native(NativeWriteBackend),
}

struct NativeWriteBackend {
    adapter: DatabaseAdapter,
    oracle: bool,
    cancel: CancellationToken,
}

impl NativeWriteBackend {
    fn quote(&self, name: &str) -> Result<String> {
        if self.oracle {
            dowai_databases_oracle::quote_identifier(name)
                .map_err(|_| invalid("table or column name is not a valid identifier"))
        } else {
            dowai_sqlserver::quote_identifier(name)
                .map_err(|_| invalid("table or column name is not a valid identifier"))
        }
    }

    fn staging(&self) -> &'static str {
        if self.oracle {
            "\"ORA$PTT_DOWAI_STAGE\""
        } else {
            "[#dowai_write_stage]"
        }
    }

    fn transaction(&mut self, action: NativeTransaction) -> Result<()> {
        self.adapter
            .native_transaction(action, &|| self.cancel.is_cancelled())
            .map_err(|error| error.into_dowai())
    }

    fn execute(&mut self, sql: &str) -> Result<u64> {
        self.adapter
            .native_execute(sql, &|| self.cancel.is_cancelled())
            .map_err(|error| error.into_dowai())
    }

    fn query(&mut self, sql: &str, parameters: &[String]) -> Result<Vec<Value>> {
        self.adapter
            .native_query(sql, parameters, &|| self.cancel.is_cancelled())
            .map_err(|error| error.into_dowai())
    }

    fn describe(&mut self, table: &str) -> Result<Vec<String>> {
        let query = if self.oracle {
            "SELECT SYS_CONTEXT('USERENV','CURRENT_SCHEMA') AS \"DOWAI_SCHEMA\" FROM DUAL"
        } else {
            "SELECT SCHEMA_NAME() AS [DOWAI_SCHEMA]"
        };
        let rows = self.query(query, &[])?;
        let schema = rows
            .first()
            .and_then(|row| row["DOWAI_SCHEMA"].as_str())
            .ok_or_else(|| precondition("destination schema could not be determined"))?
            .to_owned();
        self.adapter
            .native_columns(&schema, table, &|| self.cancel.is_cancelled())
            .map(|columns| columns.into_iter().map(|(name, _)| name).collect())
            .map_err(|error| error.into_dowai())
    }

    fn has_unique_key(&mut self, table: &str, keys: &[String]) -> Result<bool> {
        let (sql, parameter) = if self.oracle {
            ("SELECT i.INDEX_NAME AS \"DOWAI_INDEX\", c.COLUMN_NAME AS \"DOWAI_COLUMN\" \
              FROM ALL_INDEXES i JOIN ALL_IND_COLUMNS c \
              ON i.OWNER=c.INDEX_OWNER AND i.INDEX_NAME=c.INDEX_NAME \
              WHERE i.TABLE_OWNER=SYS_CONTEXT('USERENV','CURRENT_SCHEMA') AND i.TABLE_NAME=:1 \
              AND i.UNIQUENESS='UNIQUE' AND i.STATUS='VALID' ORDER BY i.INDEX_NAME,c.COLUMN_POSITION",
             table.to_owned())
        } else {
            ("SELECT i.name AS [DOWAI_INDEX], COL_NAME(ic.object_id, ic.column_id) AS [DOWAI_COLUMN] \
              FROM sys.indexes i JOIN sys.index_columns ic \
              ON i.object_id=ic.object_id AND i.index_id=ic.index_id \
              WHERE i.object_id=OBJECT_ID(@P1) AND i.is_unique=1 AND i.is_disabled=0 \
              AND i.is_hypothetical=0 AND i.has_filter=0 AND ic.key_ordinal>0 \
              ORDER BY i.name,ic.key_ordinal",
             self.quote(table)?)
        };
        let rows = self.query(sql, &[parameter])?;
        let mut indexes: std::collections::BTreeMap<String, Vec<String>> =
            std::collections::BTreeMap::new();
        for row in rows {
            let index = row["DOWAI_INDEX"]
                .as_str()
                .ok_or_else(|| precondition("unique index metadata is invalid"))?;
            let column = row["DOWAI_COLUMN"]
                .as_str()
                .ok_or_else(|| precondition("unique index metadata is invalid"))?;
            indexes
                .entry(index.to_owned())
                .or_default()
                .push(column.to_owned());
        }
        let requested: BTreeSet<&str> = keys.iter().map(String::as_str).collect();
        Ok(indexes.values().any(|columns| {
            columns.len() == keys.len()
                && columns.iter().map(String::as_str).collect::<BTreeSet<_>>() == requested
        }))
    }

    fn create_staging(&mut self, table: &str) -> Result<()> {
        let table = self.quote(table)?;
        let staging = self.staging();
        let sql = if self.oracle {
            format!("CREATE PRIVATE TEMPORARY TABLE {staging} ON COMMIT DROP DEFINITION AS SELECT * FROM {table} WHERE 1=0")
        } else {
            format!("SELECT TOP (0) * INTO {staging} FROM {table}")
        };
        self.adapter
            .native_batch(&sql, &|| self.cancel.is_cancelled())
            .map_err(|error| error.into_dowai())
    }

    fn stage_rows(&mut self, rows: &[Value]) -> Result<u64> {
        self.adapter
            .native_stage_rows("ORA$PTT_DOWAI_STAGE", "#dowai_write_stage", rows, &|| {
                self.cancel.is_cancelled()
            })
            .map_err(|error| error.into_dowai())
    }

    fn append(&mut self, table: &str, columns: &[String]) -> Result<u64> {
        let target = self.quote(table)?;
        let columns = columns
            .iter()
            .map(|name| self.quote(name))
            .collect::<Result<Vec<_>>>()?
            .join(", ");
        self.execute(&format!(
            "INSERT INTO {target} ({columns}) SELECT {columns} FROM {}",
            self.staging()
        ))
    }

    fn delete_all(&mut self, table: &str) -> Result<u64> {
        self.execute(&format!("DELETE FROM {}", self.quote(table)?))
    }

    fn count(&mut self, sql: &str) -> Result<u64> {
        let rows = self.query(sql, &[])?;
        rows.first()
            .and_then(|row| row["DOWAI_COUNT"].as_str())
            .and_then(|value| value.parse::<u64>().ok())
            .ok_or_else(|| {
                DowaiError::Execution("Database returned an invalid row count".to_owned())
            })
    }

    fn merge(&mut self, table: &str, columns: &[String], keys: &[String]) -> Result<(u64, u64)> {
        let target = self.quote(table)?;
        let staging = self.staging();
        let quoted_keys = keys
            .iter()
            .map(|name| self.quote(name))
            .collect::<Result<Vec<_>>>()?;
        let quoted_columns = columns
            .iter()
            .map(|name| self.quote(name))
            .collect::<Result<Vec<_>>>()?;
        let nulls = quoted_keys
            .iter()
            .map(|key| format!("{key} IS NULL"))
            .collect::<Vec<_>>()
            .join(" OR ");
        if self.count(&format!(
            "SELECT COUNT(*) AS DOWAI_COUNT FROM {staging} WHERE {nulls}"
        ))? != 0
        {
            return Err(precondition("upsert keys must not be null"));
        }
        let grouping = quoted_keys.join(", ");
        if self.count(&format!("SELECT COUNT(*) AS DOWAI_COUNT FROM (SELECT {grouping} FROM {staging} GROUP BY {grouping} HAVING COUNT(*) > 1) duplicates"))? != 0 {
            return Err(DowaiError::Write {
                code: WRITE_DUPLICATE_KEY,
                message: "staged rows repeat a merge key".to_owned(),
            });
        }
        let condition = quoted_keys
            .iter()
            .map(|key| format!("t.{key}=s.{key}"))
            .collect::<Vec<_>>()
            .join(" AND ");
        let lock = if self.oracle {
            ""
        } else {
            " WITH (UPDLOCK,HOLDLOCK)"
        };
        let updated = self.count(&format!(
            "SELECT COUNT(*) AS DOWAI_COUNT FROM {staging} s JOIN {target} t{lock} ON {condition}"
        ))?;
        let total = self.count(&format!("SELECT COUNT(*) AS DOWAI_COUNT FROM {staging}"))?;
        let inserted = total.checked_sub(updated).ok_or_else(|| {
            DowaiError::Execution("Database merge counts are inconsistent".to_owned())
        })?;
        let key_set: BTreeSet<&str> = keys.iter().map(String::as_str).collect();
        let mut updates = columns
            .iter()
            .zip(&quoted_columns)
            .filter(|(name, _)| !key_set.contains(name.as_str()))
            .map(|(_, quoted)| format!("t.{quoted}=s.{quoted}"))
            .collect::<Vec<_>>();
        if updates.is_empty() {
            updates.push(format!("t.{}=s.{}", quoted_keys[0], quoted_keys[0]));
        }
        let values = quoted_columns
            .iter()
            .map(|column| format!("s.{column}"))
            .collect::<Vec<_>>()
            .join(", ");
        let source = if self.oracle {
            format!("MERGE INTO {target} t USING {staging} s ON ({condition})")
        } else {
            format!("MERGE INTO {target} WITH (HOLDLOCK) AS t USING {staging} AS s ON {condition}")
        };
        let sql = format!("{source} WHEN MATCHED THEN UPDATE SET {} WHEN NOT MATCHED THEN INSERT ({}) VALUES ({values}){}", updates.join(", "), quoted_columns.join(", "), if self.oracle { "" } else { ";" });
        let affected = self.execute(&sql)?;
        if affected != total {
            return Err(DowaiError::Write {
                code: WRITE_SCHEMA_MISMATCH,
                message: "destination did not accept every staged row".to_owned(),
            });
        }
        Ok((inserted, updated))
    }
}

impl Backend {
    fn connect(
        profile: WorkspaceProfile,
        configuration: &Value,
        cancel: CancellationToken,
    ) -> Result<Self> {
        if matches!(profile.profile_type.as_str(), "oracle" | "sqlserver") {
            let database = DatabaseProfile::from_workspace(profile, configuration)?;
            let oracle = matches!(database, DatabaseProfile::Oracle { .. });
            let adapter =
                DatabaseAdapter::connect(&database).map_err(|error| error.into_dowai())?;
            return Ok(Self::Native(NativeWriteBackend {
                adapter,
                oracle,
                cancel,
            }));
        }
        if profile.profile_type != "postgres" && profile.profile_type != "mysql" {
            return Err(DowaiError::Config(format!(
                "env.write does not yet support database profile type '{}'",
                profile.profile_type
            )));
        }
        if profile.profile_type == "mysql" {
            let adapter_profile = crate::mysql_profile(profile, configuration)?;
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
            Self::Native(adapter) => adapter.transaction(NativeTransaction::Begin),
        }
    }

    fn commit(&mut self) -> Result<()> {
        match self {
            Self::Postgres(adapter) => adapter.commit().map_err(map_adapter),
            Self::Mysql(adapter) => adapter.commit().map_err(map_mysql),
            Self::Native(adapter) => adapter.transaction(NativeTransaction::Commit),
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
            Self::Native(_) => {}
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
            Self::Native(adapter) => adapter.describe(table),
        }
    }

    fn has_unique_key(&mut self, table: &str, keys: &[String]) -> Result<bool> {
        match self {
            Self::Postgres(adapter) => adapter.has_unique_key(table, keys).map_err(map_adapter),
            Self::Mysql(adapter) => adapter.has_unique_key(table, keys).map_err(map_mysql),
            Self::Native(adapter) => adapter.has_unique_key(table, keys),
        }
    }

    fn create_staging(&mut self, table: &str, staging: &str) -> Result<()> {
        match self {
            Self::Postgres(adapter) => adapter.create_staging(table, staging).map_err(map_adapter),
            Self::Mysql(adapter) => adapter.create_staging(table, staging).map_err(map_mysql),
            Self::Native(adapter) => adapter.create_staging(table),
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
            Self::Native(adapter) => adapter.stage_rows(rows),
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
            Self::Native(adapter) => adapter.append(table, columns),
        }
    }

    fn delete_all(&mut self, table: &str) -> Result<u64> {
        match self {
            Self::Postgres(adapter) => adapter.delete_all(table).map_err(map_adapter),
            Self::Mysql(adapter) => adapter.delete_all(table).map_err(map_mysql),
            Self::Native(adapter) => adapter.delete_all(table),
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
            Self::Native(adapter) => adapter.merge(table, columns, keys),
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

/// One `env.write` session, whatever its destination kind (ENT-128). A SQL profile keeps the
/// built-in adapter path; an `external` profile runs through its Python driver.
pub(crate) enum WriteSession {
    Sql(ExternalWrite),
    Driver(crate::external_driver::ExternalDriverSession),
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod tests {
    use super::*;

    fn live_profile(kind: &str) -> DatabaseProfile {
        let variable = |suffix: &str| {
            std::env::var(format!(
                "DOWAI_TEST_{}_{}",
                kind.to_ascii_uppercase(),
                suffix
            ))
            .unwrap_or_else(|_| panic!("missing database test setting {suffix}"))
        };
        let is_oracle = kind == "oracle";
        let workspace: WorkspaceProfile = serde_json::from_value(json!({
                    "type": kind,
                    "host": variable("HOST"),
                    "port": variable("PORT").parse::<u16>().expect("invalid port"),
                    "user": variable("USER"),
                    "database": if is_oracle { variable("SERVICE") } else { variable("DATABASE") },
                    "password": if is_oracle { "{{ env_var('DOWAI_ORACLE_PASSWORD') }}" } else { "{{ env_var('DOWAI_SQLSERVER_PASSWORD') }}" },
                    "tls": if is_oracle { "oracle_native" } else { "required" }
                })).unwrap();
        DatabaseProfile::from_workspace(
            workspace,
            &json!({"effective":{"database":{
                "connect_timeout_seconds":30,"query_timeout_seconds":30
            }}}),
        )
        .unwrap()
    }

    struct Cleanup {
        profile: DatabaseProfile,
        table: String,
    }

    impl Drop for Cleanup {
        fn drop(&mut self) {
            let oracle = matches!(self.profile, DatabaseProfile::Oracle { .. });
            let quoted = if oracle {
                dowai_databases_oracle::quote_identifier(&self.table).unwrap()
            } else {
                dowai_sqlserver::quote_identifier(&self.table).unwrap()
            };
            let sql = if oracle {
                format!("DROP TABLE {quoted} PURGE")
            } else {
                format!("DROP TABLE IF EXISTS {quoted}")
            };
            match DatabaseAdapter::connect(&self.profile)
                .and_then(|mut adapter| adapter.native_execute(&sql, &|| false))
            {
                Ok(_) => {}
                Err(error) => eprintln!("Test table cleanup failed: {:?}", error),
            }
        }
    }

    fn open_writer(profile: &DatabaseProfile, table: &str, mode: WriteMode) -> ExternalWrite {
        let oracle = matches!(profile, DatabaseProfile::Oracle { .. });
        let mut adapter = NativeWriteBackend {
            adapter: DatabaseAdapter::connect(profile).unwrap(),
            oracle,
            cancel: CancellationToken::new(),
        };
        adapter.transaction(NativeTransaction::Begin).unwrap();
        let columns = vec!["id".to_owned(), "label".to_owned()];
        assert_eq!(adapter.describe(table).unwrap(), columns);
        if mode == WriteMode::Upsert {
            assert!(adapter.has_unique_key(table, &["id".to_owned()]).unwrap());
        }
        adapter.create_staging(table).unwrap();
        ExternalWrite {
            adapter: Backend::Native(adapter),
            table: table.to_owned(),
            mode,
            keys: if mode == WriteMode::Upsert {
                vec!["id".to_owned()]
            } else {
                vec![]
            },
            columns,
            staged: 0,
            depth: 0,
        }
    }

    fn exercise_native_writes(kind: &str) {
        let profile = live_profile(kind);
        let oracle = kind == "oracle";
        let table = format!(
            "DOWAI_WRITE_{}",
            &uuid::Uuid::new_v4().simple().to_string()[..12]
        );
        let cleanup = Cleanup {
            profile: profile.clone(),
            table: table.clone(),
        };
        let mut connection = DatabaseAdapter::connect(&profile).unwrap();
        let quoted = if oracle {
            dowai_databases_oracle::quote_identifier(&table).unwrap()
        } else {
            dowai_sqlserver::quote_identifier(&table).unwrap()
        };
        let sql = if oracle {
            format!("CREATE TABLE {quoted} (\"id\" NUMBER PRIMARY KEY, \"label\" VARCHAR2(40))")
        } else {
            format!("CREATE TABLE {quoted} ([id] int PRIMARY KEY, [label] nvarchar(40))")
        };
        connection.native_execute(&sql, &|| false).unwrap();
        drop(connection);

        let mut append = open_writer(&profile, &table, WriteMode::Append);
        append
            .stage_rows(&[json!({"id":1,"label":"one"}), json!({"id":2,"label":"two"})])
            .unwrap();
        assert_eq!(append.commit().unwrap()["inserted"], json!(2));

        let mut aborted = open_writer(&profile, &table, WriteMode::Replace);
        aborted
            .stage_rows(&[json!({"id":3,"label":"three"})])
            .unwrap();
        aborted.abort();

        let mut replace = open_writer(&profile, &table, WriteMode::Replace);
        replace
            .stage_rows(&[json!({"id":1,"label":"before"})])
            .unwrap();
        let replaced = replace.commit().unwrap();
        assert_eq!(replaced["deleted"], json!(2));
        assert_eq!(replaced["inserted"], json!(1));

        let mut upsert = open_writer(&profile, &table, WriteMode::Upsert);
        upsert
            .stage_rows(&[
                json!({"id":1,"label":"after"}),
                json!({"id":3,"label":"new"}),
            ])
            .unwrap();
        let merged = upsert.commit().unwrap();
        assert_eq!(merged["inserted"], json!(1));
        assert_eq!(merged["updated"], json!(1));

        let mut duplicate = open_writer(&profile, &table, WriteMode::Upsert);
        duplicate
            .stage_rows(&[json!({"id":4,"label":"a"}), json!({"id":4,"label":"b"})])
            .unwrap();
        assert!(matches!(
            duplicate.commit(),
            Err(DowaiError::Write {
                code: WRITE_DUPLICATE_KEY,
                ..
            })
        ));
        let mut check = DatabaseAdapter::connect(&profile).unwrap();
        let rows = check
            .native_query(
                &format!("SELECT COUNT(*) AS DOWAI_COUNT FROM {quoted}"),
                &[],
                &|| false,
            )
            .unwrap();
        assert_eq!(rows[0]["DOWAI_COUNT"], json!("2"));
        drop(cleanup);
    }

    #[test]
    #[ignore = "requires an encrypted SQL Server test connection"]
    fn sqlserver_native_write_transaction() {
        exercise_native_writes("sqlserver");
    }

    #[test]
    #[ignore = "requires Oracle Client, native encryption and a test connection"]
    fn oracle_native_write_transaction() {
        exercise_native_writes("oracle");
    }
}

impl WriteSession {
    /// Authorizes the destination, then opens the session of its kind.
    pub(crate) fn open(
        root: &Path,
        configuration: &Value,
        request: WriteRequest,
        depth: usize,
        worker: crate::executor::WorkerSettings<'_>,
        cancel: CancellationToken,
    ) -> Result<Self> {
        enforce_external_write_policy(configuration)?;
        let target = request
            .target
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| invalid("target must be a non-empty profile name"))?
            .to_owned();
        let (_, profile) = resolve_profile(root, configuration, Some(&target))?;
        if profile.profile_type == "external" {
            return Ok(Self::Driver(
                crate::external_driver::ExternalDriverSession::open(
                    root, &profile, &request, depth, worker,
                )?,
            ));
        }
        Ok(Self::Sql(ExternalWrite::open(
            root,
            configuration,
            request,
            depth,
            cancel,
        )?))
    }

    pub(crate) fn depth(&self) -> usize {
        match self {
            Self::Sql(session) => session.depth(),
            Self::Driver(session) => session.depth(),
        }
    }

    pub(crate) fn stage_rows(&mut self, rows: &[Value]) -> Result<u64> {
        match self {
            Self::Sql(session) => session.stage_rows(rows),
            Self::Driver(session) => session.stage_rows(rows),
        }
    }

    pub(crate) fn commit(self) -> Result<Value> {
        match self {
            Self::Sql(session) => session.commit(),
            Self::Driver(session) => session.commit(),
        }
    }

    pub(crate) fn abort(self) {
        match self {
            Self::Sql(session) => session.abort(),
            Self::Driver(session) => session.abort(),
        }
    }
}
