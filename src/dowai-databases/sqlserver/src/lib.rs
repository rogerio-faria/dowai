//! Standalone asynchronous Microsoft SQL Server adapter using Tiberius (TDS).
//!
//! SQL Server connections use TCP and SQL Server authentication, so the crate works on
//! Windows as well as other Tokio-supported platforms. DDL materialization is transactional.
//! Request cancellation is intentionally not exposed: dropping an in-flight TDS future can
//! leave the connection out of sync, and Tiberius does not provide a safe cancellation handle.
//! A bounded connect timeout is supported, but per-query timeouts are not: timing out an
//! in-flight TDS request would cancel its future and require discarding the connection.

use std::collections::BTreeMap;
use std::env;
use std::fmt;
use std::future::Future;
use std::net::IpAddr;
use std::time::Duration;

use futures_util::TryStreamExt;
use serde_json::Value;
use tiberius::{AuthMethod, Client, ColumnData, Config, EncryptionLevel, QueryItem, ToSql};
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};

pub use tiberius::{QueryStream, Row};

const CONNECT_TIMEOUT_MAX: Duration = Duration::from_secs(600);
const IDENTIFIER_MAX_UTF16_UNITS: usize = 128;

/// Verified features and limitations of this adapter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capabilities {
    /// Materialization is performed in an explicit SQL Server transaction.
    pub transactional_ddl: bool,
    /// Identifiers are escaped as SQL Server bracket-delimited identifiers.
    pub identifier_quoting: bool,
    /// SQL Server schemas are supported for qualified relation names.
    pub schemas: bool,
    /// Explicit begin/commit/rollback is supported.
    pub transactions: bool,
    /// A target-shaped table can be used as a staging relation.
    pub staging: bool,
    /// JSON scalar values can be bound for append/replace/upsert writes.
    pub json_writes: bool,
    /// Result streams are available through Tiberius.
    pub query_stream: bool,
    /// JSON row queries can be consumed in caller-sized batches.
    pub json_row_batches: bool,
    /// Per-query timeout is unavailable because TDS request cancellation is not safe.
    pub query_timeout: bool,
    /// Safe cancellation of an in-flight operation is not exposed.
    pub cancellation: bool,
}

pub const SQLSERVER_CAPABILITIES: Capabilities = Capabilities {
    transactional_ddl: true,
    identifier_quoting: true,
    schemas: true,
    transactions: true,
    staging: true,
    json_writes: true,
    query_stream: true,
    json_row_batches: true,
    query_timeout: false,
    cancellation: false,
};

/// Whether transport encryption is mandatory or disabled.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TlsMode {
    #[default]
    Required,
    Disabled,
}

/// SQL Server connection settings. The password itself is never retained in this value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SqlServerProfile {
    pub host: String,
    pub port: u16,
    pub database: String,
    pub user: String,
    pub password_env: String,
    pub tls_mode: TlsMode,
    pub trust_server_certificate: bool,
    /// Bounds TCP connect and TDS login. Query execution itself has no timeout.
    pub connect_timeout: Duration,
}

impl SqlServerProfile {
    pub fn new(
        host: impl Into<String>,
        port: u16,
        database: impl Into<String>,
        user: impl Into<String>,
        password_env: impl Into<String>,
    ) -> Self {
        Self {
            host: host.into(),
            port,
            database: database.into(),
            user: user.into(),
            password_env: password_env.into(),
            tls_mode: TlsMode::Required,
            trust_server_certificate: false,
            connect_timeout: Duration::from_secs(10),
        }
    }

    pub fn validate(&self) -> Result<(), AdapterError> {
        for (field, value) in [
            ("host", self.host.as_str()),
            ("database", self.database.as_str()),
            ("user", self.user.as_str()),
        ] {
            if value.trim().is_empty() || value.chars().any(char::is_control) {
                return Err(AdapterError::InvalidProfile(field));
            }
        }
        if self.host.chars().any(char::is_whitespace) {
            return Err(AdapterError::InvalidProfile("host"));
        }
        if self.port == 0 {
            return Err(AdapterError::InvalidProfile("port"));
        }
        if !valid_env_var_name(&self.password_env) {
            return Err(AdapterError::InvalidProfile(
                "password environment variable",
            ));
        }
        if self.tls_mode == TlsMode::Disabled && !is_local_host(&self.host) {
            return Err(AdapterError::TlsDisabledForRemoteHost);
        }
        if self.connect_timeout.is_zero() || self.connect_timeout > CONNECT_TIMEOUT_MAX {
            return Err(AdapterError::InvalidProfile("connect timeout"));
        }
        Ok(())
    }
}

/// A SQL Server table or view to create.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Materialization {
    Table,
    View,
}

/// Column metadata returned by [`SqlServerAdapter::inspect_table_columns`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColumnInfo {
    pub name: String,
    pub data_type: String,
    pub nullable: bool,
    pub ordinal_position: i32,
}

/// One JSON-compatible object row. Column order is deterministic and independent of input order.
pub type JsonRecord = BTreeMap<String, Value>;

/// Write policy for [`SqlServerAdapter::write_rows`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WriteMode {
    Append,
    Replace,
    Upsert { keys: Vec<String> },
}

