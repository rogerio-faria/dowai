//! Synchronous Oracle connector.
//!
//! The caller supplies identifiers as their exact Oracle spelling. Identifiers
//! are always quoted and embedded quotes are escaped before use in generated
//! SQL. SQL statements themselves are passed through unchanged.

use oracle::sql_type::ToSql;
use oracle::Connection;
use std::env;
use std::error::Error as StdError;
use std::fmt;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::Duration;

/// Oracle DDL implicitly commits and cannot be rolled back with this adapter.
pub const DDL_IS_TRANSACTIONAL: bool = false;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum TlsMode {
    #[default]
    Disabled,
    Required,
    /// Require Oracle native AES encryption and SHA-2 integrity over TCP.
    NativeRequired,
}

const NATIVE_ENCRYPTION_CONFIG: &str = "SQLNET.ENCRYPTION_CLIENT = REQUIRED\n\
SQLNET.ENCRYPTION_TYPES_CLIENT = (AES256, AES192, AES128)\n\
SQLNET.CRYPTO_CHECKSUM_CLIENT = REQUIRED\n\
SQLNET.CRYPTO_CHECKSUM_TYPES_CLIENT = (SHA512, SHA384, SHA256)\n";

enum ClientConfiguration {
    Uninitialized,
    Native(std::path::PathBuf),
    Other,
}

static CLIENT_CONFIGURATION: Mutex<ClientConfiguration> =
    Mutex::new(ClientConfiguration::Uninitialized);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConnectionProfile {
    pub user: String,
    pub host: String,
    pub port: u16,
    pub service: String,
    pub password_env: String,
    password: Option<SecretPassword>,
    pub tls_mode: TlsMode,
    pub connect_timeout_secs: Option<u32>,
    /// Bounds each database round trip, not the overall duration of a query.
    pub call_timeout_secs: Option<u32>,
}

#[derive(Clone, Eq, PartialEq)]
struct SecretPassword(String);

impl std::fmt::Debug for SecretPassword {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("<redacted>")
    }
}

impl ConnectionProfile {
    pub fn new(
        user: impl Into<String>,
        host: impl Into<String>,
        port: u16,
        service: impl Into<String>,
        password_env: impl Into<String>,
    ) -> Self {
        Self {
            user: user.into(),
            host: host.into(),
            port,
            service: service.into(),
            password_env: password_env.into(),
            password: None,
            tls_mode: TlsMode::default(),
            connect_timeout_secs: None,
            call_timeout_secs: None,
        }
    }

    pub fn with_password(mut self, password: String) -> Self {
        self.password = Some(SecretPassword(password));
        self
    }

    pub fn with_tls_mode(mut self, tls_mode: TlsMode) -> Self {
        self.tls_mode = tls_mode;
        self
    }

    pub fn with_connect_timeout_secs(mut self, timeout_secs: u32) -> Self {
        self.connect_timeout_secs = Some(timeout_secs);
        self
    }

    pub fn with_call_timeout_secs(mut self, timeout_secs: u32) -> Self {
        self.call_timeout_secs = Some(timeout_secs);
        self
    }

    pub fn validate(&self) -> Result<(), AdapterError> {
        if self.user.trim().is_empty() {
            return Err(AdapterError::InvalidProfile(
                "user must not be empty".to_owned(),
            ));
        }
        validate_descriptor_component("host", &self.host, false)?;
        if self.tls_mode == TlsMode::Disabled
            && !self.host.eq_ignore_ascii_case("localhost")
            && !self
                .host
                .parse::<IpAddr>()
                .map(|address| address.is_loopback())
                .unwrap_or(false)
        {
            return Err(AdapterError::InvalidProfile(
                "TLS may only be disabled for a local Oracle host".to_owned(),
            ));
        }
        validate_descriptor_component("service", &self.service, true)?;
        if self.port == 0 {
            return Err(AdapterError::InvalidProfile(
                "port must be between 1 and 65535".to_owned(),
            ));
        }
        if self.password.is_none() && self.password_env.trim().is_empty() {
            return Err(AdapterError::InvalidProfile(
                "password environment variable name must not be empty".to_owned(),
            ));
        }
        if self.connect_timeout_secs == Some(0) {
            return Err(AdapterError::InvalidProfile(
                "connect timeout must be greater than zero".to_owned(),
            ));
        }
        if self
            .call_timeout_secs
            .is_some_and(|seconds| seconds == 0 || seconds > u32::MAX / 1_000)
        {
            return Err(AdapterError::InvalidProfile(
                "call timeout is outside the supported range".to_owned(),
            ));
        }
        Ok(())
    }

