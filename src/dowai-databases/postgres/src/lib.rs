use std::net::IpAddr;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use native_tls::TlsConnector;
use postgres::config::SslMode;
use postgres::types::ToSql;
use postgres::{Client, Config, Row};
use postgres_native_tls::MakeTlsConnector;

mod adapter;
mod full_refresh;
mod stream;
mod write;

pub use adapter::{Capabilities, ColumnInfo, SqlAdapter, POSTGRES_CAPABILITIES};
pub use stream::{Batch, BatchStream, RowCount, StreamLimits, STREAM_BATCH_ROWS_MAX};
pub use write::{MergeCounters, WRITE_MAX_PARAMS};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TlsMode {
    #[default]
    Required,
    Disabled,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PgProfile {
    host: String,
    port: u16,
    user: String,
    database: String,
    password_env_var: String,
    password: Option<SecretPassword>,
    tls_mode: TlsMode,
    connect_timeout: Duration,
    query_timeout: Option<Duration>,
}

#[derive(Clone, PartialEq, Eq)]
struct SecretPassword(String);

impl std::fmt::Debug for SecretPassword {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("<redacted>")
    }
}

/// Bounds of `database.connect_timeout_seconds` (catalog: 1..=600 s).
const CONNECT_TIMEOUT_MAX: Duration = Duration::from_secs(600);
/// Bounds of `database.query_timeout_seconds` (catalog: 1..=86400 s).
const QUERY_TIMEOUT_MAX: Duration = Duration::from_secs(86_400);

impl PgProfile {
    pub fn new(
        host: impl Into<String>,
        port: u16,
        user: impl Into<String>,
        database: impl Into<String>,
        password_env_var: impl Into<String>,
    ) -> Self {
        Self {
            host: host.into(),
            port,
            user: user.into(),
            database: database.into(),
            password_env_var: password_env_var.into(),
            password: None,
            tls_mode: TlsMode::Required,
            connect_timeout: Duration::from_secs(10),
            query_timeout: None,
        }
    }

    pub fn with_password(mut self, password: String) -> Self {
        self.password = Some(SecretPassword(password));
        self
    }

    pub fn tls_mode(&self) -> TlsMode {
        self.tls_mode
    }

    pub fn set_tls_mode(&mut self, tls_mode: TlsMode) {
        self.tls_mode = tls_mode;
    }

    /// Connection timeout (`database.connect_timeout_seconds`). Default 10 s.
    pub fn connect_timeout(&self) -> Duration {
        self.connect_timeout
    }

    pub fn set_connect_timeout(&mut self, timeout: Duration) {
        self.connect_timeout = timeout;
    }

    /// Server-side `statement_timeout` applied to every statement of the session
    /// (`database.query_timeout_seconds`). `None` leaves the server default.
    pub fn query_timeout(&self) -> Option<Duration> {
        self.query_timeout
    }

    pub fn set_query_timeout(&mut self, timeout: Option<Duration>) {
        self.query_timeout = timeout;
    }