/// Sanitized adapter failures; server/driver messages are not retained because they can contain
/// query text or connection details.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdapterError {
    InvalidProfile(&'static str),
    MissingPassword,
    InvalidPasswordEnvironment,
    TlsDisabledForRemoteHost,
    InvalidIdentifier,
    EmptyQuery,
    ConnectionFailed,
    TransientFailure,
    QueryFailed,
    Timeout,
    LoginTimeout,
    TransactionAlreadyOpen,
    NoTransactionOpen,
    InvalidWrite(&'static str),
}

impl AdapterError {
    pub fn is_transient(self) -> bool {
        matches!(
            self,
            Self::TransientFailure | Self::Timeout | Self::LoginTimeout
        )
    }
}

impl fmt::Display for AdapterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidProfile(field) => write!(f, "invalid SQL Server profile: {field}"),
            Self::MissingPassword => f.write_str("password environment variable is not set"),
            Self::InvalidPasswordEnvironment => {
                f.write_str("password environment variable is not valid Unicode")
            }
            Self::TlsDisabledForRemoteHost => {
                f.write_str("TLS may only be disabled for a local SQL Server host")
            }
            Self::InvalidIdentifier => f.write_str("invalid SQL Server identifier"),
            Self::EmptyQuery => f.write_str("SQL query must not be empty"),
            Self::ConnectionFailed => f.write_str("SQL Server connection failed"),
            Self::TransientFailure => f.write_str("transient SQL Server connection failure"),
            Self::QueryFailed => f.write_str("SQL Server query failed"),
            Self::Timeout => f.write_str("SQL Server connection timed out"),
            Self::LoginTimeout => f.write_str("SQL Server login timed out"),
            Self::TransactionAlreadyOpen => f.write_str("SQL Server transaction is already open"),
            Self::NoTransactionOpen => f.write_str("no SQL Server transaction is open"),
            Self::InvalidWrite(reason) => write!(f, "invalid SQL Server write: {reason}"),
        }
    }
}

impl std::error::Error for AdapterError {}

/// Quote one SQL Server identifier, escaping closing brackets.
pub fn quote_identifier(identifier: &str) -> Result<String, AdapterError> {
    if identifier.is_empty()
        || identifier.chars().any(char::is_control)
        || identifier.encode_utf16().count() > IDENTIFIER_MAX_UTF16_UNITS
    {
        return Err(AdapterError::InvalidIdentifier);
    }
    Ok(format!("[{}]", identifier.replace(']', "]]")))
}

/// Quote a table name or `schema.table` name. Dots are only separators between two components.
pub fn quote_qualified_identifier(name: &str) -> Result<String, AdapterError> {
    let mut parts = name.split('.');
    let first = parts.next().ok_or(AdapterError::InvalidIdentifier)?;
    let second = parts.next();
    if parts.next().is_some() {
        return Err(AdapterError::InvalidIdentifier);
    }
    match second {
        Some(table) => Ok(format!(
            "{}.{}",
            quote_identifier(first)?,
            quote_identifier(table)?
        )),
        None => quote_identifier(first),
    }
}

fn valid_env_var_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c == '_' || c.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

fn is_local_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<IpAddr>()
            .map(|address| address.is_loopback())
            .unwrap_or(false)
}

fn password_from_lookup(
    name: &str,
    lookup: impl FnOnce(&str) -> Result<String, env::VarError>,
) -> Result<String, AdapterError> {
    match lookup(name) {
        Ok(value) => Ok(value),
        Err(env::VarError::NotPresent) => Err(AdapterError::MissingPassword),
        Err(env::VarError::NotUnicode(_)) => Err(AdapterError::InvalidPasswordEnvironment),
    }
}

fn resolve_password(name: &str) -> Result<String, AdapterError> {
    password_from_lookup(name, |variable| env::var(variable))
}

fn map_tiberius_error(error: tiberius::error::Error) -> AdapterError {
    match error {
        tiberius::error::Error::Io { .. } => AdapterError::TransientFailure,
        tiberius::error::Error::Server(_) => AdapterError::QueryFailed,
        _ => AdapterError::QueryFailed,
    }
}

type TdsClient = Client<Compat<TcpStream>>;

/// Connected SQL Server client.
pub struct SqlServerAdapter {
    client: TdsClient,
    in_transaction: bool,
}

impl SqlServerAdapter {
    /// Connects using the password stored in the profile's environment variable.
    pub async fn connect(profile: &SqlServerProfile) -> Result<Self, AdapterError> {
        profile.validate()?;
        let password = resolve_password(&profile.password_env)?;

        let mut config = Config::new();
        config.host(&profile.host);
        config.port(profile.port);
        config.database(&profile.database);
        config.authentication(AuthMethod::sql_server(&profile.user, password));
        config.encryption(match profile.tls_mode {
            TlsMode::Required => EncryptionLevel::Required,
            TlsMode::Disabled => EncryptionLevel::NotSupported,
        });
        if profile.trust_server_certificate {
            config.trust_cert();
        }

        let stream = timeout(
            profile.connect_timeout,
            TcpStream::connect(config.get_addr()),
        )
        .await
        .map_err(|_| AdapterError::Timeout)?
        .map_err(|_| AdapterError::ConnectionFailed)?;
        stream
            .set_nodelay(true)
            .map_err(|_| AdapterError::ConnectionFailed)?;
        let client = timeout(
            profile.connect_timeout,
            Client::connect(config, stream.compat_write()),
        )
        .await
        .map_err(|_| AdapterError::LoginTimeout)?
        .map_err(|error| match map_tiberius_error(error) {
            AdapterError::TransientFailure => AdapterError::TransientFailure,
            _ => AdapterError::ConnectionFailed,
        })?;
        Ok(Self {
            client,
            in_transaction: false,
        })
    }

    pub fn capabilities(&self) -> Capabilities {
        SQLSERVER_CAPABILITIES
    }

    /// Confirms connectivity with a lightweight server query.
    pub async fn test_connection(&mut self) -> Result<Capabilities, AdapterError> {
        self.client
            .simple_query("SELECT 1")
            .await
            .map_err(map_tiberius_error)?
            .into_first_result()
            .await
            .map_err(map_tiberius_error)?;
        Ok(SQLSERVER_CAPABILITIES)
    }

    /// Executes SQL without parameters and returns the total affected row count.
    pub async fn execute_sql(&mut self, sql: &str) -> Result<u64, AdapterError> {
        if sql.trim().is_empty() {
            return Err(AdapterError::EmptyQuery);
        }
        let result = self
            .client
            .execute(sql, &[])
            .await
            .map_err(map_tiberius_error)?;
        Ok(result.total())
    }