    /// Builds the Oracle Net descriptor from the runtime's host/service fields.
    ///
    /// The descriptor is generated only for connection setup and is never
    /// logged. `Required` selects TCPS; wallet and certificate validation
    /// remain governed by the installed Oracle Client's network configuration.
    fn oracle_connect_descriptor(&self) -> Result<String, AdapterError> {
        self.validate()?;
        let protocol = match self.tls_mode {
            TlsMode::Disabled | TlsMode::NativeRequired => "TCP",
            TlsMode::Required => "TCPS",
        };
        let timeout = self
            .connect_timeout_secs
            .map(|seconds| format!("(CONNECT_TIMEOUT={seconds})"))
            .unwrap_or_default();
        Ok(format!(
            "(DESCRIPTION=(ADDRESS=(PROTOCOL={protocol})(HOST={})(PORT={})){timeout}(CONNECT_DATA=(SERVICE_NAME={})))",
            self.host, self.port, self.service
        ))
    }

    /// Fetches the password for an immediate connection attempt.
    ///
    pub fn password_from_env(&self) -> Result<String, AdapterError> {
        self.validate()?;
        if let Some(password) = &self.password {
            return Ok(password.0.clone());
        }
        env::var(&self.password_env).map_err(|_| {
            AdapterError::MissingPasswordEnvironmentVariable(self.password_env.clone())
        })
    }
}

fn validate_descriptor_component(
    label: &str,
    value: &str,
    allow_service_chars: bool,
) -> Result<(), AdapterError> {
    let valid = !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b'.' | b'-' | b'_')
                || (allow_service_chars && matches!(byte, b'$' | b'#'))
                || (!allow_service_chars && matches!(byte, b':' | b'[' | b']'))
        });
    if !valid {
        return Err(AdapterError::InvalidProfile(format!(
            "{label} contains unsupported Oracle Net descriptor characters"
        )));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OracleCapabilities {
    pub transactions: bool,
    pub transactional_ddl: bool,
    pub atomic_ddl_replacement: bool,
    pub streaming_queries: bool,
    pub tcps: bool,
    pub connect_timeout: bool,
    pub native_bulk_writes: bool,
}

pub const CAPABILITIES: OracleCapabilities = OracleCapabilities {
    transactions: true,
    transactional_ddl: false,
    atomic_ddl_replacement: false,
    streaming_queries: true,
    tcps: true,
    connect_timeout: true,
    native_bulk_writes: false,
};

pub const fn capabilities() -> OracleCapabilities {
    CAPABILITIES
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TableIdentifier {
    pub schema: Option<String>,
    pub name: String,
}

impl TableIdentifier {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            schema: None,
            name: name.into(),
        }
    }

    pub fn in_schema(schema: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            schema: Some(schema.into()),
            name: name.into(),
        }
    }

    fn validate(&self) -> Result<(), AdapterError> {
        validate_identifier(&self.name)?;
        if let Some(schema) = &self.schema {
            validate_identifier(schema)?;
        }
        Ok(())
    }

    fn quoted(&self) -> Result<String, AdapterError> {
        self.validate()?;
        match &self.schema {
            Some(schema) => Ok(format!(
                "{}.{}",
                quote_identifier(schema)?,
                quote_identifier(&self.name)?
            )),
            None => quote_identifier(&self.name),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ColumnInfo {
    pub name: String,
    pub data_type: String,
    pub nullable: bool,
    pub data_length: Option<i32>,
    pub precision: Option<i32>,
    pub scale: Option<i32>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OracleErrorKind {
    Authentication,
    Connection,
    TransientConnection,
    ObjectNotFound,
    ObjectAlreadyExists,
    ConstraintViolation,
    InvalidSql,
    Other,
}

#[derive(Debug, Eq, PartialEq)]
pub enum AdapterError {
    InvalidProfile(String),
    InvalidInput(String),
    InvalidIdentifier(String),
    MissingPasswordEnvironmentVariable(String),
    EmptySql,
    Oracle {
        kind: OracleErrorKind,
        message: String,
    },
    UnexpectedNoRows,
    ClientConfigurationUnavailable,
}

impl AdapterError {
    fn from_oracle(error: oracle::Error) -> Self {
        Self::from_oracle_message(error.to_string())
    }

    fn from_oracle_redacting(error: oracle::Error, descriptor: &str) -> Self {
        let message = error
            .to_string()
            .replace(descriptor, "[redacted Oracle Net descriptor]");
        Self::from_oracle_message(message)
    }

    fn from_oracle_message(message: String) -> Self {
        Self::Oracle {
            kind: map_oracle_error_message(&message),
            message,
        }
    }

    pub fn is_transient(&self) -> bool {
        matches!(
            self,
            Self::Oracle {
                kind: OracleErrorKind::TransientConnection,
                ..
            }
        )
    }
}

impl fmt::Display for AdapterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidProfile(message) => write!(f, "invalid Oracle profile: {message}"),
            Self::InvalidInput(message) => write!(f, "invalid Oracle input: {message}"),
            Self::InvalidIdentifier(message) => write!(f, "invalid Oracle identifier: {message}"),
            Self::MissingPasswordEnvironmentVariable(name) => {
                write!(f, "password environment variable {name:?} is not set")
            }
            Self::EmptySql => write!(f, "SQL must not be empty"),
            Self::Oracle { kind, message } => write!(f, "Oracle {kind:?} error: {message}"),
            Self::UnexpectedNoRows => write!(f, "Oracle connection test returned no rows"),
            Self::ClientConfigurationUnavailable => write!(
                f,
                "Oracle client configuration cannot guarantee the requested encryption mode"
            ),
        }
    }
}