    pub fn validate(&self) -> Result<(), AdapterError> {
        for (field, value) in [
            ("host", self.host.as_str()),
            ("user", self.user.as_str()),
            ("database", self.database.as_str()),
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
        if self.password.is_none() && !valid_env_var_name(&self.password_env_var) {
            return Err(AdapterError::InvalidProfile(
                "password environment variable",
            ));
        }
        if self.tls_mode == TlsMode::Disabled && !is_local_host(&self.host) {
            return Err(AdapterError::InsecureTlsDisabled);
        }
        if self.connect_timeout.is_zero() || self.connect_timeout > CONNECT_TIMEOUT_MAX {
            return Err(AdapterError::InvalidProfile("connect timeout"));
        }
        if let Some(query_timeout) = self.query_timeout {
            if query_timeout.is_zero() || query_timeout > QUERY_TIMEOUT_MAX {
                return Err(AdapterError::InvalidProfile("query timeout"));
            }
        }
        Ok(())
    }

    fn resolve_password(&self) -> Result<String, AdapterError> {
        match &self.password {
            Some(password) => Ok(password.0.clone()),
            None => {
                std::env::var(&self.password_env_var).map_err(|_| AdapterError::MissingPassword)
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdapterError {
    InvalidProfile(&'static str),
    InvalidIdentifier,
    UnsupportedRelationType,
    MultipleStatements,
    DestructiveApprovalRequired,
    InsecureTlsDisabled,
    MissingPassword,
    TlsConfigurationFailed,
    ConnectionFailed,
    QueryFailed,
    /// The caller cancelled the operation; the transaction was rolled back.
    Cancelled,
    /// Transient failure (ADR 0005 R2): lost connection, lock or statement timeout, deadlock or
    /// serialization failure. The transaction was rolled back, so nothing was applied.
    TransientFailure,
    /// `begin_transaction` with a transaction already open, or commit/rollback without one.
    InvalidTransactionState,
    /// Stream limits out of range: `batch_rows` in 1..=1000000 and `max_inflight_bytes` > 0.
    InvalidStreamLimit,
    /// A batch exceeded `max_inflight_bytes`. The stream fails closed; no rows are truncated.
    InflightLimitExceeded,
    /// Write specification rejected before any SQL runs (empty or mismatched columns/keys).
    InvalidWriteSpec(&'static str),
    /// The staging table holds more than one row for the same merge key in one merge.
    DuplicateMergeKey,
    /// Full refresh: the rebuilt relation's columns (name or type, in order) differ from the
    /// existing destination. Nothing is swapped; the destination is kept.
    SchemaMismatch,
}

impl AdapterError {
    /// `true` only for failures the runtime may retry (ENT-037).
    pub fn is_transient(self) -> bool {
        matches!(self, Self::TransientFailure)
    }
}

impl std::fmt::Display for AdapterError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidProfile(field) => write!(formatter, "invalid PostgreSQL profile: {field}"),
            Self::InvalidIdentifier => {
                formatter.write_str("invalid PostgreSQL relation identifier")
            }
            Self::UnsupportedRelationType => {
                formatter.write_str("cannot atomically replace this PostgreSQL relation type")
            }
            Self::MultipleStatements => {
                formatter.write_str("SQL model must contain exactly one statement")
            }
            Self::DestructiveApprovalRequired => formatter
                .write_str("replacing an existing PostgreSQL relation requires explicit approval"),
            Self::InsecureTlsDisabled => {
                formatter.write_str("TLS may only be disabled for a local PostgreSQL host")
            }
            Self::MissingPassword => {
                formatter.write_str("required password environment variable is missing")
            }
            Self::TlsConfigurationFailed => {
                formatter.write_str("PostgreSQL TLS configuration failed")
            }
            Self::ConnectionFailed => formatter.write_str("PostgreSQL connection failed"),
            Self::QueryFailed => formatter.write_str("PostgreSQL query failed"),
            Self::TransientFailure => {
                formatter.write_str("PostgreSQL transient failure; the operation may be retried")
            }
            Self::Cancelled => formatter.write_str("PostgreSQL operation cancelled"),
            Self::InvalidTransactionState => {
                formatter.write_str("invalid PostgreSQL transaction state")
            }
            Self::InvalidStreamLimit => formatter.write_str("invalid PostgreSQL stream limits"),
            Self::SchemaMismatch => formatter.write_str(
                "full refresh rejected: the rebuilt relation's columns differ from the destination",
            ),
            Self::InflightLimitExceeded => {
                formatter.write_str("PostgreSQL batch exceeds the in-flight byte limit")
            }
            Self::InvalidWriteSpec(reason) => {
                write!(
                    formatter,
                    "invalid PostgreSQL write specification: {reason}"
                )
            }
            Self::DuplicateMergeKey => {
                formatter.write_str("PostgreSQL merge staging has duplicate keys")
            }
        }
    }
}

impl std::error::Error for AdapterError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Materialization {
    Table,
    View,
}

pub fn quote_identifier(identifier: &str) -> Result<String, AdapterError> {
    if identifier.is_empty() || identifier.len() > 63 || identifier.chars().any(char::is_control) {
        return Err(AdapterError::InvalidIdentifier);
    }
    Ok(format!("\"{}\"", identifier.replace('"', "\"\"")))
}

pub struct PostgresAdapter {
    client: Client,
    tls: MakeTlsConnector,
    /// Set by `SqlAdapter::begin_transaction` and cleared by commit or rollback.
    in_transaction: bool,
}

impl PostgresAdapter {
    /// `true` once the connection is gone and the adapter must reconnect before reuse.
    pub fn is_closed(&self) -> bool {
        self.client.is_closed()
    }

    pub fn connect(profile: &PgProfile) -> Result<Self, AdapterError> {
        profile.validate()?;
        let password = profile.resolve_password()?;

        let mut config = Config::new();
        config
            .host(&profile.host)
            .port(profile.port)
            .user(&profile.user)
            .password(password)
            .dbname(&profile.database)
            .options(&session_options(profile))
            .connect_timeout(profile.connect_timeout)
            .ssl_mode(match profile.tls_mode {
                TlsMode::Required => SslMode::Require,
                TlsMode::Disabled => SslMode::Disable,
            });

        let connector = TlsConnector::new().map_err(|_| AdapterError::TlsConfigurationFailed)?;
        let tls = MakeTlsConnector::new(connector);
        let client = config
            .connect(tls.clone())
            .map_err(|_| AdapterError::ConnectionFailed)?;
        Ok(Self {
            client,
            tls,
            in_transaction: false,
        })
    }

    pub fn execute(
        &mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<u64, AdapterError> {
        self.client
            .execute(sql, params)
            .map_err(|_| AdapterError::QueryFailed)
    }

    pub fn query(
        &mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<Vec<Row>, AdapterError> {
        self.client
            .query(sql, params)
            .map_err(|_| AdapterError::QueryFailed)
    }

    /// Column names and type names of a relation. Only prepares the statement: no row is read.
    /// `relation` must be built from identifiers quoted by [`quote_identifier`].
    /// Waits are bounded (`lock_timeout` 5 s, `statement_timeout` 10 s) so a locked relation
    /// cannot stall the host indefinitely. Use this only on a dedicated, read-only connection.
    pub fn describe_relation(
        &mut self,
        relation: &str,
    ) -> Result<Vec<(String, String)>, AdapterError> {
        self.client
            .batch_execute("SET lock_timeout = '5s'; SET statement_timeout = '10s'")
            .map_err(|_| AdapterError::QueryFailed)?;
        let statement = self
            .client
            .prepare(&format!("SELECT * FROM {relation} LIMIT 0"))
            .map_err(|_| AdapterError::QueryFailed)?;
        Ok(statement
            .columns()
            .iter()
            .map(|column| (column.name().to_owned(), column.type_().name().to_owned()))
            .collect())
    }

    /// Reads one batch of rows of a relation, in physical order (`ORDER BY ctid`).
    /// `columns` are (name, type) pairs from [`PostgresAdapter::describe_relation`]; types
    /// outside the direct set are read as text (exact, e.g. `numeric`, dates, UUID).
    /// `relation` must be built from identifiers quoted by [`quote_identifier`]. The batch is
    /// one autocommit statement: it holds no transaction open between batches.
    pub fn read_rows(
        &mut self,
        relation: &str,
        columns: &[(String, String)],
        offset: u64,
        limit: u32,
    ) -> Result<Vec<Vec<DatasetCell>>, AdapterError> {
        self.read_rows_filtered(relation, columns, &[], offset, limit)
    }

    /// Like [`PostgresAdapter::read_rows`], with `filters` pushed down to the `WHERE` clause
    /// (ENT-041). Filters are combined with `AND`. Returned rows are the ones that satisfy the
    /// predicates, so the host counts only what the filter let through.
    pub fn read_rows_filtered(
        &mut self,
        relation: &str,
        columns: &[(String, String)],
        filters: &[SqlFilter],
        offset: u64,
        limit: u32,
    ) -> Result<Vec<Vec<DatasetCell>>, AdapterError> {
        let statement = rows_statement(relation, columns, filters, offset, limit)?;
        let params: Vec<&(dyn ToSql + Sync)> = statement
            .params
            .iter()
            .map(|param| param.as_ref())
            .collect();
        let rows = self
            .client
            .query(&statement.sql, &params)
            .map_err(|_| AdapterError::QueryFailed)?;
        rows.iter()
            .map(|row| {
                columns
                    .iter()
                    .enumerate()
                    .map(|(index, (_, data_type))| decode_cell(row, index, data_type))
                    .collect::<Result<Vec<_>, _>>()
            })
            .collect()
    }

    pub fn test_connection(&mut self) -> Result<(), AdapterError> {
        self.client
            .query_one("SELECT 1", &[])
            .map(|_| ())
            .map_err(|_| AdapterError::QueryFailed)
    }

    pub fn materialize(
        &mut self,
        relation: &str,
        materialization: Materialization,
        query: &str,
        allow_replace: bool,
    ) -> Result<(), AdapterError> {
        ensure_single_statement(query)?;
        if query.trim().is_empty() {
            return Err(AdapterError::QueryFailed);
        }
        let quoted = quote_identifier(relation)?;
        let mut transaction = self.client.transaction().map_err(classify_failure)?;
        let existing = transaction
            .query_opt(
                "SELECT c.relkind::text FROM pg_class c WHERE c.oid = to_regclass($1)",
                &[&quoted],
            )
            .map_err(classify_failure)?
            .map(|row| row.get::<_, String>(0));
        ensure_destructive_approval(existing.is_some(), allow_replace)?;
        if let Some(kind) = existing {
            let drop_kind = match kind.as_str() {
                "r" => "TABLE",
                "v" => "VIEW",
                _ => return Err(AdapterError::UnsupportedRelationType),
            };
            transaction
                .execute(&format!("DROP {drop_kind} {quoted}"), &[])
                .map_err(classify_failure)?;
        }
        let create = match materialization {
            Materialization::Table => "TABLE",
            Materialization::View => "VIEW",
        };
        transaction
            .batch_execute(&format!("CREATE {create} {quoted} AS {query}"))
            .map_err(classify_failure)?;
        transaction.commit().map_err(classify_failure)
    }

    /// Like [`Self::materialize`], but a watcher sends a PostgreSQL cancel request as soon
    /// as `cancelled()` turns true. The statement then fails server-side and the dropped
    /// transaction rolls back, so a replacement is never partially applied.
    pub fn materialize_cancellable(
        &mut self,
        relation: &str,
        materialization: Materialization,
        query: &str,
        allow_replace: bool,
        cancelled: &(dyn Fn() -> bool + Sync),
    ) -> Result<(), AdapterError> {
        self.with_cancel_watch(cancelled, |adapter| {
            adapter.materialize(relation, materialization, query, allow_replace)
        })
    }

    /// Runs `operation` while a watcher sends PostgreSQL cancel requests as soon as
    /// `cancelled()` turns true. A failure observed after cancellation is reported as
    /// [`AdapterError::Cancelled`].
    pub(crate) fn with_cancel_watch(
        &mut self,
        cancelled: &(dyn Fn() -> bool + Sync),
        operation: impl FnOnce(&mut Self) -> Result<(), AdapterError>,
    ) -> Result<(), AdapterError> {
        if cancelled() {
            return Err(AdapterError::Cancelled);
        }
        let token = self.client.cancel_token();
        let tls = self.tls.clone();
        let done = AtomicBool::new(false);
        let result = std::thread::scope(|scope| {
            scope.spawn(|| {
                let mut last_request: Option<Instant> = None;
                while !done.load(Ordering::SeqCst) {
                    // Re-sent periodically: a request that races ahead of the statement
                    // reaching the server would otherwise be ignored.
                    if cancelled()
                        && last_request
                            .is_none_or(|sent| sent.elapsed() >= Duration::from_millis(100))
                    {
                        let _ = token.cancel_query(tls.clone());
                        last_request = Some(Instant::now());
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
            });
            let result = operation(self);
            done.store(true, Ordering::SeqCst);
            result
        });
        match result {
            Err(AdapterError::QueryFailed | AdapterError::TransientFailure) if cancelled() => {
                Err(AdapterError::Cancelled)
            }
            other => other,
        }
    }

    pub fn drop_relation(
        &mut self,
        relation: &str,
        allow_drop: bool,
    ) -> Result<bool, AdapterError> {
        let quoted = quote_identifier(relation)?;
        let mut transaction = self
            .client
            .transaction()
            .map_err(|_| AdapterError::QueryFailed)?;
        let existing = transaction
            .query_opt(
                "SELECT c.relkind::text FROM pg_class c WHERE c.oid = to_regclass($1)",
                &[&quoted],
            )
            .map_err(|_| AdapterError::QueryFailed)?
            .map(|row| row.get::<_, String>(0));
        let Some(kind) = existing else {
            transaction
                .commit()
                .map_err(|_| AdapterError::QueryFailed)?;
            return Ok(false);
        };
        ensure_destructive_approval(true, allow_drop)?;
        let drop_kind = match kind.as_str() {
            "r" => "TABLE",
            "v" => "VIEW",
            _ => return Err(AdapterError::UnsupportedRelationType),
        };
        transaction
            .batch_execute(&format!("DROP {drop_kind} {quoted}"))
            .map_err(|_| AdapterError::QueryFailed)?;
        transaction
            .commit()
            .map_err(|_| AdapterError::QueryFailed)?;
        Ok(true)
    }
}

/// Maps a failure of a materialization statement. Transient kinds are retryable only because
/// the materialization runs in one transaction that rolls back on failure.
fn classify_failure(error: postgres::Error) -> AdapterError {
    if error.is_closed()
        || error
            .code()
            .is_some_and(|state| is_transient_sqlstate(state.code()))
    {
        AdapterError::TransientFailure
    } else {
        AdapterError::QueryFailed
    }
}

/// SQLSTATE codes of transient failures: serialization failure (40001), deadlock (40P01),
/// lock not available (55P03), statement timeout / query canceled (57014) and the connection
/// exception class (08xxx). Other codes are permanent (for example 42P01, 23505).
pub fn is_transient_sqlstate(code: &str) -> bool {
    matches!(code, "40001" | "40P01" | "55P03" | "57014") || code.starts_with("08")
}

fn ensure_destructive_approval(relation_exists: bool, approved: bool) -> Result<(), AdapterError> {
    if relation_exists && !approved {
        return Err(AdapterError::DestructiveApprovalRequired);
    }
    Ok(())
}

fn ensure_single_statement(sql: &str) -> Result<(), AdapterError> {
    let bytes = sql.as_bytes();
    let mut index = 0;
    let mut semicolon_seen = false;
    let mut trailing_content = false;
    while index < bytes.len() {
        match bytes[index] {
            byte if byte.is_ascii_whitespace() => index += 1,
            b'-' if bytes.get(index + 1) == Some(&b'-') => {
                index += 2;
                while index < bytes.len() && bytes[index] != b'\n' {
                    index += 1;
                }
            }
            b'/' if bytes.get(index..index.saturating_add(2)) == Some(&b"/*"[..]) => {
                index += 2;
                let mut depth = 1usize;
                while index < bytes.len() && depth > 0 {
                    if bytes.get(index..index.saturating_add(2)) == Some(&b"/*"[..]) {
                        depth += 1;
                        index += 2;
                    } else if bytes.get(index..index.saturating_add(2)) == Some(&b"*/"[..]) {
                        depth -= 1;
                        index += 2;
                    } else {
                        index += 1;
                    }
                }
                if depth != 0 {
                    return Err(AdapterError::QueryFailed);
                }
            }
            b';' => {
                if semicolon_seen {
                    return Err(AdapterError::MultipleStatements);
                }
                semicolon_seen = true;
                index += 1;
            }
            quote @ (b'\'' | b'"') => {
                if semicolon_seen {
                    trailing_content = true;
                }
                let escapes = quote == b'\'' && is_escape_string_prefix(bytes, index);
                index = skip_quoted(bytes, index, quote, escapes)?;
            }
            b'$' => {
                if let Some(delimiter) = dollar_quote_delimiter(bytes, index) {
                    if semicolon_seen {
                        trailing_content = true;
                    }
                    index += delimiter.len();
                    let Some(end) = find_bytes(&bytes[index..], delimiter) else {
                        return Err(AdapterError::QueryFailed);
                    };
                    index += end + delimiter.len();
                } else {
                    if semicolon_seen {
                        trailing_content = true;
                    }
                    index += 1;
                }
            }
            _ => {
                if semicolon_seen {
                    trailing_content = true;
                }
                index += 1;
            }
        }
    }
    if trailing_content {
        return Err(AdapterError::MultipleStatements);
    }
    Ok(())
}

fn skip_quoted(
    bytes: &[u8],
    mut index: usize,
    quote: u8,
    backslash_escapes: bool,
) -> Result<usize, AdapterError> {
    index += 1;
    while index < bytes.len() {
        if backslash_escapes && bytes[index] == b'\\' {
            index = (index + 2).min(bytes.len());
        } else if bytes[index] == quote {
            if bytes.get(index + 1) == Some(&quote) {
                index += 2;
            } else {
                return Ok(index + 1);
            }
        } else {
            index += 1;
        }
    }
    Err(AdapterError::QueryFailed)
}

fn is_escape_string_prefix(bytes: &[u8], quote_index: usize) -> bool {
    if quote_index == 0 || !matches!(bytes[quote_index - 1], b'E' | b'e') {
        return false;
    }
    quote_index == 1
        || !(bytes[quote_index - 2].is_ascii_alphanumeric()
            || matches!(bytes[quote_index - 2], b'_' | b'$'))
}

fn dollar_quote_delimiter(bytes: &[u8], start: usize) -> Option<&[u8]> {
    if start > 0
        && (bytes[start - 1].is_ascii_alphanumeric() || matches!(bytes[start - 1], b'_' | b'$'))
    {
        return None;
    }
    let end = bytes[start + 1..].iter().position(|byte| *byte == b'$')? + start + 1;
    let tag = &bytes[start + 1..end];
    if !tag.is_empty()
        && (!matches!(tag[0], b'A'..=b'Z' | b'a'..=b'z' | b'_')
            || !tag.iter().all(|byte| {
                matches!(
                    byte,
                    b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_'
                )
            }))
    {
        return None;
    }
    Some(&bytes[start..=end])
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn valid_env_var_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c == '_' || c.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

/// Session options applied at connect time: standard strings and the query timeout, if any.
fn session_options(profile: &PgProfile) -> String {
    let mut options = String::from("-c standard_conforming_strings=on");
    if let Some(query_timeout) = profile.query_timeout {
        options.push_str(&format!(
            " -c statement_timeout={}",
            query_timeout.as_millis()
        ));
    }
    options
}

fn is_local_host(host: &str) -> bool {
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    IpAddr::from_str(host).is_ok_and(|address| address.is_loopback())
}

#[cfg(test)]
mod tests;

/// Types read directly; every other type is cast to text in the SELECT list.
/// Comparison pushed down to PostgreSQL (ENT-041). The set is closed: the SQL operator is
/// chosen here, never taken from the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterOp {
    Eq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
    IsNull,
    IsNotNull,
}

/// Value of a pushed-down comparison. It is bound as a parameter with an explicit SQL type,
/// never concatenated into the statement text.
#[derive(Debug, Clone, PartialEq)]
pub enum FilterValue {
    /// Only valid with [`FilterOp::IsNull`] and [`FilterOp::IsNotNull`].
    None,
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
}

/// One predicate `column op value` of a dataset read.
#[derive(Debug, Clone, PartialEq)]
pub struct SqlFilter {
    pub column: String,
    pub op: FilterOp,
    pub value: FilterValue,
}

/// A batch statement and its parameters, built without a connection.
pub struct RowsStatement {
    sql: String,
    params: Vec<Box<dyn ToSql + Sync>>,
}

impl RowsStatement {
    /// The SQL text, with `$n` placeholders for every bound value.
    pub fn sql(&self) -> &str {
        &self.sql
    }
}

/// Builds the batch statement for [`PostgresAdapter::read_rows_filtered`]. Pure: it only
/// quotes identifiers and orders parameters, so the pushed predicates can be asserted directly.
pub fn rows_statement(
    relation: &str,
    columns: &[(String, String)],
    filters: &[SqlFilter],
    offset: u64,
    limit: u32,
) -> Result<RowsStatement, AdapterError> {
    let mut select = Vec::with_capacity(columns.len());
    for (name, data_type) in columns {
        let identifier = quote_identifier(name)?;
        if DIRECT_TYPES.contains(&data_type.as_str()) {
            select.push(identifier);
        } else {
            select.push(format!("{identifier}::text"));
        }
    }
    if select.is_empty() {
        return Err(AdapterError::QueryFailed);
    }
    let mut params: Vec<Box<dyn ToSql + Sync>> = Vec::new();
    let mut clauses = Vec::with_capacity(filters.len());
    for filter in filters {
        let column = quote_identifier(&filter.column)?;
        let operator = match filter.op {
            FilterOp::Eq => "=",
            FilterOp::NotEq => "<>",
            FilterOp::Lt => "<",
            FilterOp::LtEq => "<=",
            FilterOp::Gt => ">",
            FilterOp::GtEq => ">=",
            FilterOp::IsNull => {
                if filter.value != FilterValue::None {
                    return Err(AdapterError::QueryFailed);
                }
                clauses.push(format!("{column} IS NULL"));
                continue;
            }
            FilterOp::IsNotNull => {
                if filter.value != FilterValue::None {
                    return Err(AdapterError::QueryFailed);
                }
                clauses.push(format!("{column} IS NOT NULL"));
                continue;
            }
        };
        let equality_only = matches!(filter.op, FilterOp::Eq | FilterOp::NotEq);
        let position = params.len() + 1;
        let (cast, param): (&str, Box<dyn ToSql + Sync>) = match &filter.value {
            FilterValue::None => return Err(AdapterError::QueryFailed),
            FilterValue::Bool(value) if equality_only => ("bool", Box::new(*value)),
            FilterValue::Bool(_) => return Err(AdapterError::QueryFailed),
            FilterValue::Int(value) => ("int8", Box::new(*value)),
            FilterValue::Float(value) => ("float8", Box::new(*value)),
            FilterValue::Text(value) => ("text", Box::new(value.clone())),
        };
        params.push(param);
        clauses.push(format!("{column} {operator} ${position}::{cast}"));
    }
    let offset = i64::try_from(offset).map_err(|_| AdapterError::QueryFailed)?;
    params.push(Box::new(offset));
    let offset_position = params.len();
    params.push(Box::new(i64::from(limit)));
    let limit_position = params.len();
    let where_clause = if clauses.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", clauses.join(" AND "))
    };
    let sql = format!(
        "SELECT {} FROM {relation}{where_clause} ORDER BY ctid OFFSET ${offset_position} LIMIT ${limit_position}",
        select.join(", ")
    );
    Ok(RowsStatement { sql, params })
}

const DIRECT_TYPES: &[&str] = &[
    "bool", "int2", "int4", "int8", "float4", "float8", "text", "varchar", "bpchar", "name",
];

/// One decoded value of a dataset batch.
#[derive(Debug, Clone, PartialEq)]
pub enum DatasetCell {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
}

fn decode_cell(row: &Row, index: usize, data_type: &str) -> Result<DatasetCell, AdapterError> {
    let decoded = if !DIRECT_TYPES.contains(&data_type) {
        row.try_get::<_, Option<String>>(index)
            .map(|value| value.map_or(DatasetCell::Null, DatasetCell::Text))
    } else {
        match data_type {
            "bool" => row
                .try_get::<_, Option<bool>>(index)
                .map(|value| value.map_or(DatasetCell::Null, DatasetCell::Bool)),
            "int2" => row
                .try_get::<_, Option<i16>>(index)
                .map(|value| value.map_or(DatasetCell::Null, |v| DatasetCell::Int(i64::from(v)))),
            "int4" => row
                .try_get::<_, Option<i32>>(index)
                .map(|value| value.map_or(DatasetCell::Null, |v| DatasetCell::Int(i64::from(v)))),
            "int8" => row
                .try_get::<_, Option<i64>>(index)
                .map(|value| value.map_or(DatasetCell::Null, DatasetCell::Int)),
            "float4" => row
                .try_get::<_, Option<f32>>(index)
                .map(|value| value.map_or(DatasetCell::Null, |v| DatasetCell::Float(f64::from(v)))),
            "float8" => row
                .try_get::<_, Option<f64>>(index)
                .map(|value| value.map_or(DatasetCell::Null, DatasetCell::Float)),
            _ => row
                .try_get::<_, Option<String>>(index)
                .map(|value| value.map_or(DatasetCell::Null, DatasetCell::Text)),
        }
    };
    decoded.map_err(|_| AdapterError::QueryFailed)
}

#[cfg(test)]
mod rows_statement_tests {
    use super::{rows_statement, FilterOp, FilterValue, SqlFilter};

    fn columns() -> Vec<(String, String)> {
        vec![
            ("id".to_owned(), "int4".to_owned()),
            ("label".to_owned(), "text".to_owned()),
            ("amount".to_owned(), "numeric".to_owned()),
        ]
    }

    fn filter(column: &str, op: FilterOp, value: FilterValue) -> SqlFilter {
        SqlFilter {
            column: column.to_owned(),
            op,
            value,
        }
    }

    #[test]
    fn pushed_predicate_appears_in_where_with_typed_parameter() {
        let statement = rows_statement(
            "\"public\".\"t\"",
            &columns(),
            &[filter("id", FilterOp::Gt, FilterValue::Int(10))],
            0,
            100,
        )
        .expect("statement");
        assert_eq!(
            statement.sql(),
            "SELECT \"id\", \"label\", \"amount\"::text FROM \"public\".\"t\" WHERE \"id\" > $1::int8 ORDER BY ctid OFFSET $2 LIMIT $3"
        );
    }

    #[test]
    fn predicates_are_combined_with_and_and_null_tests_bind_nothing() {
        let statement = rows_statement(
            "\"t\"",
            &columns(),
            &[
                filter("label", FilterOp::Eq, FilterValue::Text("x".to_owned())),
                filter("amount", FilterOp::IsNotNull, FilterValue::None),
                filter("id", FilterOp::LtEq, FilterValue::Float(2.5)),
            ],
            5,
            10,
        )
        .expect("statement");
        assert!(statement.sql().contains(
            " WHERE \"label\" = $1::text AND \"amount\" IS NOT NULL AND \"id\" <= $2::float8 "
        ));
        assert!(statement.sql().ends_with("OFFSET $3 LIMIT $4"));
    }

    #[test]
    fn identifiers_are_quoted_and_values_never_reach_the_text() {
        let statement = rows_statement(
            "\"t\"",
            &columns(),
            &[filter(
                "label",
                FilterOp::Eq,
                FilterValue::Text("x'; DROP TABLE t; --".to_owned()),
            )],
            0,
            1,
        )
        .expect("statement");
        assert!(!statement.sql().contains("DROP"));
        assert!(!statement.sql().contains('\''));
    }

    #[test]
    fn invalid_combinations_are_refused_before_any_connection() {
        let ordered_bool = rows_statement(
            "\"t\"",
            &columns(),
            &[filter("id", FilterOp::Lt, FilterValue::Bool(true))],
            0,
            1,
        );
        assert!(ordered_bool.is_err());
        let null_with_value = rows_statement(
            "\"t\"",
            &columns(),
            &[filter("id", FilterOp::IsNull, FilterValue::Int(1))],
            0,
            1,
        );
        assert!(null_with_value.is_err());
        let hostile_name = rows_statement(
            "\"t\"",
            &columns(),
            &[filter("id\"; --", FilterOp::Eq, FilterValue::Int(1))],
            0,
            1,
        );
        // The name is quoted as an identifier (embedded quote doubled), never spliced raw.
        let statement = hostile_name.expect("quoted identifier");
        assert!(statement.sql().contains("WHERE \"id\"\"; --\" = $1::int8"));
    }
}