    /// Executes a SQL batch in the connection's session, without a parameterized RPC.
    pub async fn execute_batch(&mut self, sql: &str) -> Result<(), AdapterError> {
        if sql.trim().is_empty() {
            return Err(AdapterError::EmptyQuery);
        }
        self.client
            .simple_query(sql)
            .await
            .map_err(map_tiberius_error)?
            .into_results()
            .await
            .map_err(map_tiberius_error)?;
        Ok(())
    }

    /// Executes a statement with JSON-compatible bound parameters (`@P1`, `@P2`, ...).
    ///
    /// JSON arrays and objects are bound as JSON text; JSON null is a SQL NULL. Tiberius infers
    /// each parameter's SQL type from the JSON value, so date/time values should be supplied as
    /// ISO-formatted strings and converted by SQL Server as needed.
    pub async fn execute_json(
        &mut self,
        sql: &str,
        parameters: &[Value],
    ) -> Result<u64, AdapterError> {
        if sql.trim().is_empty() {
            return Err(AdapterError::EmptyQuery);
        }
        let values = parameters
            .iter()
            .map(SqlValue::from_json)
            .collect::<Result<Vec<_>, _>>()?;
        let parameters = values
            .iter()
            .map(|value| value as &dyn ToSql)
            .collect::<Vec<_>>();
        let result = self
            .client
            .execute(sql, &parameters)
            .await
            .map_err(map_tiberius_error)?;
        Ok(result.total())
    }

    /// Executes a parameterized query and collects its first result set.
    ///
    /// Returned rows retain Tiberius' typed access API; this avoids lossy conversion of SQL
    /// Server decimals, timestamps, binary values, and large integers to JSON.
    pub async fn query(
        &mut self,
        sql: &str,
        parameters: &[&dyn ToSql],
    ) -> Result<Vec<Row>, AdapterError> {
        if sql.trim().is_empty() {
            return Err(AdapterError::EmptyQuery);
        }
        self.client
            .query(sql, parameters)
            .await
            .map_err(map_tiberius_error)?
            .into_first_result()
            .await
            .map_err(map_tiberius_error)
    }

    /// Collects JSON-compatible rows from the first result set.
    ///
    /// Each row is an object keyed by column alias. SQL NULL becomes JSON null; every non-null
    /// SQL value becomes a string to avoid lossy implicit type conversions. Binary data is
    /// lowercase hexadecimal, and SQL XML/date/time driver values use their debug representation.
    /// Queries with duplicate column names should alias them uniquely.
    pub async fn query_json_rows(
        &mut self,
        sql: &str,
        parameters: &[Value],
    ) -> Result<Vec<JsonRecord>, AdapterError> {
        let mut rows = Vec::new();
        self.for_each_json_batch(sql, parameters, usize::MAX, |batch| {
            rows.extend(batch);
            async { Ok(()) }
        })
        .await?;
        Ok(rows)
    }

    /// Reads the first result set in bounded batches and calls `consume` for each batch.
    ///
    /// Unlike [`Self::query_json_rows`], this does not retain prior batches. Parameter values
    /// use the same JSON-to-TDS conversion as [`Self::execute_json`].
    pub async fn for_each_json_batch<F, Fut>(
        &mut self,
        sql: &str,
        parameters: &[Value],
        batch_size: usize,
        mut consume: F,
    ) -> Result<(), AdapterError>
    where
        F: FnMut(Vec<JsonRecord>) -> Fut,
        Fut: Future<Output = Result<(), AdapterError>>,
    {
        if sql.trim().is_empty() {
            return Err(AdapterError::EmptyQuery);
        }
        if batch_size == 0 {
            return Err(AdapterError::InvalidWrite(
                "batch size must be greater than zero",
            ));
        }
        let values = parameters
            .iter()
            .map(SqlValue::from_json)
            .collect::<Result<Vec<_>, _>>()?;
        let bound_parameters = values
            .iter()
            .map(|value| value as &dyn ToSql)
            .collect::<Vec<_>>();
        let mut stream = self
            .client
            .query(sql, &bound_parameters)
            .await
            .map_err(map_tiberius_error)?;
        let mut batch = Vec::with_capacity(batch_size.min(1024));
        let mut conversion_error = None;
        let mut consumer_error = None;
        while let Some(item) = stream.try_next().await.map_err(map_tiberius_error)? {
            if let QueryItem::Row(row) = item {
                if row.result_index() == 0 && conversion_error.is_none() && consumer_error.is_none()
                {
                    match row_to_json(row) {
                        Ok(row) => batch.push(row),
                        Err(error) => conversion_error = Some(error),
                    }
                    if batch.len() >= batch_size && consumer_error.is_none() {
                        if let Err(error) = consume(std::mem::take(&mut batch)).await {
                            consumer_error = Some(error);
                        }
                    }
                }
            }
        }
        if conversion_error.is_none() && consumer_error.is_none() && !batch.is_empty() {
            if let Err(error) = consume(batch).await {
                consumer_error = Some(error);
            }
        }
        conversion_error.or(consumer_error).map_or(Ok(()), Err)
    }

    /// Collects the first result set grouped into batches of `batch_size`.
    ///
    /// The batch callback API [`Self::for_each_json_batch`] should be used for large results,
    /// since this convenience method retains all returned batches.
    pub async fn query_json_batches(
        &mut self,
        sql: &str,
        parameters: &[Value],
        batch_size: usize,
    ) -> Result<Vec<Vec<JsonRecord>>, AdapterError> {
        let mut batches = Vec::new();
        self.for_each_json_batch(sql, parameters, batch_size, |batch| {
            batches.push(batch);
            async { Ok(()) }
        })
        .await?;
        Ok(batches)
    }

    /// Starts a transaction. The adapter tracks state to reject mismatched commit/rollback calls.
    pub async fn begin_transaction(&mut self) -> Result<(), AdapterError> {
        if self.in_transaction {
            return Err(AdapterError::TransactionAlreadyOpen);
        }
        self.transaction_command("BEGIN TRANSACTION").await?;
        self.in_transaction = true;
        Ok(())
    }