impl StdError for AdapterError {}

/// Classifies an Oracle error message by its standard ORA error code.
pub fn map_oracle_error_message(message: &str) -> OracleErrorKind {
    let code = message
        .match_indices("ORA-")
        .map(|(index, _)| message.get(index..index + 9).unwrap_or_default())
        .find(|candidate| {
            candidate.len() == 9 && candidate[4..].bytes().all(|byte| byte.is_ascii_digit())
        });

    match code {
        Some("ORA-01017") => OracleErrorKind::Authentication,
        Some("ORA-12154" | "ORA-12514" | "ORA-12545") => OracleErrorKind::Connection,
        Some(
            "ORA-12170" | "ORA-12535" | "ORA-12541" | "ORA-12537" | "ORA-03113" | "ORA-03114"
            | "ORA-03135" | "ORA-01012",
        ) => OracleErrorKind::TransientConnection,
        Some("ORA-00942" | "ORA-04043") => OracleErrorKind::ObjectNotFound,
        Some("ORA-00955") => OracleErrorKind::ObjectAlreadyExists,
        Some("ORA-00001" | "ORA-02291" | "ORA-02292") => OracleErrorKind::ConstraintViolation,
        Some("ORA-00900" | "ORA-00901" | "ORA-00902" | "ORA-00903" | "ORA-00904") => {
            OracleErrorKind::InvalidSql
        }
        _ => OracleErrorKind::Other,
    }
}

/// Quotes one Oracle identifier, escaping embedded double quotes.
pub fn quote_identifier(identifier: &str) -> Result<String, AdapterError> {
    validate_identifier(identifier)?;
    Ok(format!("\"{}\"", identifier.replace('"', "\"\"")))
}

fn validate_identifier(identifier: &str) -> Result<(), AdapterError> {
    if identifier.is_empty() {
        return Err(AdapterError::InvalidIdentifier(
            "identifier must not be empty".to_owned(),
        ));
    }
    if identifier.contains('\0') {
        return Err(AdapterError::InvalidIdentifier(
            "identifier must not contain NUL".to_owned(),
        ));
    }
    Ok(())
}

pub struct OracleAdapter {
    connection: Connection,
}

impl OracleAdapter {
    /// Opens a connection, reading the password only from the configured env var.
    pub fn connect(profile: &ConnectionProfile) -> Result<Self, AdapterError> {
        let connect_descriptor = profile.oracle_connect_descriptor()?;
        let password = profile.password_from_env()?;
        let mut configuration = CLIENT_CONFIGURATION
            .lock()
            .map_err(|_| AdapterError::ClientConfigurationUnavailable)?;
        match (&*configuration, profile.tls_mode) {
            (ClientConfiguration::Native(directory), TlsMode::NativeRequired) => {
                verify_native_configuration(directory)?;
            }
            (ClientConfiguration::Uninitialized, TlsMode::NativeRequired) => {
                if oracle::InitParams::is_initialized() {
                    return Err(AdapterError::ClientConfigurationUnavailable);
                }
                let directory = env::var_os("TNS_ADMIN")
                    .map(std::path::PathBuf::from)
                    .ok_or(AdapterError::ClientConfigurationUnavailable)?;
                verify_native_configuration(&directory)?;
                let initialized = oracle::InitParams::new()
                    .oracle_client_config_dir(directory.as_os_str())
                    .map_err(|_| AdapterError::ClientConfigurationUnavailable)?
                    .init()
                    .map_err(|_| AdapterError::ClientConfigurationUnavailable)?;
                if !initialized {
                    return Err(AdapterError::ClientConfigurationUnavailable);
                }
                *configuration = ClientConfiguration::Native(directory);
            }
            (ClientConfiguration::Native(_), _)
            | (ClientConfiguration::Other, TlsMode::NativeRequired) => {
                return Err(AdapterError::ClientConfigurationUnavailable);
            }
            (ClientConfiguration::Uninitialized, _) => {
                *configuration = ClientConfiguration::Other;
            }
            (ClientConfiguration::Other, _) => {}
        }
        let connection = Connection::connect(&profile.user, &password, &connect_descriptor)
            .map_err(|error| AdapterError::from_oracle_redacting(error, &connect_descriptor))?;
        if let Some(seconds) = profile.call_timeout_secs {
            connection
                .set_call_timeout(Some(Duration::from_secs(u64::from(seconds))))
                .map_err(AdapterError::from_oracle)?;
        }
        Ok(Self { connection })
    }

    pub fn test_connection(&self) -> Result<(), AdapterError> {
        let mut rows = self
            .connection
            .query("SELECT 1 FROM DUAL", &[])
            .map_err(AdapterError::from_oracle)?;
        match rows.next() {
            Some(Ok(_)) => Ok(()),
            Some(Err(error)) => Err(AdapterError::from_oracle(error)),
            None => Err(AdapterError::UnexpectedNoRows),
        }
    }

    /// Requests interruption of a database call from another thread.
    pub fn break_execution(&self) -> Result<(), AdapterError> {
        self.connection
            .break_execution()
            .map_err(AdapterError::from_oracle)
    }

    /// Starts an Oracle read-write transaction explicitly.
    ///
    /// Oracle also begins transactions implicitly on the first DML statement.
    /// Nested transactions are not supported.
    pub fn begin_transaction(&self) -> Result<(), AdapterError> {
        self.connection
            .execute("SET TRANSACTION READ WRITE", &[])
            .map(|_| ())
            .map_err(AdapterError::from_oracle)
    }

    pub fn commit(&self) -> Result<(), AdapterError> {
        self.connection.commit().map_err(AdapterError::from_oracle)
    }

    pub fn rollback(&self) -> Result<(), AdapterError> {
        self.connection
            .rollback()
            .map_err(AdapterError::from_oracle)
    }

    /// Executes a query and returns Oracle's lazy row iterator.
    ///
    /// Rows are fetched as the caller iterates; use `Row::get` with the
    /// appropriate Rust type to retain Oracle's native values and nullability.
    pub fn query<'conn>(
        &'conn self,
        sql: &str,
        parameters: &[&dyn ToSql],
    ) -> Result<impl Iterator<Item = Result<oracle::Row, oracle::Error>> + 'conn, AdapterError>
    {
        if sql.trim().is_empty() {
            return Err(AdapterError::EmptySql);
        }
        self.connection
            .query(sql, parameters)
            .map_err(AdapterError::from_oracle)
    }

    /// Streams query results one row at a time without collecting them.
    pub fn query_stream<F>(
        &self,
        sql: &str,
        parameters: &[&dyn ToSql],
        mut consume: F,
    ) -> Result<(), AdapterError>
    where
        F: FnMut(oracle::Row) -> Result<(), AdapterError>,
    {
        for row in self.query(sql, parameters)? {
            consume(row.map_err(AdapterError::from_oracle)?)?;
        }
        Ok(())
    }

    /// Fetches query results into JSON objects.
    ///
    /// To keep the conversion portable, every non-NULL database value is
    /// represented as a JSON string. Values Oracle cannot convert to `String`
    /// (for example some binary types) return an error. Use `query` or
    /// `query_stream` with typed `Row::get` calls when numeric, temporal, or
    /// binary JSON representations are required.
    pub fn query_json(
        &self,
        sql: &str,
        parameters: &[&dyn ToSql],
    ) -> Result<Vec<serde_json::Value>, AdapterError> {
        self.query(sql, parameters)?
            .map(|row| row.map_err(AdapterError::from_oracle).and_then(row_as_json))
            .collect()
    }

    /// Fetches JSON rows using string bind parameters.
    pub fn query_json_strings(
        &self,
        sql: &str,
        parameters: &[String],
    ) -> Result<Vec<serde_json::Value>, AdapterError> {
        let parameters: Vec<&dyn ToSql> =
            parameters.iter().map(|value| value as &dyn ToSql).collect();
        self.query_json(sql, &parameters)
    }

    /// Streams query results as JSON objects, representing non-NULL values as strings.
    /// A conversion failure for an Oracle type is returned as an error.
    pub fn query_json_stream<F>(
        &self,
        sql: &str,
        parameters: &[&dyn ToSql],
        mut consume: F,
    ) -> Result<(), AdapterError>
    where
        F: FnMut(serde_json::Value) -> Result<(), AdapterError>,
    {
        for row in self.query(sql, parameters)? {
            consume(row_as_json(row.map_err(AdapterError::from_oracle)?)?)?;
        }
        Ok(())
    }

    /// Executes SQL with Oracle bind parameters and returns the affected-row count.
    pub fn execute(&self, sql: &str, parameters: &[&dyn ToSql]) -> Result<u64, AdapterError> {
        if sql.trim().is_empty() {
            return Err(AdapterError::EmptySql);
        }
        let statement = self
            .connection
            .execute(sql, parameters)
            .map_err(AdapterError::from_oracle)?;
        statement.row_count().map_err(AdapterError::from_oracle)
    }

    /// Returns columns in ordinal order. Identifier spelling is matched exactly.
    pub fn table_columns(&self, table: &TableIdentifier) -> Result<Vec<ColumnInfo>, AdapterError> {
        table.validate()?;
        let (sql, params): (&str, Vec<&dyn ToSql>) = match &table.schema {
            Some(schema) => (
                "SELECT COLUMN_NAME, DATA_TYPE, NULLABLE, DATA_LENGTH, DATA_PRECISION, DATA_SCALE \
                 FROM ALL_TAB_COLUMNS WHERE OWNER = :1 AND TABLE_NAME = :2 ORDER BY COLUMN_ID",
                vec![schema as &dyn ToSql, &table.name],
            ),
            None => (
                "SELECT COLUMN_NAME, DATA_TYPE, NULLABLE, DATA_LENGTH, DATA_PRECISION, DATA_SCALE \
                 FROM USER_TAB_COLUMNS WHERE TABLE_NAME = :1 ORDER BY COLUMN_ID",
                vec![&table.name],
            ),
        };
        let rows = self
            .connection
            .query(sql, &params)
            .map_err(AdapterError::from_oracle)?;
        let mut columns = Vec::new();
        for row in rows {
            let row = row.map_err(AdapterError::from_oracle)?;
            let nullable: String = row.get("NULLABLE").map_err(AdapterError::from_oracle)?;
            columns.push(ColumnInfo {
                name: row.get("COLUMN_NAME").map_err(AdapterError::from_oracle)?,
                data_type: row.get("DATA_TYPE").map_err(AdapterError::from_oracle)?,
                nullable: nullable == "Y",
                data_length: row.get("DATA_LENGTH").map_err(AdapterError::from_oracle)?,
                precision: row
                    .get("DATA_PRECISION")
                    .map_err(AdapterError::from_oracle)?,
                scale: row.get("DATA_SCALE").map_err(AdapterError::from_oracle)?,
            });
        }
        Ok(columns)
    }

    /// Creates a table from a SELECT statement.
    ///
    /// Oracle DDL commits implicitly. This method does not offer transactional
    /// rollback or atomic replacement; it only issues `CREATE TABLE ... AS`.
    pub fn materialize_table(
        &self,
        table: &TableIdentifier,
        select_sql: &str,
    ) -> Result<(), AdapterError> {
        self.materialize("TABLE", table, select_sql)
    }

    /// Creates a view from a SELECT statement.
    ///
    /// Oracle DDL commits implicitly. This method does not offer transactional
    /// rollback or atomic replacement; an existing view is not replaced.
    pub fn materialize_view(
        &self,
        view: &TableIdentifier,
        select_sql: &str,
    ) -> Result<(), AdapterError> {
        self.materialize("VIEW", view, select_sql)
    }

    /// Inserts JSON-compatible values into an existing table.
    ///
    /// Each row must have the same set of keys. Values bind as SQL NULL,
    /// strings, numbers, or 0/1 for booleans; arrays and objects bind as their
    /// JSON text. Database-specific conversions (for example JSON columns or
    /// date strings) remain subject to Oracle's target column types.
    pub fn write_rows(
        &self,
        table: &TableIdentifier,
        rows: &[serde_json::Value],
    ) -> Result<u64, AdapterError> {
        self.insert_rows(table, rows)
    }

    /// Appends JSON-compatible rows to an existing table.
    pub fn append_rows(
        &self,
        table: &TableIdentifier,
        rows: &[serde_json::Value],
    ) -> Result<u64, AdapterError> {
        self.insert_rows(table, rows)
    }

    /// Deletes all existing rows and inserts the supplied rows.
    ///
    /// This performs DML only and leaves commit/rollback to the caller. It can
    /// be rolled back while the transaction remains open, but is not committed
    /// atomically by this method.
    pub fn replace_rows(
        &self,
        table: &TableIdentifier,
        rows: &[serde_json::Value],
    ) -> Result<u64, AdapterError> {
        row_columns(rows)?;
        let target = table.quoted()?;
        self.execute(&format!("DELETE FROM {target}"), &[])?;
        self.insert_rows(table, rows)
    }

    /// Inserts or updates each JSON-compatible row using the supplied key fields.
    ///
    /// Rows must have identical keys and include each match key. The operation
    /// leaves transaction completion to the caller.
    pub fn upsert_rows(
        &self,
        table: &TableIdentifier,
        rows: &[serde_json::Value],
        match_keys: &[&str],
    ) -> Result<u64, AdapterError> {
        let target = table.quoted()?;
        let columns = row_columns(rows)?;
        validate_match_keys(&columns, match_keys)?;
        if rows.is_empty() {
            return Ok(0);
        }

        let quoted_columns = columns
            .iter()
            .map(|column| quote_identifier(column))
            .collect::<Result<Vec<_>, _>>()?;
        let quoted_keys = match_keys
            .iter()
            .map(|key| quote_identifier(key))
            .collect::<Result<Vec<_>, _>>()?;
        let source = columns
            .iter()
            .enumerate()
            .map(|(index, column)| Ok(format!(":{} AS {}", index + 1, quote_identifier(column)?)))
            .collect::<Result<Vec<_>, AdapterError>>()?
            .join(", ");
        let on_clause = quoted_keys
            .iter()
            .map(|key| format!("t.{key} = s.{key}"))
            .collect::<Vec<_>>()
            .join(" AND ");
        let key_set = match_keys
            .iter()
            .copied()
            .collect::<std::collections::HashSet<_>>();
        let updates = columns
            .iter()
            .zip(quoted_columns.iter())
            .filter(|(column, _)| !key_set.contains(column.as_str()))
            .map(|(_, quoted)| format!("t.{quoted} = s.{quoted}"))
            .collect::<Vec<_>>();
        let insert_values = quoted_columns
            .iter()
            .map(|column| format!("s.{column}"))
            .collect::<Vec<_>>()
            .join(", ");
        let mut sql =
            format!("MERGE INTO {target} t USING (SELECT {source} FROM DUAL) s ON ({on_clause})");
        if !updates.is_empty() {
            sql.push_str(&format!(
                " WHEN MATCHED THEN UPDATE SET {}",
                updates.join(", ")
            ));
        }
        sql.push_str(&format!(
            " WHEN NOT MATCHED THEN INSERT ({}) VALUES ({insert_values})",
            quoted_columns.join(", ")
        ));

        let mut affected = 0;
        for row in rows {
            let values = row_values(row, &columns)?;
            let binds = values.iter().map(JsonBind::as_sql).collect::<Vec<_>>();
            affected += self.execute(&sql, &binds)?;
        }
        Ok(affected)
    }

    /// Inserts rows into an existing staging table; it does not create one.
    ///
    /// Create staging tables separately with `materialize_table`. Oracle DDL
    /// implicitly commits, so staging-table creation is not transactional.
    pub fn stage_rows(
        &self,
        staging_table: &TableIdentifier,
        rows: &[serde_json::Value],
    ) -> Result<u64, AdapterError> {
        self.insert_rows(staging_table, rows)
    }

    fn insert_rows(
        &self,
        table: &TableIdentifier,
        rows: &[serde_json::Value],
    ) -> Result<u64, AdapterError> {
        let target = table.quoted()?;
        let columns = row_columns(rows)?;
        if columns.is_empty() {
            return Ok(0);
        }
        let quoted_columns = columns
            .iter()
            .map(|column| quote_identifier(column))
            .collect::<Result<Vec<_>, _>>()?;
        let placeholders = (1..=columns.len())
            .map(|index| format!(":{index}"))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "INSERT INTO {target} ({}) VALUES ({placeholders})",
            quoted_columns.join(", ")
        );
        let mut affected = 0;
        for row in rows {
            let values = row_values(row, &columns)?;
            let binds = values.iter().map(JsonBind::as_sql).collect::<Vec<_>>();
            affected += self.execute(&sql, &binds)?;
        }
        Ok(affected)
    }

    fn materialize(
        &self,
        object_type: &str,
        object: &TableIdentifier,
        select_sql: &str,
    ) -> Result<(), AdapterError> {
        if select_sql.trim().is_empty() {
            return Err(AdapterError::EmptySql);
        }
        let target = object.quoted()?;
        let sql = format!("CREATE {object_type} {target} AS {select_sql}");
        self.execute(&sql, &[])?;
        Ok(())
    }
}