    /// Commits the active transaction.
    pub async fn commit_transaction(&mut self) -> Result<(), AdapterError> {
        if !self.in_transaction {
            return Err(AdapterError::NoTransactionOpen);
        }
        self.transaction_command("COMMIT TRANSACTION").await?;
        self.in_transaction = false;
        Ok(())
    }

    /// Rolls back the active transaction.
    pub async fn rollback_transaction(&mut self) -> Result<(), AdapterError> {
        if !self.in_transaction {
            return Err(AdapterError::NoTransactionOpen);
        }
        self.transaction_command("ROLLBACK TRANSACTION").await?;
        self.in_transaction = false;
        Ok(())
    }

    async fn transaction_command(&mut self, sql: &str) -> Result<(), AdapterError> {
        self.client
            .simple_query(sql)
            .await
            .map_err(map_tiberius_error)?
            .into_results()
            .await
            .map_err(map_tiberius_error)?;
        Ok(())
    }

    /// Starts a parameterized streaming query. The caller must fully consume the returned stream
    /// before reusing this adapter; dropping it early makes the connection unusable, so callers
    /// should drop the adapter rather than issue another command.
    pub async fn query_stream<'a>(
        &'a mut self,
        sql: &'a str,
        parameters: &'a [&'a dyn ToSql],
    ) -> Result<QueryStream<'a>, AdapterError> {
        if sql.trim().is_empty() {
            return Err(AdapterError::EmptyQuery);
        }
        self.client
            .query(sql, parameters)
            .await
            .map_err(map_tiberius_error)
    }

    /// Creates an empty staging table with the same columns as `target`.
    pub async fn create_staging_table(
        &mut self,
        target: &str,
        staging: &str,
    ) -> Result<(), AdapterError> {
        let target = quote_qualified_identifier(target)?;
        let staging = quote_qualified_identifier(staging)?;
        self.execute_transactional_ddl(&format!("SELECT TOP (0) * INTO {staging} FROM {target}"))
            .await?;
        Ok(())
    }

    /// Inserts JSON rows into a staging relation.
    pub async fn write_staging_rows(
        &mut self,
        staging: &str,
        rows: &[JsonRecord],
    ) -> Result<u64, AdapterError> {
        self.append_rows(staging, rows).await
    }

    /// Appends JSON rows to a relation using bound TDS parameters.
    pub async fn append_rows(
        &mut self,
        relation: &str,
        rows: &[JsonRecord],
    ) -> Result<u64, AdapterError> {
        let relation = quote_qualified_identifier(relation)?;
        let (columns, rows) = validate_rows(rows)?;
        if rows.is_empty() {
            return Ok(0);
        }
        let owns_transaction = !self.in_transaction;
        if owns_transaction {
            self.begin_transaction().await?;
        }
        let result = self.insert_rows(&relation, &columns, &rows).await;
        self.finish_owned_transaction(owns_transaction, result)
            .await
    }

    /// Writes rows using the selected append, replace, or upsert policy.
    pub async fn write_rows(
        &mut self,
        relation: &str,
        rows: &[JsonRecord],
        mode: WriteMode,
    ) -> Result<u64, AdapterError> {
        match mode {
            WriteMode::Append => self.append_rows(relation, rows).await,
            WriteMode::Replace => self.replace_rows(relation, rows).await,
            WriteMode::Upsert { keys } => self.upsert_rows(relation, rows, &keys).await,
        }
    }

    /// Deletes all current rows and inserts the provided replacement rows atomically.
    pub async fn replace_rows(
        &mut self,
        relation: &str,
        rows: &[JsonRecord],
    ) -> Result<u64, AdapterError> {
        let relation = quote_qualified_identifier(relation)?;
        let (columns, rows) = validate_rows(rows)?;
        let owns_transaction = !self.in_transaction;
        if owns_transaction {
            self.begin_transaction().await?;
        }
        let result = async {
            self.execute_sql(&format!("DELETE FROM {relation}")).await?;
            if rows.is_empty() {
                Ok(0)
            } else {
                self.insert_rows(&relation, &columns, &rows).await
            }
        }
        .await;
        self.finish_owned_transaction(owns_transaction, result)
            .await
    }

    /// Inserts or updates each row by the given key columns, atomically.
    ///
    /// SQL Server `MERGE ... WITH (HOLDLOCK)` is used for each row in the same transaction.
    /// Duplicate keys within the input are applied in order; the last row wins.
    pub async fn upsert_rows(
        &mut self,
        relation: &str,
        rows: &[JsonRecord],
        keys: &[String],
    ) -> Result<u64, AdapterError> {
        let relation = quote_qualified_identifier(relation)?;
        let (columns, rows) = validate_rows(rows)?;
        validate_keys(&columns, keys)?;
        if rows.is_empty() {
            return Ok(0);
        }
        let owns_transaction = !self.in_transaction;
        if owns_transaction {
            self.begin_transaction().await?;
        }
        let mut affected = 0;
        let result = async {
            for row in &rows {
                affected += self.merge_row(&relation, &columns, row, keys).await?;
            }
            Ok(affected)
        }
        .await;
        self.finish_owned_transaction(owns_transaction, result)
            .await
    }

    /// Appends all rows from staging into the target relation.
    pub async fn append_from_staging(
        &mut self,
        target: &str,
        staging: &str,
    ) -> Result<u64, AdapterError> {
        self.transfer_from_staging(target, staging, TransferMode::Append, &[])
            .await
    }

    /// Replaces target rows with all rows from staging in one transaction.
    pub async fn replace_from_staging(
        &mut self,
        target: &str,
        staging: &str,
    ) -> Result<u64, AdapterError> {
        self.transfer_from_staging(target, staging, TransferMode::Replace, &[])
            .await
    }

    /// Upserts staging rows into target by key columns.
    pub async fn upsert_from_staging(
        &mut self,
        target: &str,
        staging: &str,
        keys: &[String],
    ) -> Result<u64, AdapterError> {
        self.transfer_from_staging(target, staging, TransferMode::Upsert, keys)
            .await
    }

    /// Drops a staging relation if it exists. This DDL is transaction-capable on SQL Server.
    pub async fn drop_staging_table(&mut self, staging: &str) -> Result<(), AdapterError> {
        let staging = quote_qualified_identifier(staging)?;
        self.execute_sql(&format!("DROP TABLE IF EXISTS {staging}"))
            .await?;
        Ok(())
    }

    /// Returns column names, SQL Server type names, nullability, and ordinal positions.
    ///
    /// An unqualified table name is looked up in the caller's default schema; a qualified name
    /// must be supplied as `schema.table`.
    pub async fn inspect_table_columns(
        &mut self,
        table: &str,
    ) -> Result<Vec<ColumnInfo>, AdapterError> {
        let (schema, table) = split_relation_name(table)?;
        let rows = self
            .client
            .query(
                "SELECT c.name AS column_name, TYPE_NAME(c.user_type_id) AS data_type, \
                 c.is_nullable AS is_nullable, c.column_id AS ordinal_position \
                 FROM sys.columns AS c \
                 INNER JOIN sys.objects AS o ON o.object_id = c.object_id \
                 INNER JOIN sys.schemas AS s ON s.schema_id = o.schema_id \
                 WHERE s.name = @P1 AND o.name = @P2 AND o.type IN ('U', 'V') \
                 ORDER BY c.column_id",
                &[&schema, &table],
            )
            .await
            .map_err(map_tiberius_error)?
            .into_first_result()
            .await
            .map_err(map_tiberius_error)?;

        rows.into_iter()
            .map(|row| {
                let name = row
                    .try_get::<&str, _>("column_name")
                    .map_err(|_| AdapterError::QueryFailed)?
                    .ok_or(AdapterError::QueryFailed)?
                    .to_owned();
                let data_type = row
                    .try_get::<&str, _>("data_type")
                    .map_err(|_| AdapterError::QueryFailed)?
                    .ok_or(AdapterError::QueryFailed)?
                    .to_owned();
                let nullable = row
                    .try_get::<bool, _>("is_nullable")
                    .map_err(|_| AdapterError::QueryFailed)?
                    .ok_or(AdapterError::QueryFailed)?;
                let ordinal_position = row
                    .try_get::<i32, _>("ordinal_position")
                    .map_err(|_| AdapterError::QueryFailed)?
                    .ok_or(AdapterError::QueryFailed)?;
                Ok(ColumnInfo {
                    name,
                    data_type,
                    nullable,
                    ordinal_position,
                })
            })
            .collect()
    }

    /// Creates a table from a SELECT or creates/replaces a view from a SELECT.
    ///
    /// Replacement is only attempted when `allow_replace` is true. Both operations are wrapped
    /// in a transaction; SQL Server rolls back the DDL if materialization fails. For view DDL,
    /// dynamic SQL is used because CREATE VIEW must be the first statement in its batch.
    pub async fn materialize_sql(
        &mut self,
        relation: &str,
        kind: Materialization,
        select_sql: &str,
        allow_replace: bool,
    ) -> Result<(), AdapterError> {
        let relation = quote_qualified_identifier(relation)?;
        self.materialize_quoted(&relation, kind, select_sql, allow_replace)
            .await
    }

    /// Materializes a model whose name is a single identifier, including any literal dots.
    pub async fn materialize_unqualified(
        &mut self,
        relation: &str,
        kind: Materialization,
        select_sql: &str,
        allow_replace: bool,
    ) -> Result<(), AdapterError> {
        let relation = quote_identifier(relation)?;
        self.materialize_quoted(&relation, kind, select_sql, allow_replace)
            .await
    }

    async fn materialize_quoted(
        &mut self,
        relation: &str,
        kind: Materialization,
        select_sql: &str,
        allow_replace: bool,
    ) -> Result<(), AdapterError> {
        if select_sql.trim().is_empty() {
            return Err(AdapterError::EmptyQuery);
        }
        let query = select_sql.trim().trim_end_matches(';').trim();
        if query.is_empty() {
            return Err(AdapterError::EmptyQuery);
        }

        let sql = match (kind, allow_replace) {
            (Materialization::Table, true) => format!(
                "DROP TABLE IF EXISTS {relation}; \
                 SELECT * INTO {relation} FROM ({query}) AS [__dowai_source]"
            ),
            (Materialization::Table, false) => {
                format!("SELECT * INTO {relation} FROM ({query}) AS [__dowai_source]")
            }
            (Materialization::View, replace) => {
                let operation = if replace {
                    "CREATE OR ALTER VIEW"
                } else {
                    "CREATE VIEW"
                };
                let view_ddl = format!("{operation} {relation} AS {query}").replace('\'', "''");
                format!("EXEC sys.sp_executesql N'{view_ddl}'")
            }
        };
        self.execute_transactional_ddl(&sql).await?;
        Ok(())
    }

    async fn execute_transactional_ddl(&mut self, sql: &str) -> Result<(), AdapterError> {
        if self.in_transaction {
            self.execute_sql(&format!("SET XACT_ABORT ON; {sql}"))
                .await?;
        } else {
            self.execute_sql(&format!(
                "SET XACT_ABORT ON; BEGIN TRY BEGIN TRANSACTION; {sql}; \
                 COMMIT TRANSACTION; END TRY BEGIN CATCH \
                 IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION; THROW; END CATCH"
            ))
            .await?;
        }
        Ok(())
    }

    async fn insert_rows(
        &mut self,
        relation: &str,
        columns: &[String],
        rows: &[Vec<Value>],
    ) -> Result<u64, AdapterError> {
        if columns.is_empty() {
            return Err(AdapterError::InvalidWrite(
                "rows must contain at least one column",
            ));
        }
        let quoted_columns = columns
            .iter()
            .map(|column| quote_identifier(column))
            .collect::<Result<Vec<_>, _>>()?
            .join(", ");
        let markers = (1..=columns.len())
            .map(|index| format!("@P{index}"))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!("INSERT INTO {relation} ({quoted_columns}) VALUES ({markers})");
        let mut affected = 0;
        for row in rows {
            let values = row
                .iter()
                .map(SqlValue::from_json)
                .collect::<Result<Vec<_>, _>>()?;
            let parameters = values
                .iter()
                .map(|value| value as &dyn ToSql)
                .collect::<Vec<_>>();
            affected += self
                .client
                .execute(&sql, &parameters)
                .await
                .map_err(map_tiberius_error)?
                .total();
        }
        Ok(affected)
    }

    async fn merge_row(
        &mut self,
        relation: &str,
        columns: &[String],
        row: &[Value],
        keys: &[String],
    ) -> Result<u64, AdapterError> {
        let quoted_columns = columns
            .iter()
            .map(|column| quote_identifier(column))
            .collect::<Result<Vec<_>, _>>()?;
        let markers = (1..=columns.len())
            .map(|index| format!("@P{index}"))
            .collect::<Vec<_>>()
            .join(", ");
        let on = keys
            .iter()
            .map(|key| {
                let key = quote_identifier(key)?;
                Ok(format!("[target].{key} = [source].{key}"))
            })
            .collect::<Result<Vec<String>, AdapterError>>()?
            .join(" AND ");
        let updates = columns
            .iter()
            .filter(|column| !keys.contains(column))
            .map(|column| {
                let column = quote_identifier(column)?;
                Ok(format!("[target].{column} = [source].{column}"))
            })
            .collect::<Result<Vec<String>, AdapterError>>()?;
        let update_clause = if updates.is_empty() {
            format!(
                "UPDATE SET [target].{} = [source].{}",
                quoted_columns[0], quoted_columns[0]
            )
        } else {
            format!("UPDATE SET {}", updates.join(", "))
        };
        let insert_values = quoted_columns
            .iter()
            .map(|column| format!("[source].{column}"))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "MERGE INTO {relation} WITH (HOLDLOCK) AS [target] \
             USING (VALUES ({markers})) AS [source] ({}) ON {on} \
             WHEN MATCHED THEN {update_clause} \
             WHEN NOT MATCHED BY TARGET THEN INSERT ({}) VALUES ({insert_values});",
            quoted_columns.join(", "),
            quoted_columns.join(", ")
        );
        let values = row
            .iter()
            .map(SqlValue::from_json)
            .collect::<Result<Vec<_>, _>>()?;
        let parameters = values
            .iter()
            .map(|value| value as &dyn ToSql)
            .collect::<Vec<_>>();
        Ok(self
            .client
            .execute(&sql, &parameters)
            .await
            .map_err(map_tiberius_error)?
            .total())
    }

    async fn finish_owned_transaction(
        &mut self,
        owns_transaction: bool,
        result: Result<u64, AdapterError>,
    ) -> Result<u64, AdapterError> {
        if !owns_transaction {
            return result;
        }
        match result {
            Ok(affected) => {
                self.commit_transaction().await?;
                Ok(affected)
            }
            Err(error) => {
                let _ = self.rollback_transaction().await;
                Err(error)
            }
        }
    }

    async fn transfer_from_staging(
        &mut self,
        target: &str,
        staging: &str,
        mode: TransferMode,
        keys: &[String],
    ) -> Result<u64, AdapterError> {
        let target = quote_qualified_identifier(target)?;
        let staging = quote_qualified_identifier(staging)?;
        if matches!(mode, TransferMode::Append | TransferMode::Replace) {
            let owns_transaction = !self.in_transaction;
            if owns_transaction {
                self.begin_transaction().await?;
            }
            let result = async {
                if matches!(mode, TransferMode::Replace) {
                    self.execute_sql(&format!("DELETE FROM {target}")).await?;
                }
                let count = self
                    .execute_sql(&format!("INSERT INTO {target} SELECT * FROM {staging}"))
                    .await?;
                Ok(count)
            }
            .await;
            return self
                .finish_owned_transaction(owns_transaction, result)
                .await;
        }

        let target_columns = self.inspect_table_columns(&target).await?;
        let columns = target_columns
            .iter()
            .map(|column| column.name.clone())
            .collect::<Vec<_>>();
        validate_keys(&columns, keys)?;
        let quoted_columns = columns
            .iter()
            .map(|column| quote_identifier(column))
            .collect::<Result<Vec<_>, _>>()?;
        let on = keys
            .iter()
            .map(|key| {
                let key = quote_identifier(key)?;
                Ok(format!("[target].{key} = [source].{key}"))
            })
            .collect::<Result<Vec<String>, AdapterError>>()?
            .join(" AND ");
        let updates = columns
            .iter()
            .filter(|column| !keys.contains(column))
            .map(|column| {
                let column = quote_identifier(column)?;
                Ok(format!("[target].{column} = [source].{column}"))
            })
            .collect::<Result<Vec<String>, AdapterError>>()?;
        let update_clause = if updates.is_empty() {
            format!(
                "UPDATE SET [target].{} = [source].{}",
                quoted_columns[0], quoted_columns[0]
            )
        } else {
            format!("UPDATE SET {}", updates.join(", "))
        };
        let sql = format!(
            "MERGE INTO {target} WITH (HOLDLOCK) AS [target] \
             USING {staging} AS [source] ON {on} \
             WHEN MATCHED THEN {update_clause} \
             WHEN NOT MATCHED BY TARGET THEN INSERT ({}) VALUES ({});",
            quoted_columns.join(", "),
            quoted_columns
                .iter()
                .map(|column| format!("[source].{column}"))
                .collect::<Vec<_>>()
                .join(", ")
        );
        let owns_transaction = !self.in_transaction;
        if owns_transaction {
            self.begin_transaction().await?;
        }
        let result = self.execute_sql(&sql).await;
        self.finish_owned_transaction(owns_transaction, result)
            .await
    }
}