fn verify_native_configuration(directory: &std::path::Path) -> Result<(), AdapterError> {
    let configuration = std::fs::read_to_string(directory.join("sqlnet.ora"))
        .map_err(|_| AdapterError::ClientConfigurationUnavailable)?;
    if configuration.replace("\r\n", "\n") != NATIVE_ENCRYPTION_CONFIG {
        return Err(AdapterError::ClientConfigurationUnavailable);
    }
    Ok(())
}

#[derive(Debug)]
enum JsonBind {
    Null(Option<String>),
    Text(String),
    Integer(i64),
    Float(f64),
}

impl JsonBind {
    fn from_json(value: &serde_json::Value) -> Result<Self, AdapterError> {
        match value {
            serde_json::Value::Null => Ok(Self::Null(None)),
            serde_json::Value::Bool(value) => Ok(Self::Integer(i64::from(*value))),
            serde_json::Value::Number(value) => {
                if let Some(integer) = value.as_i64() {
                    Ok(Self::Integer(integer))
                } else if let Some(float) = value.as_f64() {
                    Ok(Self::Float(float))
                } else {
                    Err(AdapterError::InvalidInput(
                        "JSON number cannot be represented by Oracle bind types".to_owned(),
                    ))
                }
            }
            serde_json::Value::String(value) => Ok(Self::Text(value.clone())),
            serde_json::Value::Array(_) | serde_json::Value::Object(_) => {
                serde_json::to_string(value)
                    .map(Self::Text)
                    .map_err(|error| AdapterError::InvalidInput(error.to_string()))
            }
        }
    }