#[derive(Clone, Copy)]
enum TransferMode {
    Append,
    Replace,
    Upsert,
}

enum SqlValue {
    Null,
    Bool(bool),
    I64(i64),
    F64(f64),
    Text(String),
}

fn row_to_json(row: Row) -> Result<JsonRecord, AdapterError> {
    let columns = row
        .columns()
        .iter()
        .map(|column| column.name().to_owned())
        .collect::<Vec<_>>();
    let mut record = JsonRecord::new();
    for (name, value) in columns.into_iter().zip(row) {
        if record.insert(name, column_data_to_json(value)).is_some() {
            return Err(AdapterError::QueryFailed);
        }
    }
    Ok(record)
}

fn column_data_to_json(value: ColumnData<'_>) -> Value {
    match value {
        ColumnData::U8(value) => value.map_or(Value::Null, |v| Value::String(v.to_string())),
        ColumnData::I16(value) => value.map_or(Value::Null, |v| Value::String(v.to_string())),
        ColumnData::I32(value) => value.map_or(Value::Null, |v| Value::String(v.to_string())),
        ColumnData::I64(value) => value.map_or(Value::Null, |v| Value::String(v.to_string())),
        ColumnData::F32(value) => value.map_or(Value::Null, |v| Value::String(v.to_string())),
        ColumnData::F64(value) => value.map_or(Value::Null, |v| Value::String(v.to_string())),
        ColumnData::Bit(value) => value.map_or(Value::Null, |v| Value::String(v.to_string())),
        ColumnData::String(value) => value.map_or(Value::Null, |v| Value::String(v.into_owned())),
        ColumnData::Guid(value) => value.map_or(Value::Null, |v| Value::String(v.to_string())),
        ColumnData::Binary(value) => value.map_or(Value::Null, |v| {
            Value::String(
                v.iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>(),
            )
        }),
        ColumnData::Numeric(value) => value.map_or(Value::Null, |v| {
            Value::String(format_numeric(v.value(), v.scale()))
        }),
        ColumnData::Xml(value) => value.map_or(Value::Null, |v| Value::String(format!("{v:?}"))),
        ColumnData::DateTime(value) => {
            value.map_or(Value::Null, |v| Value::String(format!("{v:?}")))
        }
        ColumnData::SmallDateTime(value) => {
            value.map_or(Value::Null, |v| Value::String(format!("{v:?}")))
        }
        ColumnData::Time(value) => value.map_or(Value::Null, |v| Value::String(format!("{v:?}"))),
        ColumnData::Date(value) => value.map_or(Value::Null, |v| Value::String(format!("{v:?}"))),
        ColumnData::DateTime2(value) => {
            value.map_or(Value::Null, |v| Value::String(format!("{v:?}")))
        }
        ColumnData::DateTimeOffset(value) => {
            value.map_or(Value::Null, |v| Value::String(format!("{v:?}")))
        }
    }
}