    fn as_sql(&self) -> &dyn ToSql {
        match self {
            Self::Null(value) => value,
            Self::Text(value) => value,
            Self::Integer(value) => value,
            Self::Float(value) => value,
        }
    }
}

fn row_columns(rows: &[serde_json::Value]) -> Result<Vec<String>, AdapterError> {
    let Some(first) = rows.first() else {
        return Ok(Vec::new());
    };
    let first = first
        .as_object()
        .ok_or_else(|| AdapterError::InvalidInput("each row must be a JSON object".to_owned()))?;
    let columns = first.keys().cloned().collect::<Vec<_>>();
    for row in &rows[1..] {
        let object = row.as_object().ok_or_else(|| {
            AdapterError::InvalidInput("each row must be a JSON object".to_owned())
        })?;
        if object.len() != columns.len()
            || columns.iter().any(|column| !object.contains_key(column))
        {
            return Err(AdapterError::InvalidInput(
                "all rows must have the same set of keys".to_owned(),
            ));
        }
    }
    for column in &columns {
        validate_identifier(column)?;
    }
    Ok(columns)
}

fn row_as_json(row: oracle::Row) -> Result<serde_json::Value, AdapterError> {
    let mut object = serde_json::Map::new();
    for (index, column) in row.column_info().iter().enumerate() {
        let value: Option<String> = row.get(index).map_err(AdapterError::from_oracle)?;
        object.insert(
            column.name().to_owned(),
            value.map_or(serde_json::Value::Null, serde_json::Value::String),
        );
    }
    Ok(serde_json::Value::Object(object))
}

fn row_values(row: &serde_json::Value, columns: &[String]) -> Result<Vec<JsonBind>, AdapterError> {
    let object = row
        .as_object()
        .ok_or_else(|| AdapterError::InvalidInput("each row must be a JSON object".to_owned()))?;
    columns
        .iter()
        .map(|column| {
            object
                .get(column)
                .ok_or_else(|| {
                    AdapterError::InvalidInput("all rows must have the same set of keys".to_owned())
                })
                .and_then(JsonBind::from_json)
        })
        .collect()
}

fn validate_match_keys(columns: &[String], match_keys: &[&str]) -> Result<(), AdapterError> {
    if match_keys.is_empty() {
        return Err(AdapterError::InvalidInput(
            "upsert requires at least one match key".to_owned(),
        ));
    }
    for key in match_keys {
        validate_identifier(key)?;
        if !columns.iter().any(|column| column == key) {
            return Err(AdapterError::InvalidInput(format!(
                "upsert match key {key:?} is missing from row values"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn literal_password_is_used_and_redacted() {
        let profile = ConnectionProfile::new("user", "localhost", 1521, "service", "")
            .with_password("test-only-value".to_owned());
        assert_eq!(profile.password_from_env().unwrap(), "test-only-value");
        assert!(!format!("{profile:?}").contains("test-only-value"));
    }

    #[test]
    fn profile_validation_requires_each_connection_setting() {
        for profile in [
            ConnectionProfile::new("", "localhost", 1521, "service", "ORACLE_PASSWORD"),
            ConnectionProfile::new("user", "", 1521, "service", "ORACLE_PASSWORD"),
            ConnectionProfile::new("user", "localhost", 1521, "", "ORACLE_PASSWORD"),
            ConnectionProfile::new("user", "localhost", 0, "service", "ORACLE_PASSWORD"),
            ConnectionProfile::new("user", "localhost", 1521, "service", ""),
        ] {
            assert!(matches!(
                profile.validate(),
                Err(AdapterError::InvalidProfile(_))
            ));
        }
        assert!(
            ConnectionProfile::new("user", "localhost", 1521, "service", "ORACLE_PASSWORD")
                .validate()
                .is_ok()
        );
    }

    #[test]
    fn profile_composes_safe_tcp_and_tcps_descriptors_with_timeout() {
        let plain =
            ConnectionProfile::new("user", "localhost", 1521, "app_service", "ORACLE_PASSWORD");
        assert_eq!(
            plain.oracle_connect_descriptor().unwrap(),
            "(DESCRIPTION=(ADDRESS=(PROTOCOL=TCP)(HOST=localhost)(PORT=1521))(CONNECT_DATA=(SERVICE_NAME=app_service)))"
        );

        let secure =
            ConnectionProfile::new("user", "db.example", 1521, "app_service", "ORACLE_PASSWORD")
                .with_tls_mode(TlsMode::Required)
                .with_connect_timeout_secs(8);
        assert_eq!(
            secure.oracle_connect_descriptor().unwrap(),
            "(DESCRIPTION=(ADDRESS=(PROTOCOL=TCPS)(HOST=db.example)(PORT=1521))(CONNECT_TIMEOUT=8)(CONNECT_DATA=(SERVICE_NAME=app_service)))"
        );
        assert!(
            ConnectionProfile::new("user", "db.example", 1521, "service", "ORACLE_PASSWORD")
                .validate()
                .is_err()
        );
        assert!(ConnectionProfile::new(
            "user",
            "db.example)(ADDRESS=(PROTOCOL=TCP",
            1521,
            "service",
            "ORACLE_PASSWORD"
        )
        .validate()
        .is_err());
        assert!(plain
            .clone()
            .with_connect_timeout_secs(0)
            .validate()
            .is_err());
        assert!(plain.clone().with_call_timeout_secs(0).validate().is_err());
        assert!(plain.with_call_timeout_secs(u32::MAX).validate().is_err());
    }

    #[test]
    fn native_encryption_requires_the_complete_client_configuration() {
        let directory = tempfile::tempdir().unwrap();
        assert!(verify_native_configuration(directory.path()).is_err());
        std::fs::write(
            directory.path().join("sqlnet.ora"),
            NATIVE_ENCRYPTION_CONFIG,
        )
        .unwrap();
        assert!(verify_native_configuration(directory.path()).is_ok());
        std::fs::write(
            directory.path().join("sqlnet.ora"),
            NATIVE_ENCRYPTION_CONFIG.replace(
                "ENCRYPTION_CLIENT = REQUIRED",
                "ENCRYPTION_CLIENT = ACCEPTED",
            ),
        )
        .unwrap();
        assert!(verify_native_configuration(directory.path()).is_err());
    }

    #[test]
    fn native_encryption_uses_tcp_with_explicit_encryption_policy() {
        let profile =
            ConnectionProfile::new("user", "db.example", 1521, "app_service", "ORACLE_PASSWORD")
                .with_tls_mode(TlsMode::NativeRequired);
        assert!(profile.validate().is_ok());
        assert!(profile
            .oracle_connect_descriptor()
            .unwrap()
            .contains("(PROTOCOL=TCP)"));
    }

    #[test]
    fn identifier_quoting_escapes_quotes_and_rejects_nul() {
        assert_eq!(quote_identifier("odd\"name").unwrap(), "\"odd\"\"name\"");
        let table = TableIdentifier::in_schema("Mixed Case", "a.b");
        assert_eq!(table.quoted().unwrap(), "\"Mixed Case\".\"a.b\"");
        assert!(matches!(
            quote_identifier("bad\0name"),
            Err(AdapterError::InvalidIdentifier(_))
        ));
        assert!(matches!(
            quote_identifier(""),
            Err(AdapterError::InvalidIdentifier(_))
        ));
    }

    #[test]
    fn password_lookup_reads_env_without_putting_secret_in_profile() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let env_name = format!(
            "DOWAI_ORACLE_TEST_PASSWORD_{}_{}",
            std::process::id(),
            suffix
        );
        let secret = "test-password-value";
        env::set_var(&env_name, secret);
        let profile = ConnectionProfile::new("user", "localhost", 1521, "service", &env_name);

        assert_eq!(profile.password_from_env().unwrap(), secret);
        assert!(!format!("{profile:?}").contains(secret));
        assert_eq!(profile.password_env, env_name);

        env::remove_var(env_name);
    }

    #[test]
    fn oracle_error_codes_map_to_useful_categories() {
        assert_eq!(
            map_oracle_error_message("ORA-01017: invalid username/password"),
            OracleErrorKind::Authentication
        );
        assert_eq!(
            map_oracle_error_message("ORA-00942: table or view does not exist"),
            OracleErrorKind::ObjectNotFound
        );
        assert_eq!(
            map_oracle_error_message("ORA-00955: name is already used"),
            OracleErrorKind::ObjectAlreadyExists
        );
        assert_eq!(
            map_oracle_error_message("unexpected failure"),
            OracleErrorKind::Other
        );
        assert_eq!(
            map_oracle_error_message("ORA-12170: connect timeout"),
            OracleErrorKind::TransientConnection
        );
        assert!(AdapterError::Oracle {
            kind: OracleErrorKind::TransientConnection,
            message: String::new(),
        }
        .is_transient());
        assert!(!AdapterError::Oracle {
            kind: OracleErrorKind::Connection,
            message: String::new(),
        }
        .is_transient());
    }

    #[test]
    fn oracle_ddl_is_declared_non_transactional() {
        assert!(!capabilities().transactional_ddl);
        assert!(!capabilities().atomic_ddl_replacement);
        assert!(capabilities().tcps);
        assert!(capabilities().connect_timeout);
    }

    #[test]
    fn json_rows_validate_uniform_columns_and_bind_values_safely() {
        let rows = vec![
            serde_json::json!({"id": 7, "name": "it's safe", "active": true, "extra": null}),
            serde_json::json!({"id": 8, "name": "second", "active": false, "extra": {"x": 1}}),
        ];
        let columns = row_columns(&rows).unwrap();
        assert_eq!(columns, vec!["active", "extra", "id", "name"]);
        let values = row_values(&rows[0], &columns).unwrap();
        assert!(matches!(values[0], JsonBind::Integer(1)));
        assert!(matches!(values[1], JsonBind::Null(None)));
        assert!(matches!(values[2], JsonBind::Integer(7)));
        assert!(matches!(values[3], JsonBind::Text(ref value) if value == "it's safe"));

        let inconsistent = vec![
            serde_json::json!({"id": 1}),
            serde_json::json!({"different": 2}),
        ];
        assert!(row_columns(&inconsistent).is_err());
        assert!(row_columns(&[serde_json::json!(["not", "an", "object"])]).is_err());
        assert!(validate_match_keys(&columns, &[]).is_err());
        assert!(validate_match_keys(&columns, &["missing"]).is_err());
        assert!(validate_match_keys(&columns, &["id"]).is_ok());
    }
}