fn format_numeric(value: i128, scale: u8) -> String {
    if scale == 0 {
        return value.to_string();
    }
    let divisor = 10_u128.pow(u32::from(scale));
    let magnitude = value.unsigned_abs();
    let integer = magnitude / divisor;
    let fraction = format!(
        "{:0width$}",
        magnitude % divisor,
        width = usize::from(scale)
    );
    format!(
        "{}{}.{}",
        if value < 0 { "-" } else { "" },
        integer,
        fraction
    )
}

impl SqlValue {
    fn from_json(value: &Value) -> Result<Self, AdapterError> {
        Ok(match value {
            Value::Null => Self::Null,
            Value::Bool(value) => Self::Bool(*value),
            Value::Number(value) => {
                if let Some(value) = value.as_i64() {
                    Self::I64(value)
                } else if let Some(value) = value.as_u64() {
                    match i64::try_from(value) {
                        Ok(value) => Self::I64(value),
                        Err(_) => Self::Text(value.to_string()),
                    }
                } else {
                    Self::F64(
                        value
                            .as_f64()
                            .ok_or(AdapterError::InvalidWrite("unsupported JSON number"))?,
                    )
                }
            }
            Value::String(value) => Self::Text(value.clone()),
            Value::Array(_) | Value::Object(_) => Self::Text(
                serde_json::to_string(value)
                    .map_err(|_| AdapterError::InvalidWrite("cannot encode JSON value"))?,
            ),
        })
    }
}

impl ToSql for SqlValue {
    fn to_sql(&self) -> ColumnData<'_> {
        match self {
            Self::Null => ColumnData::String(None),
            Self::Bool(value) => ColumnData::Bit(Some(*value)),
            Self::I64(value) => ColumnData::I64(Some(*value)),
            Self::F64(value) => ColumnData::F64(Some(*value)),
            Self::Text(value) => ColumnData::String(Some(std::borrow::Cow::Borrowed(value))),
        }
    }
}

fn validate_rows(rows: &[JsonRecord]) -> Result<(Vec<String>, Vec<Vec<Value>>), AdapterError> {
    let Some(first) = rows.first() else {
        return Ok((Vec::new(), Vec::new()));
    };
    if first.is_empty() {
        return Err(AdapterError::InvalidWrite("row has no columns"));
    }
    let columns = first.keys().cloned().collect::<Vec<_>>();
    for column in &columns {
        quote_identifier(column)?;
    }
    let mut values = Vec::with_capacity(rows.len());
    for row in rows {
        if row.len() != columns.len() || columns.iter().any(|column| !row.contains_key(column)) {
            return Err(AdapterError::InvalidWrite(
                "all rows must contain the same columns",
            ));
        }
        values.push(columns.iter().map(|column| row[column].clone()).collect());
    }
    Ok((columns, values))
}

fn validate_keys(columns: &[String], keys: &[String]) -> Result<(), AdapterError> {
    if keys.is_empty() {
        return Err(AdapterError::InvalidWrite(
            "upsert requires at least one key",
        ));
    }
    for key in keys {
        quote_identifier(key)?;
        if !columns.contains(key) {
            return Err(AdapterError::InvalidWrite(
                "upsert keys must be present in the input columns",
            ));
        }
    }
    Ok(())
}

fn split_relation_name(name: &str) -> Result<(String, String), AdapterError> {
    let mut parts = name.split('.');
    let first = parts.next().ok_or(AdapterError::InvalidIdentifier)?;
    let second = parts.next();
    if parts.next().is_some() {
        return Err(AdapterError::InvalidIdentifier);
    }
    match second {
        Some(table) => {
            quote_identifier(first)?;
            quote_identifier(table)?;
            Ok((first.to_owned(), table.to_owned()))
        }
        None => {
            quote_identifier(first)?;
            Ok(("dbo".to_owned(), first.to_owned()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;

    fn profile() -> SqlServerProfile {
        SqlServerProfile::new("localhost", 1433, "dw", "user", "DOWAI_SQLSERVER_PASSWORD")
    }

    #[test]
    fn profile_validation_checks_required_fields_and_ranges() {
        assert!(profile().validate().is_ok());

        let mut invalid = profile();
        invalid.port = 0;
        assert_eq!(
            invalid.validate(),
            Err(AdapterError::InvalidProfile("port"))
        );

        let mut invalid = profile();
        invalid.host = "host name".into();
        assert_eq!(
            invalid.validate(),
            Err(AdapterError::InvalidProfile("host"))
        );

        let mut invalid = profile();
        invalid.password_env = "NOT-AN-ENV-NAME".into();
        assert_eq!(
            invalid.validate(),
            Err(AdapterError::InvalidProfile(
                "password environment variable"
            ))
        );

        let mut local_plaintext = profile();
        local_plaintext.tls_mode = TlsMode::Disabled;
        assert!(local_plaintext.validate().is_ok());

        let mut remote_plaintext = profile();
        remote_plaintext.host = "db.example.test".into();
        remote_plaintext.tls_mode = TlsMode::Disabled;
        assert_eq!(
            remote_plaintext.validate(),
            Err(AdapterError::TlsDisabledForRemoteHost)
        );
    }

    #[test]
    fn identifier_quoting_escapes_closing_brackets_and_rejects_bad_names() {
        assert_eq!(quote_identifier("odd]name").unwrap(), "[odd]]name]");
        assert_eq!(
            quote_qualified_identifier("app.table").unwrap(),
            "[app].[table]"
        );
        assert_eq!(
            quote_identifier("name; DROP TABLE x").unwrap(),
            "[name; DROP TABLE x]"
        );
        assert_eq!(
            quote_qualified_identifier("a.b.c"),
            Err(AdapterError::InvalidIdentifier)
        );
        assert_eq!(
            quote_identifier("bad\u{0000}name"),
            Err(AdapterError::InvalidIdentifier)
        );
    }

    #[test]
    fn password_resolution_distinguishes_missing_empty_and_non_unicode_values() {
        let password = password_from_lookup("PASSWORD", |_| Ok("  secret  ".to_owned())).unwrap();
        assert_eq!(password, "  secret  ");
        assert_eq!(
            password_from_lookup("PASSWORD", |_| Err(env::VarError::NotPresent)),
            Err(AdapterError::MissingPassword)
        );
        #[cfg(unix)]
        assert_eq!(
            password_from_lookup("PASSWORD", |_| {
                use std::os::unix::ffi::OsStringExt;
                Err(env::VarError::NotUnicode(std::ffi::OsString::from_vec(
                    vec![0xff],
                )))
            }),
            Err(AdapterError::InvalidPasswordEnvironment)
        );
    }

    #[test]
    fn tiberius_errors_map_to_sanitized_categories() {
        let timeout_error = tiberius::error::Error::Io {
            kind: io::ErrorKind::TimedOut,
            message: "private connection detail".to_owned(),
        };
        assert_eq!(
            map_tiberius_error(timeout_error),
            AdapterError::TransientFailure
        );
        let protocol_error =
            tiberius::error::Error::Protocol(std::borrow::Cow::Borrowed("private query detail"));
        assert_eq!(
            map_tiberius_error(protocol_error),
            AdapterError::QueryFailed
        );
        assert_eq!(
            AdapterError::QueryFailed.to_string(),
            "SQL Server query failed"
        );
    }

    #[test]
    fn json_rows_validate_uniform_columns_and_upsert_keys() {
        let row: JsonRecord = [
            ("id".to_owned(), Value::from(1)),
            ("payload".to_owned(), serde_json::json!({"ok": true})),
        ]
        .into_iter()
        .collect();
        let (columns, values) = validate_rows(std::slice::from_ref(&row)).unwrap();
        assert_eq!(columns, ["id", "payload"]);
        assert_eq!(values.len(), 1);
        assert!(validate_keys(&columns, &["id".to_owned()]).is_ok());
        assert!(validate_keys(&columns, &["missing".to_owned()]).is_err());

        let other: JsonRecord = [("id".to_owned(), Value::from(2))].into_iter().collect();
        assert!(validate_rows(&[row, other]).is_err());
    }

    #[test]
    fn json_scalars_and_structures_convert_to_safe_bound_values() {
        assert!(matches!(
            SqlValue::from_json(&Value::Null).unwrap(),
            SqlValue::Null
        ));
        assert!(matches!(
            SqlValue::from_json(&Value::from(true)).unwrap(),
            SqlValue::Bool(true)
        ));
        assert!(matches!(
            SqlValue::from_json(&serde_json::json!({"k": 1})).unwrap(),
            SqlValue::Text(ref value) if value == r#"{"k":1}"#
        ));
    }

    #[test]
    fn query_values_are_stringified_and_nulls_remain_null() {
        assert_eq!(
            column_data_to_json(ColumnData::I64(Some(9001))),
            Value::String("9001".into())
        );
        assert_eq!(column_data_to_json(ColumnData::String(None)), Value::Null);
        assert_eq!(
            column_data_to_json(ColumnData::Binary(Some(std::borrow::Cow::Borrowed(&[
                0x0a, 0xff
            ])))),
            Value::String("0aff".into())
        );
        assert_eq!(format_numeric(-1203, 2), "-12.03");
    }
}
